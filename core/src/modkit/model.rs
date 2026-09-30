//! GC-free model of inspected objects.
//!
//! Everything here is plain data so it can be unit tested without a player:
//! typed property values, the JSON snapshot that is sent to an LLM, and the
//! strict parser/validator for the JSON patch that comes back.

use serde_json::{Map, Value as Json};
use std::collections::HashSet;
use std::fmt::Write as _;

/// The property types that can be edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropKind {
    Int,
    Uint,
    Number,
    Boolean,
    String,
}

impl PropKind {
    pub fn name(self) -> &'static str {
        match self {
            PropKind::Int => "int",
            PropKind::Uint => "uint",
            PropKind::Number => "Number",
            PropKind::Boolean => "Boolean",
            PropKind::String => "String",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    Int(i32),
    Uint(u32),
    Number(f64),
    Boolean(bool),
    String(String),
}

impl PropValue {
    pub fn kind(&self) -> PropKind {
        match self {
            PropValue::Int(_) => PropKind::Int,
            PropValue::Uint(_) => PropKind::Uint,
            PropValue::Number(_) => PropKind::Number,
            PropValue::Boolean(_) => PropKind::Boolean,
            PropValue::String(_) => PropKind::String,
        }
    }

    /// JSON has no NaN or infinity, so those are written as strings.
    /// `from_json` never accepts them back.
    pub fn to_json(&self, max_string_len: usize) -> Json {
        match self {
            PropValue::Int(v) => Json::from(*v),
            PropValue::Uint(v) => Json::from(*v),
            PropValue::Number(v) => serde_json::Number::from_f64(*v)
                .map(Json::Number)
                .unwrap_or_else(|| Json::String(format_number(*v))),
            PropValue::Boolean(v) => Json::Bool(*v),
            PropValue::String(v) => Json::String(truncate_chars(v, max_string_len)),
        }
    }

    /// Converts a JSON value to `kind` without any lossy coercion.
    ///
    /// Integers may be written as `5` or `5.0`, but never `5.5` or `"5"`.
    pub fn from_json(kind: PropKind, json: &Json) -> Result<PropValue, String> {
        let type_error = || format!("expected {}, got {}", kind.name(), json_type_name(json));
        match kind {
            PropKind::Int => json_integer(json)
                .and_then(|i| i32::try_from(i).ok())
                .map(PropValue::Int)
                .ok_or_else(|| {
                    if json.is_number() {
                        format!("{json} is not an int (-2147483648..2147483647, no fraction)")
                    } else {
                        type_error()
                    }
                }),
            PropKind::Uint => json_integer(json)
                .and_then(|i| u32::try_from(i).ok())
                .map(PropValue::Uint)
                .ok_or_else(|| {
                    if json.is_number() {
                        format!("{json} is not a uint (0..4294967295, no fraction)")
                    } else {
                        type_error()
                    }
                }),
            PropKind::Number => json.as_f64().map(PropValue::Number).ok_or_else(type_error),
            PropKind::Boolean => json
                .as_bool()
                .map(PropValue::Boolean)
                .ok_or_else(type_error),
            PropKind::String => json
                .as_str()
                .map(|s| PropValue::String(s.to_string()))
                .ok_or_else(type_error),
        }
    }

    pub fn display(&self) -> String {
        match self {
            PropValue::Int(v) => v.to_string(),
            PropValue::Uint(v) => v.to_string(),
            PropValue::Number(v) => format_number(*v),
            PropValue::Boolean(v) => v.to_string(),
            PropValue::String(v) => format!("{v:?}"),
        }
    }
}

fn json_integer(json: &Json) -> Option<i64> {
    if let Some(i) = json.as_i64() {
        return Some(i);
    }
    let f = json.as_f64()?;
    // 2^63 is exactly representable; anything at or beyond it can't be an i64.
    if f.fract() == 0.0 && f.abs() < 9_223_372_036_854_775_808.0 {
        Some(f as i64)
    } else {
        None
    }
}

fn json_type_name(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

fn format_number(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        v.to_string()
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// How a property is read and written. Plain ids, so it holds no GC pointers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Slot(usize),
    /// A `const` slot: readable, never written.
    ConstSlot(usize),
    Accessor {
        get: usize,
        set: Option<usize>,
    },
    Dynamic(String),
}

#[derive(Debug, Clone)]
pub struct PropEntry {
    pub name: String,
    /// `None` if the property can't be edited (object values, `const`,
    /// user getters that aren't called, ...).
    pub kind: Option<PropKind>,
    pub value: Option<PropValue>,
    /// Human-readable value, also for non-editable properties.
    pub display: String,
    pub access: Access,
    /// Declared by a Flash Player class (e.g. `x`, `alpha`), not the game.
    pub builtin: bool,
    /// Id of the object this property points to, if it is in the snapshot.
    pub object_ref: Option<u32>,
}

impl PropEntry {
    pub fn is_editable(&self) -> bool {
        self.kind.is_some() && self.value.is_some() && self.access_is_writable()
    }

    fn access_is_writable(&self) -> bool {
        !matches!(
            self.access,
            Access::ConstSlot(_) | Access::Accessor { set: None, .. }
        )
    }
}

#[derive(Debug, Clone)]
pub struct ObjectEntry {
    pub id: u32,
    pub class_name: String,
    pub instance_name: Option<String>,
    pub props: Vec<PropEntry>,
}

impl ObjectEntry {
    pub fn label(&self) -> String {
        match &self.instance_name {
            Some(name) => format!("#{} {} \"{}\"", self.id, self.class_name, name),
            None => format!("#{} {}", self.id, self.class_name),
        }
    }

    pub fn prop(&self, name: &str) -> Option<&PropEntry> {
        self.props.iter().find(|p| p.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    /// How many levels of object-valued properties to follow.
    pub max_depth: u32,
    pub max_objects: usize,
    pub max_props_per_object: usize,
    pub max_string_len: usize,
    /// Upper bound for the serialized JSON. Objects that don't fit are left out.
    pub max_json_bytes: usize,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            max_depth: 1,
            max_objects: 40,
            max_props_per_object: 80,
            max_string_len: 200,
            max_json_bytes: 60_000,
        }
    }
}

/// Built-in accessors worth showing to an LLM. Every other Flash Player
/// accessor is noise for game edits and only costs tokens.
pub const LLM_BUILTIN_PROPS: &[&str] = &[
    "x",
    "y",
    "rotation",
    "scaleX",
    "scaleY",
    "alpha",
    "visible",
    "name",
    "text",
    "textColor",
    "mouseEnabled",
    "frameRate",
];

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub objects: Vec<ObjectEntry>,
    /// Some objects or properties were cut by the limits.
    pub truncated: bool,
}

impl Snapshot {
    pub fn object(&self, id: u32) -> Option<&ObjectEntry> {
        self.objects.iter().find(|o| o.id == id)
    }

    /// Serializes the snapshot for an LLM. Only editable properties carry a
    /// type; everything else is marked read-only.
    pub fn to_llm_json(&self, limits: &SnapshotLimits) -> String {
        let mut objects = Vec::new();
        let mut size = 0;
        let mut truncated = self.truncated;
        for object in self.objects.iter().take(limits.max_objects) {
            let json = object_to_json(object, limits, &mut truncated);
            let len = json.to_string().len();
            if size + len > limits.max_json_bytes {
                truncated = true;
                break;
            }
            size += len;
            objects.push(json);
        }
        if self.objects.len() > limits.max_objects {
            truncated = true;
        }
        let mut root = Map::new();
        root.insert("objects".into(), Json::Array(objects));
        if truncated {
            root.insert("truncated".into(), Json::Bool(true));
        }
        serde_json::to_string_pretty(&Json::Object(root)).unwrap_or_default()
    }
}

fn object_to_json(object: &ObjectEntry, limits: &SnapshotLimits, truncated: &mut bool) -> Json {
    let mut props = Map::new();
    let mut refs = Map::new();
    let mut count = 0;
    for prop in &object.props {
        if prop.builtin && !LLM_BUILTIN_PROPS.contains(&prop.name.as_str()) {
            continue;
        }
        if count >= limits.max_props_per_object {
            *truncated = true;
            break;
        }
        count += 1;
        if let Some(id) = prop.object_ref {
            refs.insert(prop.name.clone(), Json::from(id));
            continue;
        }
        let mut entry = Map::new();
        match (prop.is_editable(), prop.kind, &prop.value) {
            (true, Some(kind), Some(value)) => {
                entry.insert("type".into(), Json::from(kind.name()));
                entry.insert("value".into(), value.to_json(limits.max_string_len));
            }
            _ => {
                entry.insert("readonly".into(), Json::Bool(true));
                entry.insert(
                    "value".into(),
                    Json::String(truncate_chars(&prop.display, limits.max_string_len)),
                );
            }
        }
        props.insert(prop.name.clone(), Json::Object(entry));
    }
    let mut json = Map::new();
    json.insert("id".into(), Json::from(object.id));
    json.insert("class".into(), Json::from(object.class_name.clone()));
    if let Some(name) = &object.instance_name {
        json.insert("name".into(), Json::from(name.clone()));
    }
    json.insert("props".into(), Json::Object(props));
    if !refs.is_empty() {
        json.insert("refs".into(), Json::Object(refs));
    }
    Json::Object(json)
}

/// One validated change from a patch.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub id: u32,
    pub prop: String,
    pub old: PropValue,
    pub new: PropValue,
}

pub const MAX_PATCH_CHANGES: usize = 500;

/// Parses and validates an LLM patch of the form
/// `[{"id": <id>, "set": {"prop": value, ...}}, ...]`.
///
/// The patch is all-or-nothing: any unknown object, unknown or read-only
/// property, type mismatch, duplicate or extra key rejects the whole patch
/// and every problem found is returned.
pub fn parse_patch(text: &str, snapshot: &Snapshot) -> Result<Vec<Change>, Vec<String>> {
    let json_text = extract_json_array(text).ok_or_else(|| {
        vec!["response contains no JSON array (expected [{\"id\":..,\"set\":{..}}])".to_string()]
    })?;
    let json: Json =
        serde_json::from_str(json_text).map_err(|e| vec![format!("invalid JSON: {e}")])?;
    let Json::Array(entries) = json else {
        return Err(vec!["patch must be a JSON array".to_string()]);
    };

    let mut errors = Vec::new();
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let Json::Object(entry) = entry else {
            errors.push(format!("entry {index}: must be an object"));
            continue;
        };
        for key in entry.keys() {
            if key != "id" && key != "set" {
                errors.push(format!("entry {index}: unknown key {key:?}"));
            }
        }
        let Some(id) = entry.get("id").and_then(Json::as_u64) else {
            errors.push(format!("entry {index}: missing or invalid \"id\""));
            continue;
        };
        let Some(object) = u32::try_from(id).ok().and_then(|id| snapshot.object(id)) else {
            errors.push(format!("entry {index}: no object with id {id}"));
            continue;
        };
        let Some(Json::Object(set)) = entry.get("set") else {
            errors.push(format!("entry {index}: missing or invalid \"set\" object"));
            continue;
        };
        if set.is_empty() {
            errors.push(format!("entry {index}: \"set\" is empty"));
        }
        for (name, value) in set {
            let at = format!("#{} {}", object.id, name);
            let Some(prop) = object.prop(name) else {
                errors.push(format!("{at}: no such property on {}", object.class_name));
                continue;
            };
            let (Some(kind), Some(old), true) = (prop.kind, &prop.value, prop.is_editable()) else {
                errors.push(format!("{at}: property is read-only"));
                continue;
            };
            if !seen.insert((object.id, name.clone())) {
                errors.push(format!("{at}: set more than once"));
                continue;
            }
            match PropValue::from_json(kind, value) {
                Ok(new) => changes.push(Change {
                    id: object.id,
                    prop: name.clone(),
                    old: old.clone(),
                    new,
                }),
                Err(e) => errors.push(format!("{at}: {e}")),
            }
        }
    }
    if changes.len() > MAX_PATCH_CHANGES {
        errors.push(format!(
            "patch has {} changes, the limit is {MAX_PATCH_CHANGES}",
            changes.len()
        ));
    }
    if errors.is_empty() {
        Ok(changes)
    } else {
        Err(errors)
    }
}

/// LLMs like to wrap JSON in prose or code fences. Take the outermost
/// `[...]` and let the JSON parser decide whether it is valid.
fn extract_json_array(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    (end > start).then(|| &text[start..=end])
}

pub const SYSTEM_PROMPT: &str = "You edit a running Flash (ActionScript 3) game.
You get a JSON snapshot of selected game objects: each has an \"id\", a \"class\", and \"props\".
Editable props have a \"type\" (int, uint, Number, Boolean, String) and a \"value\".
Props marked \"readonly\" must not be changed. \"refs\" lists props that point to other objects by id.

Answer with ONLY a JSON array, no prose and no code fences:
[{\"id\": <object id>, \"set\": {\"<prop>\": <new value>, ...}}, ...]

Rules:
- Only use ids and prop names that appear in the snapshot.
- Values must match the prop type exactly: int/uint are whole numbers, Boolean is true/false, String is a JSON string.
- Change as little as needed. If the request can't be done with these props, answer [].";

/// Builds the user message for the LLM.
pub fn build_user_prompt(
    request: &str,
    snapshot_json: &str,
    sources: &[(String, String)],
) -> String {
    let mut prompt = String::new();
    let _ = writeln!(prompt, "Request: {}\n", request.trim());
    let _ = writeln!(prompt, "Snapshot:\n{snapshot_json}");
    for (class_name, source) in sources {
        let _ = writeln!(
            prompt,
            "\nDecompiled source of {class_name} (for context only):\n{source}"
        );
    }
    prompt
}

/// Short text diff of a change list, one line per change.
pub fn describe_changes(changes: &[Change], snapshot: &Snapshot) -> Vec<String> {
    changes
        .iter()
        .map(|c| {
            let label = snapshot
                .object(c.id)
                .map(|o| o.label())
                .unwrap_or_else(|| format!("#{}", c.id));
            format!(
                "{label}.{}: {} -> {}",
                c.prop,
                c.old.display(),
                c.new.display()
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prop(name: &str, value: PropValue) -> PropEntry {
        PropEntry {
            name: name.to_string(),
            kind: Some(value.kind()),
            display: value.display(),
            value: Some(value),
            access: Access::Slot(0),
            builtin: false,
            object_ref: None,
        }
    }

    fn snapshot() -> Snapshot {
        let mut readonly = prop("maxHealth", PropValue::Int(10));
        readonly.access = Access::Accessor { get: 1, set: None };
        let mut builtin = prop("tabIndex", PropValue::Int(-1));
        builtin.builtin = true;
        let mut weapon = prop("weapon", PropValue::Int(0));
        weapon.kind = None;
        weapon.value = None;
        weapon.object_ref = Some(2);
        Snapshot {
            objects: vec![
                ObjectEntry {
                    id: 1,
                    class_name: "Hero".into(),
                    instance_name: Some("hero".into()),
                    props: vec![
                        prop("health", PropValue::Int(100)),
                        prop("gold", PropValue::Uint(5)),
                        prop("speed", PropValue::Number(2.5)),
                        prop("alive", PropValue::Boolean(true)),
                        prop("title", PropValue::String("Hi".into())),
                        readonly,
                        builtin,
                        weapon,
                    ],
                },
                ObjectEntry {
                    id: 2,
                    class_name: "Gun".into(),
                    instance_name: None,
                    props: vec![prop("damage", PropValue::Number(1.0))],
                },
            ],
            truncated: false,
        }
    }

    #[test]
    fn accepts_valid_patch() {
        let s = snapshot();
        let changes = parse_patch(
            r#"[{"id":1,"set":{"health":500,"gold":7.0,"speed":1,"alive":false,"title":"Yo"}},
                {"id":2,"set":{"damage":2.5}}]"#,
            &s,
        )
        .unwrap();
        let new: Vec<_> = changes.iter().map(|c| c.new.clone()).collect();
        assert_eq!(
            new,
            vec![
                PropValue::Int(500),
                PropValue::Uint(7),
                PropValue::Number(1.0),
                PropValue::Boolean(false),
                PropValue::String("Yo".into()),
                PropValue::Number(2.5),
            ]
        );
        assert_eq!(changes[0].old, PropValue::Int(100));
    }

    #[test]
    fn accepts_fenced_patch_and_empty_patch() {
        let s = snapshot();
        let changes = parse_patch(
            "Sure!\n```json\n[{\"id\":1,\"set\":{\"health\":1}}]\n```",
            &s,
        );
        assert_eq!(changes.unwrap().len(), 1);
        assert_eq!(parse_patch("[]", &s).unwrap(), vec![]);
    }

    #[test]
    fn rejects_type_mismatches() {
        let s = snapshot();
        for (patch, needle) in [
            (r#"[{"id":1,"set":{"health":1.5}}]"#, "not an int"),
            (
                r#"[{"id":1,"set":{"health":"5"}}]"#,
                "expected int, got string",
            ),
            (r#"[{"id":1,"set":{"health":2147483648}}]"#, "not an int"),
            (r#"[{"id":1,"set":{"gold":-1}}]"#, "not a uint"),
            (r#"[{"id":1,"set":{"gold":4294967296}}]"#, "not a uint"),
            (r#"[{"id":1,"set":{"speed":"fast"}}]"#, "expected Number"),
            (r#"[{"id":1,"set":{"alive":1}}]"#, "expected Boolean"),
            (
                r#"[{"id":1,"set":{"title":null}}]"#,
                "expected String, got null",
            ),
        ] {
            let errors = parse_patch(patch, &s).unwrap_err();
            assert!(
                errors.iter().any(|e| e.contains(needle)),
                "{patch}: {errors:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_and_readonly() {
        let s = snapshot();
        for (patch, needle) in [
            (r#"[{"id":9,"set":{"health":1}}]"#, "no object with id 9"),
            (r#"[{"id":1,"set":{"mana":1}}]"#, "no such property"),
            (r#"[{"id":1,"set":{"maxHealth":1}}]"#, "read-only"),
            (r#"[{"id":1,"set":{"weapon":1}}]"#, "read-only"),
            (r#"[{"id":1,"set":{}}]"#, "is empty"),
            (r#"[{"id":1,"set":{"health":1},"extra":0}]"#, "unknown key"),
            (r#"[{"set":{"health":1}}]"#, "invalid \"id\""),
            (r#"[{"id":-1,"set":{"health":1}}]"#, "invalid \"id\""),
            (
                r#"[{"id":1,"set":{"health":1}},{"id":1,"set":{"health":2}}]"#,
                "more than once",
            ),
            (r#"[1]"#, "must be an object"),
            (r#"{"id":1}"#, "no JSON array"),
            (r#"[{"id":1,"set":{"health":1}"#, "no JSON array"),
            (r#"[{"id":1 "set":{}}]"#, "invalid JSON"),
        ] {
            let errors = parse_patch(patch, &s).unwrap_err();
            assert!(
                errors.iter().any(|e| e.contains(needle)),
                "{patch}: {errors:?}"
            );
        }
    }

    #[test]
    fn one_bad_change_rejects_whole_patch() {
        let s = snapshot();
        let errors = parse_patch(r#"[{"id":1,"set":{"health":5,"mana":1}}]"#, &s).unwrap_err();
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn llm_json_shape_and_limits() {
        let s = snapshot();
        let json: Json = serde_json::from_str(&s.to_llm_json(&SnapshotLimits::default())).unwrap();
        let hero = &json["objects"][0];
        assert_eq!(hero["id"], json!(1));
        assert_eq!(hero["name"], json!("hero"));
        assert_eq!(
            hero["props"]["health"],
            json!({"type": "int", "value": 100})
        );
        assert_eq!(hero["props"]["maxHealth"]["readonly"], json!(true));
        assert_eq!(hero["refs"]["weapon"], json!(2));
        // Built-ins outside the allow list are dropped.
        assert!(hero["props"].get("tabIndex").is_none());
        assert!(json.get("truncated").is_none());

        let limits = SnapshotLimits {
            max_objects: 1,
            ..Default::default()
        };
        let json: Json = serde_json::from_str(&s.to_llm_json(&limits)).unwrap();
        assert_eq!(json["objects"].as_array().unwrap().len(), 1);
        assert_eq!(json["truncated"], json!(true));

        let limits = SnapshotLimits {
            max_json_bytes: 10,
            ..Default::default()
        };
        let json: Json = serde_json::from_str(&s.to_llm_json(&limits)).unwrap();
        assert!(json["objects"].as_array().unwrap().is_empty());

        let limits = SnapshotLimits {
            max_props_per_object: 2,
            ..Default::default()
        };
        let json: Json = serde_json::from_str(&s.to_llm_json(&limits)).unwrap();
        assert_eq!(json["objects"][0]["props"].as_object().unwrap().len(), 2);
        assert_eq!(json["truncated"], json!(true));
    }

    #[test]
    fn non_finite_numbers_serialize_as_strings() {
        assert_eq!(PropValue::Number(f64::NAN).to_json(10), json!("NaN"));
        assert_eq!(
            PropValue::Number(f64::NEG_INFINITY).to_json(10),
            json!("-Infinity")
        );
        assert_eq!(PropValue::String("abcdef".into()).to_json(3), json!("abc…"));
    }
}
