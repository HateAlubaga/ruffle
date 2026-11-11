//! Overlay mods: rules from TOML files that change game objects while the
//! game keeps running.
//!
//! A rule is bound to a class, not to an instance, because games create and
//! destroy their objects all the time:
//!
//! ```toml
//! [mod]
//! name = "Weak bosses"
//! swf_sha256 = "…"          # optional; without it the mod applies to any SWF
//!
//! [[rule]]
//! class = "Boss"            # dotted name, or just the class name
//! on = "create"             # "create": right after the constructor,
//!                           # "first_frame": at the end of the frame the
//!                           #   object was created in,
//!                           # "every_frame": freeze the value
//! target = "instance"       # default; "static" edits the class object
//! exact = false             # default: the class or any subclass
//! [rule.set]
//! hp = 10
//! speed = 0.5
//! ```
//!
//! This module has two layers. The first one (`parse_mod`, `RuleSpec`) is
//! plain data. The second one (`ModRuntime`) lives inside `Avm2` and applies
//! the rules:
//!
//! * `on_constructed` is called by `ClassObject::call_init` when the
//!   constructor of the most derived class has finished.
//! * `run_frame` is called by the player after the AVM2 frame scripts ran, so
//!   a value written there is first seen by the game on the next frame.
//!
//! Many games set their stats up later than the constructor, for example when
//! the object is added to the stage. `create` is overwritten by such code,
//! `first_frame` is not (it waits until the frame ends), and `every_frame`
//! never is.
//! * Only declared properties (slots, and accessors that have a setter) are
//!   supported, with the same strict value checks as the object editor.
//!
//! The core never touches the file system: the frontend reads the files and
//! calls `Player::install_mods`.

use crate::avm2::object::WeakObject;
use crate::avm2::{Activation, Class, Object, TObject as _, Value};
use crate::context::UpdateContext;
use crate::modkit::inspect;
use crate::modkit::model::{Access, PropKind, PropValue};
use gc_arena::{Collect, Mutation};
use serde_json::Value as Json;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Instances that `every_frame` rules keep applying to. Past this, new
/// instances are ignored (and the UI says so) instead of growing forever.
const MAX_TRACKED: usize = 20_000;
/// Errors kept per rule.
const MAX_ERRORS: usize = 10;
/// Frames between lookups of a class that a static rule cannot find yet.
const STATIC_RETRY_FRAMES: u64 = 60;

// ---------------------------------------------------------------------------
// Plain data: the mod file format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Trigger {
    /// Right after the constructor of the class has finished.
    #[default]
    Create,
    /// Once, after the scripts of the frame in which the object was created.
    /// Wins over initialisation that the game does when the object is added
    /// to the stage, which `Create` does not.
    FirstFrame,
    /// After every frame's scripts, so the value cannot drift.
    EveryFrame,
}

impl Trigger {
    pub fn name(self) -> &'static str {
        match self {
            Trigger::Create => "create",
            Trigger::FirstFrame => "first_frame",
            Trigger::EveryFrame => "every_frame",
        }
    }

    /// Whether the rule writes once and then lets the game change the value.
    pub fn is_one_shot(self) -> bool {
        !matches!(self, Trigger::EveryFrame)
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "create" => Some(Trigger::Create),
            "first_frame" => Some(Trigger::FirstFrame),
            "every_frame" => Some(Trigger::EveryFrame),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// The objects of the class.
    Instance,
    /// The class object itself, i.e. its static variables.
    Static,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Instance => "instance",
            Target::Static => "static",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "instance" => Some(Target::Instance),
            "static" => Some(Target::Static),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuleSpec {
    /// Dotted class name (`com.foo.Boss`) or a bare class name (`Boss`).
    pub class: String,
    pub on: Trigger,
    pub target: Target,
    /// Only the class itself, not its subclasses.
    pub exact: bool,
    /// Property name and value, sorted by name.
    pub set: Vec<(String, Json)>,
}

impl RuleSpec {
    /// Class name as compared with real classes: `a::B` and `a.B` are equal.
    pub fn normalized_class(&self) -> String {
        normalize_class(&self.class)
    }

    /// Rules with the same key are the same rule in a file.
    pub fn same_key(&self, other: &RuleSpec) -> bool {
        self.normalized_class() == other.normalized_class()
            && self.on == other.on
            && self.target == other.target
            && self.exact == other.exact
    }

    /// `hp = 10, speed = 0.5`
    pub fn describe_set(&self) -> String {
        self.set
            .iter()
            .map(|(name, value)| format!("{name} = {value}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn normalize_class(name: &str) -> String {
    name.trim().replace("::", ".")
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModFile {
    pub name: String,
    pub description: String,
    /// A mod that is switched off in the file itself.
    pub enabled: bool,
    /// Lowercase hex SHA-256 of the SWF file the mod is made for.
    pub swf_sha256: Option<String>,
    pub rules: Vec<RuleSpec>,
}

fn check_keys(table: &toml::Table, allowed: &[&str], what: &str) -> Result<(), String> {
    match table.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!(
            "{what}: unknown key `{key}` (expected one of: {})",
            allowed.join(", ")
        )),
        None => Ok(()),
    }
}

fn optional_string<'a>(
    table: &'a toml::Table,
    key: &str,
    what: &str,
) -> Result<Option<&'a str>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(format!(
            "{what}: `{key}` must be a string, not {}",
            other.type_str()
        )),
    }
}

fn optional_bool(table: &toml::Table, key: &str, what: &str) -> Result<Option<bool>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Boolean(b)) => Ok(Some(*b)),
        Some(other) => Err(format!(
            "{what}: `{key}` must be true or false, not {}",
            other.type_str()
        )),
    }
}

fn json_from_toml(value: &toml::Value) -> Result<Json, String> {
    match value {
        toml::Value::Boolean(b) => Ok(Json::Bool(*b)),
        toml::Value::Integer(i) => Ok(Json::from(*i)),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .ok_or_else(|| "NaN and infinity are not allowed".to_string()),
        toml::Value::String(s) => Ok(Json::String(s.clone())),
        other => Err(format!(
            "{} values are not supported, use a boolean, number or string",
            other.type_str()
        )),
    }
}

fn parse_rule(table: &toml::Table) -> Result<RuleSpec, String> {
    check_keys(table, &["class", "on", "target", "exact", "set"], "rule")?;

    let class = optional_string(table, "class", "rule")?
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or("rule: `class` is required")?
        .to_string();

    let on = optional_string(table, "on", "rule")?.ok_or("rule: `on` is required")?;
    let on = Trigger::parse(on).ok_or_else(|| {
        format!("rule: `on` must be \"create\", \"first_frame\" or \"every_frame\", not {on:?}")
    })?;

    let target = match optional_string(table, "target", "rule")? {
        None => Target::Instance,
        Some(text) => Target::parse(text).ok_or_else(|| {
            format!("rule: `target` must be \"instance\" or \"static\", not {text:?}")
        })?,
    };
    let exact = optional_bool(table, "exact", "rule")?.unwrap_or(false);

    let set = match table.get("set") {
        Some(toml::Value::Table(set)) => set,
        Some(other) => {
            return Err(format!(
                "rule: `set` must be a table, not {}",
                other.type_str()
            ));
        }
        None => return Err("rule: `set` is required".into()),
    };
    if set.is_empty() {
        return Err("rule: `set` is empty".into());
    }
    let mut pairs = Vec::new();
    for (name, value) in set {
        let value = json_from_toml(value).map_err(|e| format!("rule: `{name}`: {e}"))?;
        pairs.push((name.clone(), value));
    }

    Ok(RuleSpec {
        class,
        on,
        target,
        exact,
        set: pairs,
    })
}

/// Parses and validates a mod file. Unknown keys are errors, so a typo never
/// turns into a rule that silently does nothing.
pub fn parse_mod(text: &str) -> Result<ModFile, String> {
    let root: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    check_keys(&root, &["mod", "rule"], "mod file")?;

    let empty = toml::Table::new();
    let header = match root.get("mod") {
        None => &empty,
        Some(toml::Value::Table(t)) => t,
        Some(other) => {
            return Err(format!("[mod] must be a table, not {}", other.type_str()));
        }
    };
    check_keys(
        header,
        &["name", "description", "swf_sha256", "enabled"],
        "[mod]",
    )?;
    let name = optional_string(header, "name", "[mod]")?
        .unwrap_or_default()
        .to_string();
    let description = optional_string(header, "description", "[mod]")?
        .unwrap_or_default()
        .to_string();
    let enabled = optional_bool(header, "enabled", "[mod]")?.unwrap_or(true);
    let swf_sha256 = match optional_string(header, "swf_sha256", "[mod]")? {
        None => None,
        Some(sha) => {
            let sha = sha.trim().to_ascii_lowercase();
            if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("[mod]: `swf_sha256` must be 64 hex digits".into());
            }
            Some(sha)
        }
    };

    let mut rules = Vec::new();
    match root.get("rule") {
        None => {}
        Some(toml::Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                let toml::Value::Table(table) = item else {
                    return Err(format!("rule {}: must be a table", index + 1));
                };
                rules.push(parse_rule(table).map_err(|e| format!("rule {}: {e}", index + 1))?);
            }
        }
        Some(other) => {
            return Err(format!(
                "`rule` must be an array of tables ([[rule]]), not {}",
                other.type_str()
            ));
        }
    }

    Ok(ModFile {
        name,
        description,
        enabled,
        swf_sha256,
        rules,
    })
}

// ---------------------------------------------------------------------------
// Data exchanged with the frontend and the UI
// ---------------------------------------------------------------------------

/// A mod file as the frontend read it.
#[derive(Debug, Clone)]
pub struct ModSource {
    pub file: String,
    pub text: Result<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModStatus {
    Active,
    /// `enabled = false` in the file.
    Disabled,
    /// Bound to another SWF (or the SWF is not a local file).
    OtherSwf,
    Invalid(String),
}

#[derive(Debug, Clone)]
pub struct RuleInfo {
    pub spec: RuleSpec,
    pub enabled: bool,
    pub applied: u64,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModInfo {
    pub file: String,
    pub name: String,
    pub description: String,
    pub status: ModStatus,
    pub rules: Vec<RuleInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct ModsSnapshot {
    pub mods: Vec<ModInfo>,
    pub dir: String,
    pub swf_sha256: Option<String>,
    pub tracked: usize,
    pub overflowed: bool,
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

struct ActiveRule {
    spec: RuleSpec,
    class: String,
    enabled: Cell<bool>,
    applied: Cell<u64>,
    errors: RefCell<Vec<String>>,
}

impl ActiveRule {
    fn new(spec: RuleSpec) -> Rc<Self> {
        Rc::new(Self {
            class: spec.normalized_class(),
            spec,
            enabled: Cell::new(true),
            applied: Cell::new(0),
            errors: RefCell::new(Vec::new()),
        })
    }

    fn note_error(&self, message: String) {
        let mut errors = self.errors.borrow_mut();
        if errors.len() < MAX_ERRORS && !errors.contains(&message) {
            errors.push(message);
        }
    }

    fn matches_chain(&self, chain: &[(String, String)]) -> bool {
        let names_match =
            |(dotted, local): &(String, String)| *dotted == self.class || *local == self.class;
        if self.spec.exact {
            chain.first().is_some_and(names_match)
        } else {
            chain.iter().any(names_match)
        }
    }
}

struct LoadedMod {
    file: String,
    name: String,
    description: String,
    status: ModStatus,
    rules: Vec<Rc<ActiveRule>>,
}

/// What is known about one property of one rule.
#[derive(Clone)]
enum PropState {
    Unresolved,
    Ready {
        /// Behind an `Rc`, so cloning the state never copies strings.
        data: Rc<ReadyProp>,
        /// Frames with a failed write in a row. A write can fail for a reason
        /// that passes (a setter that throws on some frames), so one failure
        /// does not end the rule; `MAX_WRITE_FAILURES` in a row do.
        failures: u8,
        /// Frame of the last counted failure (`u64::MAX`: none yet).
        last_failure_frame: u64,
    },
    /// Cannot be applied; the reason is in the rule's errors.
    Dead,
}

/// How to write one resolved property.
struct ReadyProp {
    access: Access,
    value: PropValue,
    /// Skip the write when the property already holds the value.
    compare: Option<PropKind>,
}

impl PropState {
    /// Counts a failed write, at most once per frame: the state is shared by
    /// all instances of the class. Returns whether to give the property up.
    fn note_failure(&mut self, frame: u64) -> bool {
        match self {
            PropState::Ready {
                failures,
                last_failure_frame,
                ..
            } => {
                if *last_failure_frame != frame {
                    *last_failure_frame = frame;
                    *failures += 1;
                }
                *failures >= MAX_WRITE_FAILURES
            }
            _ => false,
        }
    }
}

/// A property whose writes fail in this many frames in a row is given up.
const MAX_WRITE_FAILURES: u8 = 20;

/// A rule together with what was learned about its properties. Properties
/// belong to the class, so the state is shared by all of its instances.
struct PlanRule {
    rule: Rc<ActiveRule>,
    props: RefCell<Vec<PropState>>,
}

impl PlanRule {
    fn new(rule: Rc<ActiveRule>) -> Self {
        let props = vec![PropState::Unresolved; rule.spec.set.len()];
        Self {
            rule,
            props: RefCell::new(props),
        }
    }
}

/// The rules that apply to the instances of one class.
#[derive(Collect)]
#[collect(require_static)]
struct Plan {
    entries: Vec<PlanRule>,
    has_create: bool,
    has_first_frame: bool,
    has_every_frame: bool,
}

#[derive(Collect)]
#[collect(no_drop)]
struct Tracked<'gc> {
    object: WeakObject<'gc>,
    plan: Rc<Plan>,
}

#[derive(Collect)]
#[collect(no_drop)]
struct StaticRule<'gc> {
    #[collect(require_static)]
    entry: Rc<PlanRule>,
    class: Option<Object<'gc>>,
    next_lookup: u64,
    done: bool,
}

#[derive(Collect, Default)]
#[collect(no_drop)]
pub struct ModRuntime<'gc> {
    #[collect(require_static)]
    mods: Vec<LoadedMod>,
    #[collect(require_static)]
    instance_rules: Vec<Rc<ActiveRule>>,
    plans: HashMap<Class<'gc>, Option<Rc<Plan>>>,
    tracked: Vec<Tracked<'gc>>,
    /// Objects created in the current frame that have `first_frame` rules.
    pending: Vec<Tracked<'gc>>,
    statics: Vec<StaticRule<'gc>>,
    frame: u64,
    /// `frame + 1` of the last prune of a full `tracked` list (0: never).
    pruned_at: u64,
    overflowed: bool,
    /// URL of the root movie the mods were installed for.
    #[collect(require_static)]
    installed_for: Option<String>,
    #[collect(require_static)]
    dir: String,
    #[collect(require_static)]
    swf_sha256: Option<String>,
}

fn class_chain<'gc>(class: Class<'gc>, mc: &Mutation<'gc>) -> Vec<(String, String)> {
    let mut chain = Vec::new();
    let mut next = Some(class);
    while let Some(class) = next {
        let dotted = class
            .name()
            .to_qualified_name(mc)
            .to_string()
            .replace("::", ".");
        chain.push((dotted, class.name().local_name().to_string()));
        next = class.super_class();
    }
    chain
}

fn build_plan<'gc>(
    rules: &[Rc<ActiveRule>],
    class: Class<'gc>,
    mc: &Mutation<'gc>,
) -> Option<Rc<Plan>> {
    let chain = class_chain(class, mc);
    let entries: Vec<PlanRule> = rules
        .iter()
        .filter(|rule| rule.matches_chain(&chain))
        .map(|rule| PlanRule::new(rule.clone()))
        .collect();
    if entries.is_empty() {
        return None;
    }
    let has = |trigger| entries.iter().any(|e| e.rule.spec.on == trigger);
    Some(Rc::new(Plan {
        has_create: has(Trigger::Create),
        has_first_frame: has(Trigger::FirstFrame),
        has_every_frame: has(Trigger::EveryFrame),
        entries,
    }))
}

impl<'gc> ModRuntime<'gc> {
    pub fn installed_for(&self) -> Option<&str> {
        self.installed_for.as_deref()
    }

    /// Replaces all mods. Called whenever the frontend (re)loads them.
    pub fn install(
        &mut self,
        sources: Vec<ModSource>,
        swf_sha256: Option<String>,
        root_url: String,
        dir: String,
        mc: &Mutation<'gc>,
    ) {
        self.mods.clear();
        self.instance_rules.clear();
        self.plans.clear();
        self.statics.clear();
        let old_pending = std::mem::take(&mut self.pending);
        self.overflowed = false;

        for source in sources {
            let loaded = load_mod(source, swf_sha256.as_deref());
            if loaded.status == ModStatus::Active {
                for rule in &loaded.rules {
                    match rule.spec.target {
                        Target::Instance => self.instance_rules.push(rule.clone()),
                        Target::Static => self.statics.push(StaticRule {
                            entry: Rc::new(PlanRule::new(rule.clone())),
                            class: None,
                            next_lookup: 0,
                            done: false,
                        }),
                    }
                }
            }
            self.mods.push(loaded);
        }
        self.installed_for = Some(root_url);
        self.dir = dir;
        self.swf_sha256 = swf_sha256;

        // Objects that are already frozen keep their freeze if the new rules
        // still say so.
        let old = std::mem::take(&mut self.tracked);
        for tracked in old {
            if let Some(object) = tracked.object.upgrade(mc) {
                self.track(object, mc);
            }
        }
        // Objects still waiting for their `first_frame` rules keep waiting.
        for waiting in old_pending {
            if let Some(object) = waiting.object.upgrade(mc)
                && let Some(plan) = self.plan_for(object.instance_class(), mc)
                && plan.has_first_frame
            {
                self.defer_first_frame(object, &plan);
            }
        }
    }

    fn plan_for(&mut self, class: Class<'gc>, mc: &Mutation<'gc>) -> Option<Rc<Plan>> {
        if self.instance_rules.is_empty() {
            return None;
        }
        if let Some(plan) = self.plans.get(&class) {
            return plan.clone();
        }
        let plan = build_plan(&self.instance_rules, class, mc);
        self.plans.insert(class, plan.clone());
        plan
    }

    /// Starts applying `every_frame` rules to `object`, if there are any for
    /// its class. Returns the plan of the class.
    fn track(&mut self, object: Object<'gc>, mc: &Mutation<'gc>) -> Option<Rc<Plan>> {
        let plan = self.plan_for(object.instance_class(), mc)?;
        if plan.has_every_frame {
            // A full list may hold dead objects; drop them, once per frame.
            if self.tracked.len() >= MAX_TRACKED && self.pruned_at != self.frame + 1 {
                self.pruned_at = self.frame + 1;
                self.tracked.retain(|t| t.object.upgrade(mc).is_some());
            }
            if self.tracked.len() >= MAX_TRACKED {
                self.overflowed = true;
            } else {
                self.tracked.push(Tracked {
                    object: object.downgrade(),
                    plan: plan.clone(),
                });
            }
        }
        Some(plan)
    }

    /// Makes `every_frame` rules take effect on an object that exists
    /// already (the one selected in the editor), not only on new ones.
    pub fn adopt(&mut self, object: Object<'gc>, mc: &Mutation<'gc>) {
        if object.as_class_object().is_some() {
            return;
        }
        let known = self.tracked.iter().any(|t| {
            t.object
                .upgrade(mc)
                .is_some_and(|o| o.as_ptr() == object.as_ptr())
        });
        if !known {
            self.track(object, mc);
        }
    }

    pub fn set_rule_enabled(&mut self, mod_index: usize, rule_index: usize, enabled: bool) {
        if let Some(rule) = self
            .mods
            .get(mod_index)
            .and_then(|m| m.rules.get(rule_index))
        {
            rule.enabled.set(enabled);
        }
    }

    pub fn snapshot(&self) -> ModsSnapshot {
        ModsSnapshot {
            mods: self
                .mods
                .iter()
                .map(|m| ModInfo {
                    file: m.file.clone(),
                    name: m.name.clone(),
                    description: m.description.clone(),
                    status: m.status.clone(),
                    rules: m
                        .rules
                        .iter()
                        .map(|r| RuleInfo {
                            spec: r.spec.clone(),
                            enabled: r.enabled.get(),
                            applied: r.applied.get(),
                            errors: r.errors.borrow().clone(),
                        })
                        .collect(),
                })
                .collect(),
            dir: self.dir.clone(),
            swf_sha256: self.swf_sha256.clone(),
            tracked: self.tracked.len(),
            overflowed: self.overflowed,
        }
    }

    /// The live objects that `every_frame` rules are applied to.
    #[cfg(test)]
    pub(crate) fn tracked_objects(&self, mc: &Mutation<'gc>) -> Vec<Object<'gc>> {
        self.tracked
            .iter()
            .filter_map(|t| t.object.upgrade(mc))
            .collect()
    }

    fn has_frame_work(&self) -> bool {
        !self.tracked.is_empty() || !self.pending.is_empty() || self.statics.iter().any(|s| !s.done)
    }

    /// Remembers an object for the `first_frame` rules of its plan.
    fn defer_first_frame(&mut self, object: Object<'gc>, plan: &Rc<Plan>) {
        if self.pending.len() >= MAX_TRACKED {
            self.overflowed = true;
        } else {
            self.pending.push(Tracked {
                object: object.downgrade(),
                plan: plan.clone(),
            });
        }
    }

    /// The live objects waiting for their `first_frame` rules. Empties the
    /// list.
    fn take_pending(&mut self, mc: &Mutation<'gc>) -> Vec<(Object<'gc>, Rc<Plan>)> {
        std::mem::take(&mut self.pending)
            .into_iter()
            .filter_map(|t| Some((t.object.upgrade(mc)?, t.plan)))
            .collect()
    }
}

fn load_mod(source: ModSource, swf_sha256: Option<&str>) -> LoadedMod {
    let stem = source.file.strip_suffix(".toml").unwrap_or(&source.file);
    let mut loaded = LoadedMod {
        file: source.file.clone(),
        name: stem.to_string(),
        description: String::new(),
        status: ModStatus::Active,
        rules: Vec::new(),
    };
    let parsed = source.text.and_then(|text| parse_mod(&text));
    let file = match parsed {
        Ok(file) => file,
        Err(e) => {
            loaded.status = ModStatus::Invalid(e);
            return loaded;
        }
    };
    if !file.name.is_empty() {
        loaded.name = file.name;
    }
    loaded.description = file.description;
    loaded.status = if !file.enabled {
        ModStatus::Disabled
    } else if file
        .swf_sha256
        .as_deref()
        .is_some_and(|wanted| Some(wanted) != swf_sha256)
    {
        ModStatus::OtherSwf
    } else {
        ModStatus::Active
    };
    loaded.rules = file.rules.into_iter().map(ActiveRule::new).collect();
    loaded
}

// ---------------------------------------------------------------------------
// Applying rules
// ---------------------------------------------------------------------------

fn apply_prop<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    entry: &PlanRule,
    index: usize,
) {
    let rule = &entry.rule;
    let (name, json) = &rule.spec.set[index];

    // Never hold a RefCell borrow across calls into AVM2: a setter can
    // construct objects and come back here.
    let state = entry.props.borrow()[index].clone();
    let state = match state {
        PropState::Unresolved => {
            let resolved = match inspect::resolve_writable(activation, object, name, json) {
                Ok(w) => PropState::Ready {
                    data: Rc::new(ReadyProp {
                        access: w.access,
                        value: w.value,
                        compare: w.comparable.then_some(w.kind),
                    }),
                    failures: 0,
                    last_failure_frame: u64::MAX,
                },
                Err(e) => {
                    rule.note_error(format!("{name}: {e}"));
                    PropState::Dead
                }
            };
            entry.props.borrow_mut()[index] = resolved.clone();
            resolved
        }
        other => other,
    };
    let PropState::Ready { data, failures, .. } = state else {
        return;
    };
    let (access, value) = (&data.access, &data.value);

    if let Some(kind) = data.compare
        && inspect::read_prop(activation, object, access, kind).as_ref() == Ok(value)
    {
        return;
    }
    match inspect::write_prop(activation, object, access, value) {
        Ok(()) => {
            rule.applied.set(rule.applied.get() + 1);
            if failures > 0
                && let PropState::Ready { failures, .. } = &mut entry.props.borrow_mut()[index]
            {
                *failures = 0;
            }
        }
        Err(e) => {
            rule.note_error(format!("{name}: {e}"));
            let frame = activation.avm2().mods.frame;
            let mut props = entry.props.borrow_mut();
            if props[index].note_failure(frame) {
                props[index] = PropState::Dead;
            }
        }
    }
}

fn apply_entry<'gc>(activation: &mut Activation<'_, 'gc>, object: Object<'gc>, entry: &PlanRule) {
    for index in 0..entry.rule.spec.set.len() {
        apply_prop(activation, object, entry, index);
    }
}

fn apply_plan<'gc>(
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    plan: &Plan,
    trigger: Trigger,
) {
    for entry in &plan.entries {
        if entry.rule.spec.on == trigger && entry.rule.enabled.get() {
            apply_entry(activation, object, entry);
        }
    }
}

/// Called when the constructor of `class_object` has finished for `receiver`.
///
/// Constructors of superclasses finish earlier and are ignored: a rule must
/// see the object after the whole constructor chain, or the game's own
/// initialisation would overwrite it.
pub(crate) fn on_constructed<'gc>(
    class_object: crate::avm2::object::ClassObject<'gc>,
    receiver: Value<'gc>,
    activation: &mut Activation<'_, 'gc>,
) {
    if activation.avm2().mods.instance_rules.is_empty() {
        return;
    }
    let Some(object) = receiver.as_object() else {
        return;
    };
    if class_object.inner_class_definition() != object.instance_class() {
        return;
    }
    let mc = activation.gc();
    let Some(plan) = activation.avm2().mods.track(object, mc) else {
        return;
    };
    if plan.has_first_frame {
        activation.avm2().mods.defer_first_frame(object, &plan);
    }
    if plan.has_create {
        apply_plan(activation, object, &plan, Trigger::Create);
    }
}

/// Called once per frame after the AVM2 frame scripts ran.
pub(crate) fn run_frame<'gc>(context: &mut UpdateContext<'gc>) {
    if !context.avm2.mods.has_frame_work() {
        return;
    }
    let mc = context.gc();
    let mods = &mut context.avm2.mods;
    mods.frame += 1;
    let frame = mods.frame;
    let pending = mods.take_pending(mc);
    // Taken out for the loop: rules call into AVM2, which can track objects.
    let mut tracked = std::mem::take(&mut mods.tracked);

    let domain = context.avm2.stage_domain();
    let mut activation = Activation::from_domain(context, domain);
    for (object, plan) in &pending {
        apply_plan(&mut activation, *object, plan, Trigger::FirstFrame);
    }
    tracked.retain(|t| match t.object.upgrade(mc) {
        Some(object) => {
            apply_plan(&mut activation, object, &t.plan, Trigger::EveryFrame);
            true
        }
        None => false,
    });
    // Put the list back, followed by the objects tracked during the loop.
    let mods = &mut activation.avm2().mods;
    let added = std::mem::replace(&mut mods.tracked, tracked);
    let room = MAX_TRACKED.saturating_sub(mods.tracked.len());
    if added.len() > room {
        mods.overflowed = true;
    }
    mods.tracked.extend(added.into_iter().take(room));
    if mods.tracked.len() < MAX_TRACKED && mods.pending.len() < MAX_TRACKED {
        mods.overflowed = false;
    }

    for index in 0..activation.avm2().mods.statics.len() {
        run_static(&mut activation, index, frame);
    }
}

fn run_static<'gc>(activation: &mut Activation<'_, 'gc>, index: usize, frame: u64) {
    let (entry, class, next_lookup, done) = {
        let slot = &activation.avm2().mods.statics[index];
        (slot.entry.clone(), slot.class, slot.next_lookup, slot.done)
    };
    if done || !entry.rule.enabled.get() {
        return;
    }
    let class = match class {
        Some(class) => class,
        None => {
            if frame < next_lookup {
                return;
            }
            let found = inspect::find_class_object(activation, &entry.rule.class);
            let slot = &mut activation.avm2().mods.statics[index];
            match found {
                Some(class) => {
                    slot.class = Some(class);
                    class
                }
                None => {
                    slot.next_lookup = frame + STATIC_RETRY_FRAMES;
                    return;
                }
            }
        }
    };
    apply_entry(activation, class, &entry);
    if entry.rule.spec.on.is_one_shot() {
        activation.avm2().mods.statics[index].done = true;
    }
}

/// Replaces the mods of the running game. See `Player::install_mods`.
pub(crate) fn install<'gc>(
    context: &mut UpdateContext<'gc>,
    sources: Vec<ModSource>,
    swf_sha256: Option<String>,
    root_url: String,
    dir: String,
) {
    let mc = context.gc();
    context
        .avm2
        .mods
        .install(sources, swf_sha256, root_url, dir, mc);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn rule(class: &str, exact: bool) -> RuleSpec {
        RuleSpec {
            class: class.into(),
            on: Trigger::Create,
            target: Target::Instance,
            exact,
            set: vec![("hp".into(), Json::from(1))],
        }
    }

    fn chain(names: &[(&str, &str)]) -> Vec<(String, String)> {
        names
            .iter()
            .map(|(d, l)| (d.to_string(), l.to_string()))
            .collect()
    }

    #[test]
    fn parses_a_full_mod() {
        let file = parse_mod(&format!(
            r#"
            [mod]
            name = "Weak bosses"
            description = "less hp"
            swf_sha256 = "{}"

            [[rule]]
            class = "com.foo::Boss"
            on = "create"
            [rule.set]
            hp = 10
            speed = 0.5
            name = "x"
            alive = true

            [[rule]]
            class = "Stats"
            on = "every_frame"
            target = "static"
            exact = true
            [rule.set]
            bulletNum = 3
            "#,
            SHA.to_uppercase()
        ))
        .unwrap();
        assert_eq!(file.name, "Weak bosses");
        assert_eq!(file.description, "less hp");
        assert!(file.enabled);
        assert_eq!(file.swf_sha256.as_deref(), Some(SHA));
        assert_eq!(file.rules.len(), 2);
        let first = &file.rules[0];
        assert_eq!(first.on, Trigger::Create);
        assert_eq!(first.target, Target::Instance);
        assert!(!first.exact);
        assert_eq!(first.normalized_class(), "com.foo.Boss");
        let get = |name: &str| first.set.iter().find(|(n, _)| n == name).unwrap().1.clone();
        assert_eq!(get("hp"), Json::from(10));
        assert_eq!(get("speed"), Json::from(0.5));
        assert_eq!(get("name"), Json::from("x"));
        assert_eq!(get("alive"), Json::Bool(true));
        let second = &file.rules[1];
        assert_eq!(second.on, Trigger::EveryFrame);
        assert_eq!(second.target, Target::Static);
        assert!(second.exact);
    }

    #[test]
    fn trigger_names_round_trip() {
        for trigger in [Trigger::Create, Trigger::FirstFrame, Trigger::EveryFrame] {
            assert_eq!(Trigger::parse(trigger.name()), Some(trigger));
            let file = parse_mod(&format!(
                "[[rule]]\nclass='A'\non='{}'\n[rule.set]\nx=1\n",
                trigger.name()
            ))
            .unwrap();
            assert_eq!(file.rules[0].on, trigger);
        }
        assert!(Trigger::Create.is_one_shot());
        assert!(Trigger::FirstFrame.is_one_shot());
        assert!(!Trigger::EveryFrame.is_one_shot());
        assert_eq!(Trigger::parse("FIRST_FRAME"), None);
    }

    #[test]
    fn mod_header_is_optional() {
        let file = parse_mod("[[rule]]\nclass='A'\non='create'\n[rule.set]\nx=1\n").unwrap();
        assert_eq!(file.name, "");
        assert_eq!(file.swf_sha256, None);
        assert_eq!(parse_mod("").unwrap().rules.len(), 0);
    }

    #[test]
    fn rejects_bad_files() {
        let bad = |text: &str| parse_mod(text).unwrap_err();
        let rule = |body: &str| format!("[[rule]]\n{body}");
        assert!(bad("[mod").contains("expected"), "syntax error");
        assert!(bad("x = 1").contains("unknown key `x`"));
        assert!(bad("[mod]\nnmae = 'a'").contains("unknown key `nmae`"));
        assert!(bad("[mod]\nswf_sha256 = 'abc'").contains("64 hex"));
        assert!(bad("[mod]\nname = 3").contains("must be a string"));
        assert!(bad("rule = 3").contains("array of tables"));
        assert!(bad(&rule("on='create'\n[rule.set]\nx=1")).contains("`class` is required"));
        assert!(bad(&rule("class=' '\non='create'\n[rule.set]\nx=1")).contains("`class`"));
        assert!(bad(&rule("class='A'\n[rule.set]\nx=1")).contains("`on` is required"));
        assert!(bad(&rule("class='A'\non='sometimes'\n[rule.set]\nx=1")).contains("`on` must be"));
        assert!(
            bad(&rule(
                "class='A'\non='create'\ntarget='both'\n[rule.set]\nx=1"
            ))
            .contains("`target` must be")
        );
        assert!(
            bad(&rule(
                "class='A'\non='create'\nexact='yes'\n[rule.set]\nx=1"
            ))
            .contains("true or false")
        );
        assert!(bad(&rule("class='A'\non='create'")).contains("`set` is required"));
        assert!(bad(&rule("class='A'\non='create'\nset=3")).contains("`set` must be a table"));
        assert!(bad(&rule("class='A'\non='create'\n[rule.set]")).contains("`set` is empty"));
        assert!(
            bad(&rule("class='A'\non='create'\nspeed=1\n[rule.set]\nx=1"))
                .contains("unknown key `speed`")
        );
        assert!(bad(&rule("class='A'\non='create'\n[rule.set]\nx=[1]")).contains("array values"));
        assert!(bad(&rule("class='A'\non='create'\n[rule.set]\nx=nan")).contains("NaN"));
        assert!(bad(&rule("class='A'\non='create'\n[rule.set]\nx=inf")).contains("infinity"));
        // The rule number helps to find the problem in a long file.
        let two = format!(
            "{}\n{}",
            rule("class='A'\non='create'\n[rule.set]\nx=1"),
            rule("on='create'")
        );
        assert!(bad(&two).starts_with("rule 2:"), "{}", bad(&two));
    }

    #[test]
    fn disabled_and_foreign_mods_are_not_active() {
        let text = |header: &str| ModSource {
            file: "m.toml".into(),
            text: Ok(format!(
                "[mod]\n{header}\n[[rule]]\nclass='A'\non='create'\n[rule.set]\nx=1\n"
            )),
        };
        let status = |header: &str, sha: Option<&str>| load_mod(text(header), sha).status;
        assert_eq!(status("", None), ModStatus::Active);
        assert_eq!(status("enabled=false", None), ModStatus::Disabled);
        assert_eq!(
            status(&format!("swf_sha256='{SHA}'"), Some(SHA)),
            ModStatus::Active
        );
        assert_eq!(
            status(&format!("swf_sha256='{SHA}'"), Some(&"f".repeat(64))),
            ModStatus::OtherSwf
        );
        // No hash for the running SWF (not a local file): a bound mod waits.
        assert_eq!(
            status(&format!("swf_sha256='{SHA}'"), None),
            ModStatus::OtherSwf
        );
        let broken = load_mod(
            ModSource {
                file: "b.toml".into(),
                text: Err("denied".into()),
            },
            None,
        );
        assert_eq!(broken.status, ModStatus::Invalid("denied".into()));
        assert_eq!(broken.name, "b");
        assert!(broken.rules.is_empty());
    }

    #[test]
    fn class_matching() {
        let boss3 = chain(&[
            ("game.Boss3", "Boss3"),
            ("game.Boss", "Boss"),
            ("flash.display.Sprite", "Sprite"),
        ]);
        let matches =
            |class: &str, exact: bool| ActiveRule::new(rule(class, exact)).matches_chain(&boss3);
        assert!(matches("game.Boss3", true));
        assert!(matches("Boss3", true));
        assert!(matches("game::Boss3", true));
        assert!(
            !matches("Boss", true),
            "exact means the instance class only"
        );
        assert!(matches("Boss", false));
        assert!(matches("game.Boss", false));
        assert!(matches("Sprite", false));
        assert!(!matches("other.Boss", false));
        assert!(!matches("Boss4", false));
        assert!(!ActiveRule::new(rule("Boss", false)).matches_chain(&[]));
    }

    #[test]
    fn write_failures_count_once_per_frame() {
        let mut state = PropState::Ready {
            data: Rc::new(ReadyProp {
                access: Access::ConstSlot(0),
                value: PropValue::Int(1),
                compare: None,
            }),
            failures: 0,
            last_failure_frame: u64::MAX,
        };
        // Many instances failing in one frame count as one failure.
        for _ in 0..100 {
            assert!(!state.note_failure(0));
        }
        for frame in 1..u64::from(MAX_WRITE_FAILURES) - 1 {
            assert!(!state.note_failure(frame));
        }
        assert!(state.note_failure(u64::from(MAX_WRITE_FAILURES)));
        assert!(!PropState::Unresolved.note_failure(0));
        assert!(!PropState::Dead.note_failure(0));
    }

    #[test]
    fn rule_keys() {
        let a = rule("game::Boss", false);
        let mut b = rule("game.Boss", false);
        b.set = vec![("other".into(), Json::from(2))];
        assert!(a.same_key(&b));
        b.on = Trigger::EveryFrame;
        assert!(!a.same_key(&b));
        assert_eq!(a.describe_set(), "hp = 1");
    }
}
