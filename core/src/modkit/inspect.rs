//! Reading and writing AVM2 object properties, and picking objects on stage.

use crate::avm2::property::Property;
use crate::avm2::{Activation, Class, Error, Object, TObject as _, Value};
use crate::context::UpdateContext;
use crate::debug_ui::handle::AVM2ObjectHandle;
use crate::display_object::{
    DisplayObject, HitTestOptions, TDisplayObject, TDisplayObjectContainer,
};
use crate::modkit::model::{
    Access, ObjectEntry, PropEntry, PropKind, PropValue, Snapshot, SnapshotLimits,
};
use crate::string::AvmString;
use gc_arena::DynamicRootSet;
use std::collections::{HashMap, HashSet, VecDeque};
use swf::{Point, Twips};

/// Max dynamic (non-declared) properties listed per object.
const MAX_DYNAMIC_PROPS: usize = 200;

/// Gives objects small stable ids for the UI and LLM patches.
///
/// Holding a handle keeps the object alive, so ids never dangle.
#[derive(Default)]
pub struct Registry {
    handles: Vec<AVM2ObjectHandle>,
    by_ptr: HashMap<usize, u32>,
}

impl Registry {
    pub fn id_for<'gc>(&mut self, context: &mut UpdateContext<'gc>, object: Object<'gc>) -> u32 {
        let ptr = object.as_ptr() as usize;
        if let Some(id) = self.by_ptr.get(&ptr) {
            return *id;
        }
        self.handles.push(AVM2ObjectHandle::new(context, object));
        let id = self.handles.len() as u32;
        self.by_ptr.insert(ptr, id);
        id
    }

    pub fn fetch<'gc>(&self, dynamic_root: DynamicRootSet<'gc>, id: u32) -> Option<Object<'gc>> {
        let index = (id as usize).checked_sub(1)?;
        self.handles.get(index).map(|h| h.fetch(dynamic_root))
    }
}

fn error_string(error: Error<'_>) -> String {
    format!("{error:?}")
}

/// Classes that come from Flash Player itself rather than the game.
pub fn is_builtin_class<'gc>(activation: &mut Activation<'_, 'gc>, class: Class<'gc>) -> bool {
    class
        .translation_unit()
        .map(|unit| unit.domain().is_playerglobals_domain(activation.avm2()))
        .unwrap_or(true)
}

/// The class that defines `object`'s behaviour: for a class object, the class
/// itself; otherwise the instance class.
fn defining_class<'gc>(object: Object<'gc>) -> Class<'gc> {
    match object.as_class_object() {
        Some(class) => class.inner_class_definition(),
        None => object.instance_class(),
    }
}

pub fn class_label<'gc>(activation: &mut Activation<'_, 'gc>, object: Object<'gc>) -> String {
    let name = defining_class(object)
        .name()
        .to_qualified_name(activation.gc())
        .to_string();
    if object.as_class_object().is_some() {
        format!("{name} (static)")
    } else {
        name
    }
}

/// Dotted class name as FFDec expects it (`com.foo::Bar` -> `com.foo.Bar`),
/// using the nearest game class if `object` is a Flash Player class.
pub fn source_class_name<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
) -> Option<String> {
    let mut class = Some(defining_class(object));
    while let Some(c) = class {
        if !is_builtin_class(activation, c) {
            let name = c.name().to_qualified_name(activation.gc()).to_string();
            return Some(name.replace("::", "."));
        }
        class = c.super_class();
    }
    None
}

/// URL of the SWF that defines the object's nearest game class. Classes can
/// live in a loaded child movie, not only in the root SWF.
pub fn source_swf_url<'gc>(activation: &mut Activation<'_, 'gc>, object: Object<'gc>) -> String {
    let mut class = Some(defining_class(object));
    while let Some(c) = class {
        if !is_builtin_class(activation, c) {
            if let Some(unit) = c.translation_unit() {
                return unit.movie().url().to_string();
            }
            break;
        }
        class = c.super_class();
    }
    activation.context.root_swf.url().to_string()
}

pub fn instance_name<'gc>(object: Object<'gc>) -> Option<String> {
    let name = object.as_display_object()?.name()?;
    (!name.is_empty()).then(|| name.to_string())
}

fn kind_of_class(class: Class<'_>) -> Option<PropKind> {
    if class.is_builtin_int() {
        Some(PropKind::Int)
    } else if class.is_builtin_uint() {
        Some(PropKind::Uint)
    } else if class.is_builtin_number() {
        Some(PropKind::Number)
    } else if class.is_builtin_boolean() {
        Some(PropKind::Boolean)
    } else if class.is_builtin_string() {
        Some(PropKind::String)
    } else {
        None
    }
}

/// For untyped (`*` / `Object`) storage, the kind follows the current value.
/// Assigning a value of the same kind can never fail coercion.
fn kind_of_value(value: Value<'_>) -> Option<PropKind> {
    match value {
        Value::Integer(_) | Value::Number(_) => Some(PropKind::Number),
        Value::Bool(_) => Some(PropKind::Boolean),
        Value::String(_) => Some(PropKind::String),
        _ => None,
    }
}

fn is_untyped(class: Option<Class<'_>>) -> bool {
    class.is_none_or(|c| c.is_builtin_object())
}

fn to_prop_value(kind: PropKind, value: Value<'_>) -> Option<PropValue> {
    Some(match (kind, value) {
        (PropKind::Int, v @ (Value::Integer(_) | Value::Number(_))) => PropValue::Int(v.as_i32()),
        (PropKind::Uint, v @ (Value::Integer(_) | Value::Number(_))) => PropValue::Uint(v.as_u32()),
        (PropKind::Number, v @ (Value::Integer(_) | Value::Number(_))) => {
            PropValue::Number(v.as_f64())
        }
        (PropKind::Boolean, Value::Bool(b)) => PropValue::Boolean(b),
        (PropKind::String, Value::String(s)) => PropValue::String(s.to_string()),
        _ => return None,
    })
}

pub fn to_avm_value<'gc>(activation: &mut Activation<'_, 'gc>, value: &PropValue) -> Value<'gc> {
    match value {
        PropValue::Int(i) => Value::Integer(*i),
        PropValue::Uint(u) => Value::Number(*u as f64),
        PropValue::Number(n) => Value::Number(*n),
        PropValue::Boolean(b) => Value::Bool(*b),
        PropValue::String(s) => AvmString::new_utf8(activation.gc(), s).into(),
    }
}

fn display_value<'gc>(activation: &mut Activation<'_, 'gc>, value: Value<'gc>) -> String {
    match value {
        Value::Undefined => "undefined".into(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("{:?}", s.to_string()),
        Value::Object(o) => {
            if o.as_function_object().is_some() {
                "function".into()
            } else if let Some(array) = o.as_array_storage() {
                format!("Array({})", array.length())
            } else {
                class_label(activation, o)
            }
        }
    }
}

/// Whether an object-valued property is worth following into: game objects,
/// not Flash Player internals, functions or classes.
fn is_followable<'gc>(activation: &mut Activation<'_, 'gc>, object: Object<'gc>) -> bool {
    object.as_function_object().is_none()
        && object.as_class_object().is_none()
        && !is_builtin_class(activation, object.instance_class())
}

pub struct ReadProp<'gc> {
    pub entry: PropEntry,
    /// The object this property points to, if it is a game object.
    pub target: Option<Object<'gc>>,
}

/// Lists the properties of `object`: declared variables, built-in accessors
/// with a setter (e.g. `x`, `alpha`) and dynamic properties.
///
/// Game-defined getters are listed but never called, since they can have
/// side effects. Without `include_builtin` the built-in accessors are left
/// out and their getters are not called.
pub fn read_props<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    include_builtin: bool,
) -> Vec<ReadProp<'gc>> {
    let vtable = object.vtable();
    let traits: Vec<_> = vtable
        .resolved_traits()
        .iter()
        .filter(|(_, _, prop)| {
            matches!(
                prop,
                Property::Slot { .. }
                    | Property::ConstSlot { .. }
                    | Property::Virtual { get: Some(_), .. }
            )
        })
        .map(|(name, ns, prop)| (name.to_string(), ns.is_public(), *prop))
        .collect();

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    // Public names first so a private name clash never hides a public property.
    let (public, other): (Vec<_>, Vec<_>) = traits.into_iter().partition(|t| t.1);
    for (name, _, prop) in public.into_iter().chain(other) {
        if !seen.insert(name.clone()) {
            continue;
        }
        let read = match prop {
            Property::Slot { slot_id } | Property::ConstSlot { slot_id } => {
                let value = object.get_slot(slot_id);
                let class = vtable
                    .slot_class(slot_id)
                    .and_then(|mut c| c.get_class(activation).ok())
                    .flatten();
                let kind = match class {
                    c if is_untyped(c) => kind_of_value(value),
                    Some(c) => kind_of_class(c),
                    None => None,
                };
                let access = if matches!(prop, Property::ConstSlot { .. }) {
                    Access::ConstSlot(slot_id)
                } else {
                    Access::Slot(slot_id)
                };
                make_entry(activation, name, kind, value, access, false)
            }
            Property::Virtual {
                get: Some(get),
                set,
            } => {
                let Some(getter) = vtable.get_method(get) else {
                    continue;
                };
                let builtin = getter
                    .translation_unit()
                    .domain()
                    .is_playerglobals_domain(activation.avm2());
                if !builtin {
                    ReadProp {
                        entry: PropEntry {
                            name,
                            kind: None,
                            value: None,
                            display: "(getter, not called)".into(),
                            access: Access::Accessor { get, set },
                            builtin: false,
                            object_ref: None,
                        },
                        target: None,
                    }
                } else {
                    // Only built-in accessors that can be written are useful here.
                    let Some(set) = set else { continue };
                    if !include_builtin {
                        continue;
                    }
                    let Ok(value) = Value::from(object).call_method(get, &[], activation) else {
                        continue;
                    };
                    let class = setter_param_class(activation, object, set);
                    let kind = match class {
                        c if is_untyped(c) => kind_of_value(value),
                        Some(c) => kind_of_class(c),
                        None => None,
                    };
                    let access = Access::Accessor {
                        get,
                        set: Some(set),
                    };
                    make_entry(activation, name, kind, value, access, true)
                }
            }
            _ => continue,
        };
        out.push(read);
    }
    out.sort_by(|a, b| (a.entry.builtin, &a.entry.name).cmp(&(b.entry.builtin, &b.entry.name)));

    out.extend(read_dynamic_props(activation, object));
    out
}

fn setter_param_class<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    set: usize,
) -> Option<Class<'gc>> {
    let method = object.vtable().get_method(set)?;
    method.resolve_info(activation).ok()?;
    method.resolved_param_config().first()?.param_type
}

fn make_entry<'gc>(
    activation: &mut Activation<'_, 'gc>,
    name: String,
    kind: Option<PropKind>,
    value: Value<'gc>,
    access: Access,
    builtin: bool,
) -> ReadProp<'gc> {
    let prop_value = kind.and_then(|k| to_prop_value(k, value));
    let target = value.as_object().filter(|o| is_followable(activation, *o));
    ReadProp {
        entry: PropEntry {
            name,
            kind,
            display: display_value(activation, value),
            value: prop_value,
            access,
            builtin,
            object_ref: None,
        },
        target,
    }
}

fn read_dynamic_props<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
) -> Vec<ReadProp<'gc>> {
    let mut out = Vec::new();
    // Enumerating a Proxy runs game code, so it is never done.
    if object.as_array_storage().is_some()
        || object.as_function_object().is_some()
        || matches!(object, Object::ProxyObject(_))
    {
        return out;
    }
    let mut index = 0;
    // Bounded by iterations: keys that are not strings are skipped.
    for _ in 0..MAX_DYNAMIC_PROPS * 4 {
        if out.len() >= MAX_DYNAMIC_PROPS {
            break;
        }
        index = match object.get_next_enumerant(index, activation) {
            Ok(0) | Err(_) => break,
            Ok(next) => next,
        };
        let Ok(Value::String(name)) = object.get_enumerant_name(index, activation) else {
            continue;
        };
        let Ok(value) = Value::from(object).get_public_property(name, activation) else {
            continue;
        };
        let name = name.to_string();
        let kind = kind_of_value(value);
        let access = Access::Dynamic(name.clone());
        out.push(make_entry(activation, name, kind, value, access, false));
    }
    out
}

pub fn read_prop<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    access: &Access,
    kind: PropKind,
) -> Result<PropValue, String> {
    let value = match access {
        Access::Slot(slot_id) | Access::ConstSlot(slot_id) => object.get_slot(*slot_id),
        Access::Accessor { get, .. } => Value::from(object)
            .call_method(*get, &[], activation)
            .map_err(error_string)?,
        Access::Dynamic(name) => {
            let name = AvmString::new_utf8(activation.gc(), name);
            Value::from(object)
                .get_public_property(name, activation)
                .map_err(error_string)?
        }
    };
    to_prop_value(kind, value).ok_or_else(|| format!("value is no longer a {}", kind.name()))
}

pub fn write_prop<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    access: &Access,
    value: &PropValue,
) -> Result<(), String> {
    let avm_value = to_avm_value(activation, value);
    match access {
        Access::Slot(slot_id) => object
            .set_slot(*slot_id, avm_value, activation)
            .map_err(error_string),
        Access::Accessor { set: Some(set), .. } => Value::from(object)
            .call_method(*set, &[avm_value], activation)
            .map(|_| ())
            .map_err(error_string),
        Access::ConstSlot(_) | Access::Accessor { set: None, .. } => {
            Err("property is read-only".into())
        }
        Access::Dynamic(name) => {
            let name = AvmString::new_utf8(activation.gc(), name);
            Value::from(object)
                .set_public_property(name, avm_value, activation)
                .map_err(error_string)
        }
    }
}

/// A declared property that a mod rule can write, with the value converted.
pub struct Writable {
    pub access: Access,
    pub kind: PropKind,
    pub value: PropValue,
    /// Reading the property is free of side effects, so the rule can skip the
    /// write when the value is already right.
    pub comparable: bool,
}

/// The kind a JSON value would have for storage that has no declared type.
fn kind_of_json(json: &serde_json::Value) -> Option<PropKind> {
    match json {
        serde_json::Value::Number(_) => Some(PropKind::Number),
        serde_json::Value::Bool(_) => Some(PropKind::Boolean),
        serde_json::Value::String(_) => Some(PropKind::String),
        _ => None,
    }
}

/// Finds the declared property `name` of `object` (a slot, or an accessor
/// with a setter) and converts `json` for it with the same strict checks as
/// the object editor. Nothing is called or written.
///
/// Dynamic properties are not supported: they exist per instance, so a rule
/// for a class cannot rely on them.
pub fn resolve_writable<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    name: &str,
    json: &serde_json::Value,
) -> Result<Writable, String> {
    let vtable = object.vtable();
    let found = {
        let mut public = None;
        let mut other = None;
        for (trait_name, ns, prop) in vtable.resolved_traits().iter() {
            if trait_name.to_string() == name {
                let slot = if ns.is_public() {
                    &mut public
                } else {
                    &mut other
                };
                slot.get_or_insert(*prop);
            }
        }
        public.or(other)
    };
    let Some(prop) = found else {
        return Err(
            "no such declared property (only declared variables and setters can be set)".into(),
        );
    };

    let (access, class, current, comparable) = match prop {
        Property::Slot { slot_id } => {
            let class = vtable
                .slot_class(slot_id)
                .and_then(|mut c| c.get_class(activation).ok())
                .flatten();
            (
                Access::Slot(slot_id),
                class,
                Some(object.get_slot(slot_id)),
                true,
            )
        }
        Property::ConstSlot { .. } => return Err("property is a constant".into()),
        Property::Virtual { get, set } => {
            let Some(set) = set else {
                return Err("property has no setter".into());
            };
            let builtin_getter = get
                .and_then(|get| vtable.get_method(get))
                .is_some_and(|getter| {
                    getter
                        .translation_unit()
                        .domain()
                        .is_playerglobals_domain(activation.avm2())
                });
            let class = setter_param_class(activation, object, set);
            (
                Access::Accessor {
                    get: get.unwrap_or(0),
                    set: Some(set),
                },
                class,
                None,
                // A game getter may have side effects, so it is never called
                // just to compare.
                builtin_getter && get.is_some(),
            )
        }
        _ => return Err("property cannot be set".into()),
    };

    let kind = if is_untyped(class) {
        current
            .and_then(kind_of_value)
            .or_else(|| kind_of_json(json))
    } else {
        class.and_then(kind_of_class)
    }
    .ok_or("property type is not a number, string or boolean")?;
    let value = PropValue::from_json(kind, json)?;
    Ok(Writable {
        access,
        kind,
        value,
        comparable,
    })
}

/// Snapshots `roots` and, up to `limits.max_depth` levels, the game objects
/// they reference. Ids come from `registry`, so they stay stable.
pub fn build_snapshot<'gc>(
    activation: &mut Activation<'_, 'gc>,
    roots: &[Object<'gc>],
    limits: &SnapshotLimits,
    registry: &mut Registry,
) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let mut queued = HashSet::new();
    let mut queue = VecDeque::new();
    for root in roots {
        if queued.insert(root.as_ptr() as usize) {
            queue.push_back((*root, 0));
        }
    }
    while let Some((object, depth)) = queue.pop_front() {
        if snapshot.objects.len() >= limits.max_objects {
            snapshot.truncated = true;
            break;
        }
        let id = registry.id_for(activation.context, object);
        let mut props = Vec::new();
        for ReadProp { mut entry, target } in read_props(activation, object, true) {
            if let Some(target) = target
                && depth < limits.max_depth
            {
                entry.object_ref = Some(registry.id_for(activation.context, target));
                if queued.insert(target.as_ptr() as usize) {
                    queue.push_back((target, depth + 1));
                }
            }
            props.push(entry);
        }
        snapshot.objects.push(ObjectEntry {
            id,
            class_name: class_label(activation, object),
            instance_name: instance_name(object),
            props,
        });
    }
    // Refs to objects cut by `max_objects` would dangle.
    let present: HashSet<u32> = snapshot.objects.iter().map(|o| o.id).collect();
    for object in &mut snapshot.objects {
        for prop in &mut object.props {
            if prop.object_ref.is_some_and(|id| !present.contains(&id)) {
                prop.object_ref = None;
            }
        }
    }
    snapshot
}

/// Converts a pointer position in window pixels to stage coordinates.
pub fn pointer_to_stage(
    context: &UpdateContext<'_>,
    egui_ctx: &egui::Context,
    movie_offset: f64,
) -> Option<Point<Twips>> {
    let pointer = egui_ctx.pointer_latest_pos()?;
    let ppp = egui_ctx.pixels_per_point() as f64;
    let x = pointer.x as f64 * ppp;
    let y = pointer.y as f64 * ppp - movie_offset;
    Some(context.stage.inverse_view_matrix() * Point::from_pixels(x, y))
}

/// The deepest visible display object under `point`.
fn deepest_hit<'gc>(
    context: &mut UpdateContext<'gc>,
    object: DisplayObject<'gc>,
    point: Point<Twips>,
) -> Option<DisplayObject<'gc>> {
    if !object.visible() {
        return None;
    }
    if let Some(container) = object.as_container() {
        for child in container.iter_render_list().rev() {
            if let Some(hit) = deepest_hit(context, child, point) {
                return Some(hit);
            }
        }
    }
    object
        .hit_test_shape(context, point, HitTestOptions::MOUSE_PICK)
        .then_some(object)
}

/// Picks the game object under `point`: the deepest display object hit,
/// then its nearest ancestor whose class comes from the game. Returns the
/// object and the display object it belongs to.
pub fn pick<'gc>(
    activation: &mut Activation<'_, 'gc>,
    point: Point<Twips>,
) -> Option<(Object<'gc>, DisplayObject<'gc>)> {
    let stage = activation.context.stage;
    let mut hit = None;
    for child in stage.iter_render_list().rev() {
        hit = deepest_hit(activation.context, child, point);
        if hit.is_some() {
            break;
        }
    }
    let mut current = hit;
    let mut fallback = None;
    while let Some(display_object) = current {
        if let Some(object) = display_object.object2() {
            let object: Object<'gc> = object.into();
            if !is_builtin_class(activation, object.instance_class()) {
                return Some((object, display_object));
            }
            fallback.get_or_insert((object, display_object));
        }
        current = display_object.parent();
    }
    fallback
}

/// Finds a game class by its qualified name (`Stats` or `com.foo.Bar` or
/// `com.foo::Bar`) and returns its class object, for editing static vars.
pub fn find_class_object<'gc>(
    activation: &mut Activation<'_, 'gc>,
    name: &str,
) -> Option<Object<'gc>> {
    let wanted = name.trim().replace("::", ".");
    let domain = activation.context.avm2.stage_domain();
    let classes: Vec<Class<'gc>> = domain.classes().iter().map(|(_, _, c)| *c).collect();
    classes.into_iter().find_map(|class| {
        let qualified = class
            .name()
            .to_qualified_name(activation.gc())
            .to_string()
            .replace("::", ".");
        (qualified == wanted || class.name().local_name().to_string() == wanted)
            .then(|| class.class_object())
            .flatten()
            .map(Object::from)
    })
}

/// Lists game class names in the main movie's domain, for autocompletion.
pub fn game_class_names<'gc>(activation: &mut Activation<'_, 'gc>) -> Vec<String> {
    let domain = activation.context.avm2.stage_domain();
    let classes: Vec<Class<'gc>> = domain.classes().iter().map(|(_, _, c)| *c).collect();
    let mut names = Vec::new();
    for class in classes {
        if !is_builtin_class(activation, class) {
            let name = class.name().to_qualified_name(activation.gc()).to_string();
            names.push(name.replace("::", "."));
        }
    }
    names.sort();
    names
}
