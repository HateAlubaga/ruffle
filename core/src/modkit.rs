//! Ruffle Modkit: live game editing tools.
//!
//! * [`speed::Speed`] scales game time (`Player::set_speed`).
//! * The Modkit window picks objects on stage, shows their properties and
//!   edits them with type checks, with undo and value locks.
//! * AI edits: a JSON snapshot of the selection goes to an LLM, which answers
//!   with a strict JSON patch that is validated, previewed and applied.
//! * FFDec bridge: shows the decompiled source of a selected object's class.
//!
//! Anything that needs the network, processes or the file system (LLM calls,
//! FFDec) is done by the frontend: this module queues [`HostRequest`]s and
//! receives [`HostResponse`]s.

pub mod inspect;
#[cfg(test)]
mod live_tests;
pub mod model;
pub mod mods;
pub mod speed;
mod ui;

use crate::avm2::{Activation, Object};
use crate::context::{RenderContext, UpdateContext};
use crate::debug_ui::handle::DisplayObjectHandle;
use crate::display_object::{DisplayObject, TDisplayObject};
use crate::modkit::inspect::Registry;
use crate::modkit::model::{Access, Change, PropValue, Snapshot, SnapshotLimits};
use crate::modkit::mods::{RuleSpec, Target, Trigger};
use gc_arena::DynamicRootSet;
use ruffle_render::matrix::Matrix;
use std::sync::atomic::{AtomicU64, Ordering};
use swf::{Color, Twips};

pub use crate::modkit::speed::{MAX_SPEED, MIN_SPEED};

/// Request ids are unique across players: the host outlives a `Player`, so a
/// late response for an old one must not match a request of the new one.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiProvider {
    /// Any `/chat/completions` endpoint: OpenAI, OpenRouter, OmniRoute, Ollama, ...
    #[default]
    OpenAiCompatible,
    Anthropic,
}

impl AiProvider {
    pub fn id(self) -> &'static str {
        match self {
            AiProvider::OpenAiCompatible => "openai",
            AiProvider::Anthropic => "anthropic",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "openai" => Some(AiProvider::OpenAiCompatible),
            "anthropic" => Some(AiProvider::Anthropic),
            _ => None,
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            AiProvider::OpenAiCompatible => "http://127.0.0.1:20128/v1",
            AiProvider::Anthropic => "https://api.anthropic.com",
        }
    }

    /// Environment variable read when no key is set in the settings.
    pub fn key_env_var(self) -> &'static str {
        match self {
            AiProvider::OpenAiCompatible => "RUFFLE_MODKIT_API_KEY",
            AiProvider::Anthropic => "ANTHROPIC_API_KEY",
        }
    }
}

/// Settings owned by the frontend (which persists them) and edited in the
/// Modkit window.
#[derive(Debug, Clone, PartialEq)]
pub struct ModkitSettings {
    /// Path to `ffdec-cli` (or `ffdec.jar` / `ffdec.bat`).
    pub ffdec_path: String,
    pub ai_provider: AiProvider,
    /// Empty means `ai_provider.default_base_url()`.
    pub ai_base_url: String,
    /// Empty means the `RUFFLE_MODKIT_MODEL` environment variable.
    pub ai_model: String,
    /// Empty means the provider's environment variable.
    pub ai_api_key: String,
    /// Pause the game while an AI request is pending and under review, and
    /// resume when the patch is applied or dismissed.
    pub ai_auto_pause: bool,
}

impl Default for ModkitSettings {
    fn default() -> Self {
        Self {
            ffdec_path: String::new(),
            ai_provider: AiProvider::default(),
            ai_base_url: String::new(),
            ai_model: String::new(),
            ai_api_key: String::new(),
            ai_auto_pause: true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum HostRequest {
    Ai {
        id: u64,
        system: String,
        user: String,
    },
    ClassSource {
        id: u64,
        /// Dotted name, e.g. `com.foo.Bar`.
        class_name: String,
        swf_url: String,
    },
    OpenInFfdec {
        class_name: String,
        swf_url: String,
    },
    /// Adds (or merges) a rule into a mod file of the current SWF.
    SaveRule {
        rule: RuleSpec,
    },
    /// Removes a rule from a mod file.
    RemoveRule {
        file: String,
        rule: RuleSpec,
    },
    /// Re-reads the mod files and reinstalls them.
    ReloadMods,
}

#[derive(Debug, Clone)]
pub enum HostResponse {
    Ai {
        id: u64,
        result: Result<String, String>,
    },
    ClassSource {
        id: u64,
        result: Result<String, String>,
    },
    /// The mods were (re)installed or a mod file operation finished.
    Mods(Result<String, String>),
    /// A message for the status line.
    Status(String),
}

#[derive(Debug, Clone)]
struct UndoEntry {
    id: u32,
    prop: String,
    access: Access,
    old: PropValue,
    new: PropValue,
}

#[derive(Debug, Clone)]
struct UndoBatch {
    label: String,
    entries: Vec<UndoEntry>,
    /// Pass number of the last UI edit merged into this batch.
    last_pass: Option<u64>,
}

#[derive(Debug, Clone)]
struct Lock {
    id: u32,
    prop: String,
    access: Access,
    value: PropValue,
}

#[derive(Debug, Default)]
enum AiState {
    #[default]
    Idle,
    Waiting {
        request_id: u64,
        snapshot: Snapshot,
    },
    Review {
        changes: Vec<Change>,
        /// One description per change, built once.
        lines: Vec<String>,
        enabled: Vec<bool>,
        raw: String,
    },
    Failed {
        errors: Vec<String>,
        raw: String,
    },
}

#[derive(Debug)]
struct Ai {
    prompt: String,
    limits: SnapshotLimits,
    include_source: bool,
    state: AiState,
    preview_json: Option<String>,
}

impl Default for Ai {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            limits: SnapshotLimits::default(),
            include_source: true,
            state: AiState::Idle,
            preview_json: None,
        }
    }
}

#[derive(Debug)]
enum SourceState {
    Loading(u64),
    Loaded(String),
    Failed(String),
}

#[derive(Debug)]
struct SourceView {
    class_name: String,
    swf_url: String,
    state: SourceState,
    open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Inspect,
    Ai,
    Mods,
    Settings,
}

#[derive(Default)]
pub struct Modkit {
    open: bool,
    tab: Tab,
    /// Paused from the Modkit window.
    paused: bool,
    /// `paused` was set by an AI request, not by the user, and ends with it.
    auto_paused: bool,
    picking: bool,
    pick_hover: Option<DisplayObjectHandle>,
    registry: Registry,
    selection: Vec<u32>,
    /// Selected object hovered in the window, highlighted on stage.
    hovered: Option<u32>,
    class_input: String,
    /// Game class names with their lowercase copies.
    class_names: Option<Vec<(String, String)>>,
    show_builtin: bool,
    undo: Vec<UndoBatch>,
    /// Cached `session_edits().len()`.
    edit_count: usize,
    locks: Vec<Lock>,
    status: String,
    ai: Ai,
    sources: Vec<SourceView>,
    settings: ModkitSettings,
    settings_dirty: bool,
    requests: Vec<HostRequest>,
    /// Current and requested player speed; the player syncs these.
    speed: f64,
    speed_request: Option<f64>,
    /// Trigger used by "Save as rule" buttons.
    rule_trigger: Trigger,
    /// Root movie URL the mods were installed for.
    mods_loaded_for: Option<String>,
    /// Selected objects should be handed to the new rules.
    adopt_pending: bool,
}

impl Modkit {
    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The player should not run while the user picks objects or has
    /// paused from the Modkit window.
    pub fn should_suspend_player(&self) -> bool {
        self.open && (self.paused || self.picking)
    }

    pub fn take_host_requests(&mut self) -> Vec<HostRequest> {
        std::mem::take(&mut self.requests)
    }

    pub fn push_host_response(&mut self, response: HostResponse) {
        match response {
            HostResponse::Ai { id, result } => self.on_ai_response(id, result),
            HostResponse::ClassSource { id, result } => {
                for view in &mut self.sources {
                    if matches!(view.state, SourceState::Loading(req) if req == id) {
                        view.state = match &result {
                            Ok(source) => SourceState::Loaded(source.clone()),
                            Err(e) => SourceState::Failed(e.clone()),
                        };
                    }
                }
            }
            HostResponse::Mods(result) => match result {
                Ok(message) => {
                    self.status = message;
                    self.adopt_pending = true;
                }
                Err(e) => self.status = format!("Mods: {e}"),
            },
            HostResponse::Status(message) => self.status = message,
        }
    }

    /// URL of the movie the mods were last installed for.
    pub fn mods_loaded_for(&self) -> Option<&str> {
        self.mods_loaded_for.as_deref()
    }

    pub fn set_mods_loaded_for(&mut self, url: Option<String>) {
        self.mods_loaded_for = url;
    }

    pub fn settings(&self) -> &ModkitSettings {
        &self.settings
    }

    /// Replaces the settings without marking them as changed.
    pub fn set_settings(&mut self, settings: ModkitSettings) {
        self.settings = settings;
        self.settings_dirty = false;
    }

    /// Returns the settings if the user changed them since the last call.
    pub fn take_changed_settings(&mut self) -> Option<ModkitSettings> {
        std::mem::take(&mut self.settings_dirty).then(|| self.settings.clone())
    }

    pub(crate) fn sync_speed(&mut self, current: f64) -> Option<f64> {
        self.speed = current;
        self.speed_request.take()
    }

    fn next_id(&mut self) -> u64 {
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    }

    fn refresh_edit_count(&mut self) {
        self.edit_count = self.session_edits().len();
    }

    fn selected_objects<'gc>(&self, dynamic_root: DynamicRootSet<'gc>) -> Vec<Object<'gc>> {
        self.selection
            .iter()
            .filter_map(|id| self.registry.fetch(dynamic_root, *id))
            .collect()
    }

    fn select(&mut self, id: u32, add: bool) {
        if !add {
            self.selection.clear();
            // A lock of an object that left the selection could not be released.
            self.locks.retain(|l| l.id == id);
        }
        if !self.selection.contains(&id) {
            self.selection.push(id);
        }
    }

    /// Writes one property, recording it for undo.
    ///
    /// Consecutive UI edits of the same property within a few passes (for
    /// example while dragging a value) merge into one undo step.
    fn edit<'gc>(
        &mut self,
        activation: &mut Activation<'_, 'gc>,
        edit: EditRequest,
        pass: Option<u64>,
    ) {
        let Some(object) = self
            .registry
            .fetch(activation.context.dynamic_root, edit.id)
        else {
            return;
        };
        let old = match inspect::read_prop(activation, object, &edit.access, edit.value.kind()) {
            Ok(old) => old,
            Err(e) => {
                self.status = format!("{}: {e}", edit.prop);
                return;
            }
        };
        if let Err(e) = inspect::write_prop(activation, object, &edit.access, &edit.value) {
            self.status = format!("{}: {e}", edit.prop);
            return;
        }
        // Setters can clamp; record what was actually stored.
        let new = inspect::read_prop(activation, object, &edit.access, edit.value.kind())
            .unwrap_or(edit.value);
        if let Some(lock) = self
            .locks
            .iter_mut()
            .find(|l| l.id == edit.id && l.prop == edit.prop)
        {
            lock.value = new.clone();
        }

        if let (Some(pass), Some(last)) = (pass, self.undo.last_mut())
            && let Some(last_pass) = last.last_pass
            && pass <= last_pass + 30
            && let [entry] = last.entries.as_mut_slice()
            && entry.id == edit.id
            && entry.prop == edit.prop
        {
            entry.new = new;
            last.last_pass = Some(pass);
            self.refresh_edit_count();
            return;
        }
        self.undo.push(UndoBatch {
            label: format!("#{} {}", edit.id, edit.prop),
            entries: vec![UndoEntry {
                id: edit.id,
                prop: edit.prop,
                access: edit.access,
                old,
                new,
            }],
            last_pass: pass,
        });
        self.refresh_edit_count();
    }

    fn undo<'gc>(&mut self, activation: &mut Activation<'_, 'gc>) {
        let Some(batch) = self.undo.pop() else {
            return;
        };
        let mut failed = 0;
        for entry in batch.entries.iter().rev() {
            let Some(object) = self
                .registry
                .fetch(activation.context.dynamic_root, entry.id)
            else {
                failed += 1;
                continue;
            };
            if inspect::write_prop(activation, object, &entry.access, &entry.old).is_err() {
                failed += 1;
            }
            if let Some(lock) = self
                .locks
                .iter_mut()
                .find(|l| l.id == entry.id && l.prop == entry.prop)
            {
                lock.value = entry.old.clone();
            }
        }
        self.status = if failed == 0 {
            format!("Undid {}", batch.label)
        } else {
            format!("Undid {} ({failed} writes failed)", batch.label)
        };
        self.refresh_edit_count();
    }

    /// Applies a validated patch as one undo step. Each property is looked
    /// up again first, so a patch never writes a property whose type changed
    /// since the snapshot.
    fn apply_changes<'gc>(
        &mut self,
        activation: &mut Activation<'_, 'gc>,
        changes: &[Change],
    ) -> Vec<UndoEntry> {
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        for change in changes {
            let Some(object) = self
                .registry
                .fetch(activation.context.dynamic_root, change.id)
            else {
                errors.push(format!("#{}: object is gone", change.id));
                continue;
            };
            let current = inspect::read_props(activation, object, true)
                .into_iter()
                .map(|r| r.entry)
                .find(|p| p.name == change.prop);
            let Some(prop) = current.filter(|p| p.is_editable()) else {
                errors.push(format!(
                    "#{} {}: no longer editable",
                    change.id, change.prop
                ));
                continue;
            };
            let (Some(kind), Some(old)) = (prop.kind, prop.value) else {
                continue;
            };
            if kind != change.new.kind() {
                errors.push(format!(
                    "#{} {}: type is now {}",
                    change.id,
                    change.prop,
                    kind.name()
                ));
                continue;
            }
            match inspect::write_prop(activation, object, &prop.access, &change.new) {
                Ok(()) => {
                    // Keep a lock on this property from reverting the change.
                    if let Some(lock) = self
                        .locks
                        .iter_mut()
                        .find(|l| l.id == change.id && l.prop == change.prop)
                    {
                        lock.value = change.new.clone();
                    }
                    entries.push(UndoEntry {
                        id: change.id,
                        prop: change.prop.clone(),
                        access: prop.access,
                        old,
                        new: change.new.clone(),
                    });
                }
                Err(e) => errors.push(format!("#{} {}: {e}", change.id, change.prop)),
            }
        }
        let applied = entries.len();
        let done = entries.clone();
        if !entries.is_empty() {
            self.undo.push(UndoBatch {
                label: format!("AI patch ({applied} changes)"),
                entries,
                last_pass: None,
            });
            self.refresh_edit_count();
        }
        self.status = if errors.is_empty() {
            format!("Applied {applied} changes")
        } else {
            format!("Applied {applied} changes; {}", errors.join("; "))
        };
        done
    }

    /// Queues rule files for patch entries that were applied: one rule per
    /// class and target, using the current trigger. Dynamic properties are
    /// skipped, rules only support declared ones.
    fn queue_rules_for<'gc>(
        &mut self,
        activation: &mut Activation<'_, 'gc>,
        applied: &[UndoEntry],
    ) {
        let dynamic_root = activation.context.dynamic_root;
        let mut rules: Vec<RuleSpec> = Vec::new();
        let mut skipped = 0;
        for entry in applied {
            let Some(object) = self.registry.fetch(dynamic_root, entry.id) else {
                continue;
            };
            let Some(class) = inspect::source_class_name(activation, object) else {
                skipped += 1;
                continue;
            };
            if matches!(entry.access, Access::Dynamic(_)) {
                skipped += 1;
                continue;
            }
            let target = if object.as_class_object().is_some() {
                Target::Static
            } else {
                Target::Instance
            };
            let Some(value) = entry.new.to_json_exact() else {
                skipped += 1;
                continue;
            };
            match rules
                .iter_mut()
                .find(|r| r.class == class && r.target == target)
            {
                Some(rule) => match rule.set.iter_mut().find(|(name, _)| *name == entry.prop) {
                    Some(slot) => slot.1 = value,
                    None => rule.set.push((entry.prop.clone(), value)),
                },
                None => rules.push(RuleSpec {
                    class,
                    on: self.rule_trigger,
                    target,
                    exact: false,
                    set: vec![(entry.prop.clone(), value)],
                }),
            }
        }
        let count = rules.len();
        for rule in rules {
            self.requests.push(HostRequest::SaveRule { rule });
        }
        if skipped > 0 {
            self.status.push_str(&format!(
                "; {skipped} not saved as rules (no game class, dynamic, or NaN/infinity)"
            ));
        }
        if count > 0 {
            self.status.push_str(&format!("; saving {count} rules"));
        }
    }

    /// Re-applies locked values. Called every UI pass.
    fn apply_locks<'gc>(&mut self, activation: &mut Activation<'_, 'gc>) {
        let dynamic_root = activation.context.dynamic_root;
        let registry = &self.registry;
        self.locks.retain(|lock| {
            let Some(object) = registry.fetch(dynamic_root, lock.id) else {
                return false;
            };
            let current = inspect::read_prop(activation, object, &lock.access, lock.value.kind());
            if current.as_ref() != Ok(&lock.value) {
                return inspect::write_prop(activation, object, &lock.access, &lock.value).is_ok();
            }
            true
        });
    }

    /// Pauses the game for the AI round trip, unless it is paused already.
    fn auto_pause(&mut self) {
        if self.settings.ai_auto_pause && !self.paused {
            self.paused = true;
            self.auto_paused = true;
        }
    }

    /// Ends a pause made by `auto_pause`. A pause the user chose stays.
    fn auto_resume(&mut self) {
        if std::mem::take(&mut self.auto_paused) {
            self.paused = false;
        }
    }

    /// One entry per property edited in this session, from its first old
    /// value to its last new one. Edits that were changed back are dropped.
    fn session_edits(&self) -> Vec<UndoEntry> {
        let mut net: Vec<UndoEntry> = Vec::new();
        for entry in self.undo.iter().flat_map(|batch| &batch.entries) {
            match net
                .iter_mut()
                .find(|e| e.id == entry.id && e.prop == entry.prop)
            {
                Some(existing) => existing.new = entry.new.clone(),
                None => net.push(entry.clone()),
            }
        }
        net.retain(|e| e.old != e.new);
        net
    }

    fn save_session_edits<'gc>(&mut self, activation: &mut Activation<'_, 'gc>) {
        let edits = self.session_edits();
        if edits.is_empty() {
            self.status = "No edits to save".into();
            return;
        }
        self.status = format!("{} edited properties", edits.len());
        self.queue_rules_for(activation, &edits);
    }

    fn start_ai_request<'gc>(&mut self, activation: &mut Activation<'_, 'gc>) {
        let roots = self.selected_objects(activation.context.dynamic_root);
        if roots.is_empty() {
            self.status = "Select at least one object first".into();
            return;
        }
        self.auto_pause();
        let snapshot =
            inspect::build_snapshot(activation, &roots, &self.ai.limits, &mut self.registry);
        let json = snapshot.to_llm_json(&self.ai.limits);
        let sources: Vec<(String, String)> = if self.ai.include_source {
            self.sources
                .iter()
                .filter_map(|s| match &s.state {
                    SourceState::Loaded(text) => Some((s.class_name.clone(), text.clone())),
                    _ => None,
                })
                .collect()
        } else {
            vec![]
        };
        let user = model::build_user_prompt(&self.ai.prompt, &json, &sources);
        let id = self.next_id();
        self.requests.push(HostRequest::Ai {
            id,
            system: model::SYSTEM_PROMPT.to_string(),
            user,
        });
        self.ai.state = AiState::Waiting {
            request_id: id,
            snapshot,
        };
    }

    /// Rejects, dismisses or cancels the AI patch, and ends the pause it made.
    fn dismiss_ai(&mut self) {
        self.ai.state = AiState::Idle;
        self.auto_resume();
    }

    /// Applies the ticked changes of the patch under review, and ends the
    /// pause the request made. With `as_rule` they are also queued as rules.
    fn apply_ai<'gc>(&mut self, activation: &mut Activation<'_, 'gc>, as_rule: bool) {
        let AiState::Review {
            changes, enabled, ..
        } = std::mem::take(&mut self.ai.state)
        else {
            return;
        };
        let chosen: Vec<_> = changes
            .into_iter()
            .zip(enabled)
            .filter_map(|(change, on)| on.then_some(change))
            .collect();
        let applied = self.apply_changes(activation, &chosen);
        if as_rule {
            self.queue_rules_for(activation, &applied);
        }
        self.auto_resume();
    }

    fn on_ai_response(&mut self, id: u64, result: Result<String, String>) {
        let snapshot = match std::mem::take(&mut self.ai.state) {
            AiState::Waiting {
                request_id,
                snapshot,
            } if request_id == id => snapshot,
            // Stale response (cancelled or superseded).
            other => {
                self.ai.state = other;
                return;
            }
        };
        self.ai.state = match result {
            Err(e) => AiState::Failed {
                errors: vec![e],
                raw: String::new(),
            },
            Ok(raw) => match model::parse_patch(&raw, &snapshot) {
                Ok(changes) => AiState::Review {
                    enabled: vec![true; changes.len()],
                    lines: model::describe_changes(&changes, &snapshot),
                    changes,
                    raw,
                },
                Err(errors) => AiState::Failed { errors, raw },
            },
        };
    }

    fn request_source<'gc>(&mut self, activation: &mut Activation<'_, 'gc>, object: Object<'gc>) {
        let Some(class_name) = inspect::source_class_name(activation, object) else {
            self.status = "Flash Player class, no game source".into();
            return;
        };
        let swf_url = inspect::source_swf_url(activation, object);
        if let Some(view) = self.sources.iter_mut().find(|s| s.class_name == class_name) {
            view.open = true;
            if !matches!(view.state, SourceState::Failed(_)) {
                return;
            }
        }
        let id = self.next_id();
        self.requests.push(HostRequest::ClassSource {
            id,
            class_name: class_name.clone(),
            swf_url: swf_url.clone(),
        });
        self.sources.retain(|s| s.class_name != class_name);
        self.sources.push(SourceView {
            class_name,
            swf_url,
            state: SourceState::Loading(id),
            open: true,
        });
    }

    pub(crate) fn draw_debug_rects<'gc>(
        &self,
        context: &mut RenderContext<'_, 'gc>,
        dynamic_root: DynamicRootSet<'gc>,
        world_matrix: Matrix,
    ) {
        if !self.open {
            return;
        }
        let draw = |context: &mut RenderContext<'_, 'gc>,
                    object: DisplayObject<'gc>,
                    color: Color,
                    thickness: f32| {
            let bounds = world_matrix * object.debug_rect_bounds();
            let thickness = Twips::from_pixels(thickness as f64);
            context.draw_rect_outline(color, bounds.grow(thickness), thickness);
        };
        for id in &self.selection {
            let Some(object) = self.registry.fetch(dynamic_root, *id) else {
                continue;
            };
            if let Some(display_object) = object.as_display_object() {
                let color = if self.hovered == Some(*id) {
                    Color::RED
                } else {
                    Color::CYAN
                };
                draw(context, display_object, color, 3.0);
            }
        }
        if self.picking
            && let Some(handle) = &self.pick_hover
        {
            draw(context, handle.fetch(dynamic_root), Color::YELLOW, 4.0);
        }
    }

    pub(crate) fn show<'gc>(
        &mut self,
        egui_ctx: &egui::Context,
        context: &mut UpdateContext<'gc>,
        movie_offset: f64,
    ) {
        if !self.open {
            self.picking = false;
            self.adopt_pending = false;
            // Locks hold while the window is closed.
            if !self.locks.is_empty() {
                let domain = context.avm2.stage_domain();
                let mut activation = Activation::from_domain(context, domain);
                self.apply_locks(&mut activation);
            }
            return;
        }
        let domain = context.avm2.stage_domain();
        let mut activation = Activation::from_domain(context, domain);
        self.apply_locks(&mut activation);
        if std::mem::take(&mut self.adopt_pending) {
            // Rules just (re)installed: let `every_frame` rules take hold of
            // the objects being edited, which were created before them.
            let mc = activation.gc();
            for object in self.selected_objects(activation.context.dynamic_root) {
                activation.avm2().mods.adopt(object, mc);
            }
        }
        if self.picking {
            self.update_picking(egui_ctx, &mut activation, movie_offset);
        }
        self.show_window(egui_ctx, &mut activation);
    }

    fn update_picking<'gc>(
        &mut self,
        egui_ctx: &egui::Context,
        activation: &mut Activation<'_, 'gc>,
        movie_offset: f64,
    ) {
        self.pick_hover = None;
        if egui_ctx.is_pointer_over_egui() {
            return;
        }
        let Some(point) = inspect::pointer_to_stage(activation.context, egui_ctx, movie_offset)
        else {
            return;
        };
        let Some((object, display_object)) = inspect::pick(activation, point) else {
            return;
        };
        self.pick_hover = Some(DisplayObjectHandle::new(activation.context, display_object));
        let (clicked, shift) = egui_ctx.input(|i| (i.pointer.primary_clicked(), i.modifiers.shift));
        if clicked {
            let id = self.registry.id_for(activation.context, object);
            self.select(id, shift);
            // Shift keeps picking so several objects can be added in a row.
            if !shift {
                self.picking = false;
                self.pick_hover = None;
            }
        }
    }
}

/// A property write requested by the UI.
#[derive(Debug, Clone)]
struct EditRequest {
    id: u32,
    prop: String,
    access: Access,
    value: PropValue,
}
