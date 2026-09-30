//! The Modkit egui window.
//!
//! Drawing only collects [`Action`]s; they run after the window is drawn so
//! the UI code never needs mutable access to both `Modkit` and the AVM.

use crate::avm2::{Activation, Object};
use crate::modkit::inspect::{self, ReadProp};
use crate::modkit::model::{Access, PropEntry, PropValue, describe_changes};
use crate::modkit::{
    AiProvider, AiState, EditRequest, HostRequest, Lock, MAX_SPEED, MIN_SPEED, Modkit, SourceState,
    Tab,
};
use egui::{
    Align, CollapsingHeader, Color32, ComboBox, DragValue, Grid, Layout, RichText, ScrollArea,
    TextEdit, Ui, Window,
};

enum Action<'gc> {
    Edit(EditRequest),
    ToggleLock {
        id: u32,
        prop: String,
        access: Access,
        value: PropValue,
    },
    Select(Object<'gc>),
    Remove(u32),
    ClearSelection,
    ShowSource(u32),
    OpenInFfdec(u32),
    AddClass(String),
    Undo,
    StartAi,
    CancelAi,
    ApplyAi,
    RejectAi,
    PreviewJson,
    LoadClassNames,
}

/// One selected object, read for this pass.
struct ObjectView<'gc> {
    id: u32,
    label: String,
    has_source: bool,
    props: Vec<ReadProp<'gc>>,
}

const SPEED_PRESETS: &[f64] = &[0.25, 0.5, 1.0, 2.0, 4.0];

impl Modkit {
    pub(super) fn show_window<'gc>(
        &mut self,
        egui_ctx: &egui::Context,
        activation: &mut Activation<'_, 'gc>,
    ) {
        let pass = egui_ctx.cumulative_pass_nr();
        let views = if self.tab == Tab::Inspect {
            self.read_views(activation)
        } else {
            vec![]
        };
        let mut actions = Vec::new();
        let mut open = self.open;
        Window::new("Modkit")
            .open(&mut open)
            .default_size([420.0, 520.0])
            .show(egui_ctx, |ui| {
                self.show_toolbar(ui, &mut actions);
                ui.separator();
                match self.tab {
                    Tab::Inspect => self.show_inspect(ui, &views, &mut actions),
                    Tab::Ai => self.show_ai(ui, &mut actions),
                    Tab::Settings => self.show_settings(ui),
                }
                if !self.status.is_empty() {
                    ui.separator();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(&self.status).small());
                    });
                }
            });
        self.open = open;
        self.show_sources(egui_ctx);

        for action in actions {
            self.run_action(activation, action, pass);
        }
    }

    fn read_views<'gc>(&mut self, activation: &mut Activation<'_, 'gc>) -> Vec<ObjectView<'gc>> {
        let dynamic_root = activation.context.dynamic_root;
        let mut views = Vec::new();
        for &id in &self.selection {
            let Some(object) = self.registry.fetch(dynamic_root, id) else {
                continue;
            };
            let mut label = format!("#{id} {}", inspect::class_label(activation, object));
            if let Some(name) = inspect::instance_name(object) {
                label.push_str(&format!(" \"{name}\""));
            }
            views.push(ObjectView {
                id,
                label,
                has_source: inspect::source_class_name(activation, object).is_some(),
                props: inspect::read_props(activation, object),
            });
        }
        views
    }

    fn show_toolbar(&mut self, ui: &mut Ui, actions: &mut Vec<Action<'_>>) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Inspect, "Inspect");
            ui.selectable_value(&mut self.tab, Tab::Ai, "AI edit");
            ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
        });
        ui.horizontal(|ui| {
            ui.toggle_value(&mut self.paused, "⏸ Pause");
            let pick_label = if self.picking {
                "Picking… (Esc)"
            } else {
                "🎯 Pick"
            };
            if ui
                .toggle_value(&mut self.picking, pick_label)
                .on_hover_text("Click an object on stage. Shift+click adds to the selection.")
                .changed()
                && !self.picking
            {
                self.pick_hover = None;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.picking = false;
            }
            let undo = ui.add_enabled(!self.undo.is_empty(), egui::Button::new("↶ Undo"));
            let undo = match self.undo.last() {
                Some(batch) => undo.on_hover_text(format!("Undo {}", batch.label)),
                None => undo,
            };
            if undo.clicked() {
                actions.push(Action::Undo);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Speed");
            for &preset in SPEED_PRESETS {
                if ui
                    .selectable_label(self.speed == preset, format!("×{preset}"))
                    .clicked()
                {
                    self.speed_request = Some(preset);
                }
            }
            let mut speed = self.speed;
            if ui
                .add(
                    DragValue::new(&mut speed)
                        .range(MIN_SPEED..=MAX_SPEED)
                        .speed(0.01)
                        .prefix("×"),
                )
                .changed()
            {
                self.speed_request = Some(speed);
            }
        });
    }

    fn show_inspect<'gc>(
        &mut self,
        ui: &mut Ui,
        views: &[ObjectView<'gc>],
        actions: &mut Vec<Action<'gc>>,
    ) {
        ui.horizontal(|ui| {
            let response = ui.add(
                TextEdit::singleline(&mut self.class_input)
                    .hint_text("Class for static vars, e.g. Stats")
                    .desired_width(200.0),
            );
            if response.gained_focus() && self.class_names.is_none() {
                actions.push(Action::LoadClassNames);
            }
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Add class").clicked() || enter {
                actions.push(Action::AddClass(self.class_input.clone()));
            }
        });
        if let Some(names) = &self.class_names
            && !self.class_input.is_empty()
        {
            let needle = self.class_input.to_ascii_lowercase();
            let matches: Vec<&String> = names
                .iter()
                .filter(|n| n.to_ascii_lowercase().contains(&needle) && **n != self.class_input)
                .take(8)
                .collect();
            if !matches.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for name in matches {
                        if ui.small_button(name).clicked() {
                            self.class_input = name.clone();
                        }
                    }
                });
            }
        }
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut self.show_builtin,
                "Show Flash properties (x, alpha, …)",
            );
            if !self.selection.is_empty() && ui.small_button("Clear").clicked() {
                actions.push(Action::ClearSelection);
            }
        });
        if views.is_empty() {
            ui.weak("Nothing selected. Use Pick, or add a class for its static vars.");
            return;
        }

        self.hovered = None;
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for view in views {
                    let header = CollapsingHeader::new(&view.label)
                        .id_salt(("modkit_object", view.id))
                        .default_open(true)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if view.has_source {
                                    if ui.small_button("Show class source").clicked() {
                                        actions.push(Action::ShowSource(view.id));
                                    }
                                    if ui.small_button("Open in FFDec").clicked() {
                                        actions.push(Action::OpenInFfdec(view.id));
                                    }
                                }
                                if ui.small_button("Remove").clicked() {
                                    actions.push(Action::Remove(view.id));
                                }
                            });
                            self.show_props(ui, view, actions);
                        });
                    if header.header_response.hovered() {
                        self.hovered = Some(view.id);
                    }
                }
            });
    }

    fn show_props<'gc>(&self, ui: &mut Ui, view: &ObjectView<'gc>, actions: &mut Vec<Action<'gc>>) {
        Grid::new(("modkit_props", view.id))
            .striped(true)
            .num_columns(3)
            .show(ui, |ui| {
                for ReadProp { entry, target } in &view.props {
                    if entry.builtin && !self.show_builtin {
                        continue;
                    }
                    let name = ui.label(&entry.name);
                    name.on_hover_text(match entry.kind {
                        Some(kind) => kind.name(),
                        None => "read-only",
                    });
                    self.show_value(ui, view.id, entry, *target, actions);
                    if entry.is_editable()
                        && let Some(value) = &entry.value
                    {
                        let mut locked = self
                            .locks
                            .iter()
                            .any(|l| l.id == view.id && l.prop == entry.name);
                        if ui
                            .checkbox(&mut locked, "")
                            .on_hover_text("Lock: keep this value")
                            .changed()
                        {
                            actions.push(Action::ToggleLock {
                                id: view.id,
                                prop: entry.name.clone(),
                                access: entry.access.clone(),
                                value: value.clone(),
                            });
                        }
                    } else {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });
    }

    fn show_value<'gc>(
        &self,
        ui: &mut Ui,
        id: u32,
        entry: &PropEntry,
        target: Option<Object<'gc>>,
        actions: &mut Vec<Action<'gc>>,
    ) {
        let (true, Some(value)) = (entry.is_editable(), &entry.value) else {
            if let Some(target) = target {
                if ui
                    .link(&entry.display)
                    .on_hover_text("Add to selection")
                    .clicked()
                {
                    actions.push(Action::Select(target));
                }
            } else {
                ui.weak(&entry.display);
            }
            return;
        };
        let new_value = match value {
            PropValue::Int(v) => {
                let mut v = *v;
                ui.add(DragValue::new(&mut v))
                    .changed()
                    .then_some(PropValue::Int(v))
            }
            PropValue::Uint(v) => {
                let mut v = *v;
                ui.add(DragValue::new(&mut v))
                    .changed()
                    .then_some(PropValue::Uint(v))
            }
            PropValue::Number(v) => {
                let mut v = *v;
                ui.add(DragValue::new(&mut v).speed(0.1).max_decimals(6))
                    .changed()
                    .then_some(PropValue::Number(v))
            }
            PropValue::Boolean(v) => {
                let mut v = *v;
                ui.checkbox(&mut v, "")
                    .changed()
                    .then_some(PropValue::Boolean(v))
            }
            PropValue::String(v) => {
                let mut v = v.clone();
                ui.add(TextEdit::singleline(&mut v).desired_width(160.0))
                    .changed()
                    .then_some(PropValue::String(v))
            }
        };
        if let Some(value) = new_value {
            actions.push(Action::Edit(EditRequest {
                id,
                prop: entry.name.clone(),
                access: entry.access.clone(),
                value,
            }));
        }
    }

    fn show_ai(&mut self, ui: &mut Ui, actions: &mut Vec<Action<'_>>) {
        ui.label(format!(
            "Selected objects: {}. Pause the game, pick objects, then describe the change.",
            self.selection.len()
        ));
        ui.add(
            TextEdit::multiline(&mut self.ai.prompt)
                .hint_text("e.g. make the boss twice as slow and give the hero 10x damage")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        CollapsingHeader::new("Snapshot limits")
            .id_salt("modkit_ai_limits")
            .show(ui, |ui| {
                let limits = &mut self.ai.limits;
                Grid::new("modkit_limits").show(ui, |ui| {
                    ui.label("Follow references (depth)");
                    ui.add(DragValue::new(&mut limits.max_depth).range(0..=4));
                    ui.end_row();
                    ui.label("Max objects");
                    ui.add(DragValue::new(&mut limits.max_objects).range(1..=500));
                    ui.end_row();
                    ui.label("Max properties per object");
                    ui.add(DragValue::new(&mut limits.max_props_per_object).range(1..=1000));
                    ui.end_row();
                    ui.label("Max string length");
                    ui.add(DragValue::new(&mut limits.max_string_len).range(8..=10_000));
                    ui.end_row();
                    ui.label("Max JSON bytes");
                    ui.add(
                        DragValue::new(&mut limits.max_json_bytes)
                            .range(1_000..=1_000_000)
                            .speed(100),
                    );
                    ui.end_row();
                });
                ui.checkbox(
                    &mut self.ai.include_source,
                    "Include loaded class sources (from 'Show class source')",
                );
            });

        let waiting = matches!(self.ai.state, AiState::Waiting { .. });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !waiting && !self.selection.is_empty() && !self.ai.prompt.trim().is_empty(),
                    egui::Button::new("Send to AI"),
                )
                .clicked()
            {
                actions.push(Action::StartAi);
            }
            if ui
                .add_enabled(
                    !self.selection.is_empty(),
                    egui::Button::new("Preview JSON"),
                )
                .clicked()
            {
                actions.push(Action::PreviewJson);
            }
            if waiting {
                ui.spinner();
                if ui.button("Cancel").clicked() {
                    actions.push(Action::CancelAi);
                }
            }
        });

        if let Some(json) = &mut self.ai.preview_json {
            CollapsingHeader::new(format!("Snapshot JSON ({} bytes)", json.len()))
                .id_salt("modkit_ai_json")
                .show(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("modkit_ai_json_scroll")
                        .max_height(200.0)
                        .show(ui, |ui| {
                            ui.add(
                                TextEdit::multiline(&mut json.as_str())
                                    .code_editor()
                                    .desired_width(f32::INFINITY),
                            );
                        });
                });
        }

        match &mut self.ai.state {
            AiState::Idle | AiState::Waiting { .. } => {}
            AiState::Failed { errors, raw } => {
                ui.colored_label(ui.visuals().error_fg_color, "Patch rejected:");
                for error in errors.iter() {
                    ui.label(format!("• {error}"));
                }
                show_raw(ui, raw);
                if ui.button("Dismiss").clicked() {
                    actions.push(Action::RejectAi);
                }
            }
            AiState::Review {
                snapshot,
                changes,
                enabled,
                raw,
            } => {
                if changes.is_empty() {
                    ui.label("The AI proposed no changes.");
                } else {
                    ui.strong("Proposed changes");
                    let lines = describe_changes(changes, snapshot);
                    ScrollArea::vertical()
                        .id_salt("modkit_ai_diff")
                        .max_height(240.0)
                        .show(ui, |ui| {
                            for (line, on) in lines.iter().zip(enabled.iter_mut()) {
                                ui.checkbox(on, RichText::new(line).monospace());
                            }
                        });
                }
                show_raw(ui, raw);
                ui.horizontal(|ui| {
                    let any = enabled.iter().any(|e| *e);
                    if ui.add_enabled(any, egui::Button::new("Apply")).clicked() {
                        actions.push(Action::ApplyAi);
                    }
                    if ui.button("Reject").clicked() {
                        actions.push(Action::RejectAi);
                    }
                });
            }
        }
    }

    fn show_settings(&mut self, ui: &mut Ui) {
        let before = self.settings.clone();
        let settings = &mut self.settings;
        Grid::new("modkit_settings").num_columns(2).show(ui, |ui| {
            ui.label("FFDec path");
            ui.add(
                TextEdit::singleline(&mut settings.ffdec_path)
                    .hint_text("…\\ffdec-cli.exe or ffdec.jar")
                    .desired_width(260.0),
            );
            ui.end_row();

            ui.label("AI provider");
            ComboBox::from_id_salt("modkit_provider")
                .selected_text(match settings.ai_provider {
                    AiProvider::OpenAiCompatible => "OpenAI-compatible",
                    AiProvider::Anthropic => "Anthropic",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut settings.ai_provider,
                        AiProvider::OpenAiCompatible,
                        "OpenAI-compatible",
                    );
                    ui.selectable_value(
                        &mut settings.ai_provider,
                        AiProvider::Anthropic,
                        "Anthropic",
                    );
                });
            ui.end_row();

            ui.label("Base URL");
            ui.add(
                TextEdit::singleline(&mut settings.ai_base_url)
                    .hint_text(settings.ai_provider.default_base_url())
                    .desired_width(260.0),
            );
            ui.end_row();

            ui.label("Model");
            ui.add(TextEdit::singleline(&mut settings.ai_model).desired_width(260.0));
            ui.end_row();

            ui.label("API key");
            ui.add(
                TextEdit::singleline(&mut settings.ai_api_key)
                    .password(true)
                    .hint_text(format!("empty: ${}", settings.ai_provider.key_env_var()))
                    .desired_width(260.0),
            );
            ui.end_row();
        });
        ui.with_layout(Layout::top_down(Align::Min), |ui| {
            ui.weak(
                "Settings are saved by the frontend. The key is only sent to the base URL, \
                 and only when you press 'Send to AI'.",
            );
        });
        if self.settings != before {
            self.settings_dirty = true;
        }
    }

    fn show_sources(&mut self, egui_ctx: &egui::Context) {
        for view in &mut self.sources {
            if !view.open {
                continue;
            }
            let mut open = true;
            let mut open_in_ffdec = false;
            Window::new(format!("Source: {}", view.class_name))
                .id(egui::Id::new(("modkit_source", &view.class_name)))
                .open(&mut open)
                .default_size([560.0, 480.0])
                .show(egui_ctx, |ui| {
                    if ui.small_button("Open in FFDec").clicked() {
                        open_in_ffdec = true;
                    }
                    match &view.state {
                        SourceState::Loading(_) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Decompiling with FFDec…");
                            });
                        }
                        SourceState::Failed(e) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        SourceState::Loaded(text) => {
                            ScrollArea::both()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.add(
                                        TextEdit::multiline(&mut text.as_str())
                                            .code_editor()
                                            .desired_width(f32::INFINITY),
                                    );
                                });
                        }
                    }
                });
            view.open = open;
            if open_in_ffdec {
                self.requests.push(HostRequest::OpenInFfdec {
                    class_name: view.class_name.clone(),
                    swf_url: view.swf_url.clone(),
                });
            }
        }
    }

    fn run_action<'gc>(
        &mut self,
        activation: &mut Activation<'_, 'gc>,
        action: Action<'gc>,
        pass: u64,
    ) {
        let dynamic_root = activation.context.dynamic_root;
        match action {
            Action::Edit(edit) => self.edit(activation, edit, Some(pass)),
            Action::ToggleLock {
                id,
                prop,
                access,
                value,
            } => {
                let before = self.locks.len();
                self.locks.retain(|l| !(l.id == id && l.prop == prop));
                if self.locks.len() == before {
                    self.locks.push(Lock {
                        id,
                        prop,
                        access,
                        value,
                    });
                }
            }
            Action::Select(object) => {
                let id = self.registry.id_for(activation.context, object);
                self.select(id, true);
            }
            Action::Remove(id) => self.selection.retain(|s| *s != id),
            Action::ClearSelection => self.selection.clear(),
            Action::ShowSource(id) => {
                if let Some(object) = self.registry.fetch(dynamic_root, id) {
                    self.request_source(activation, object);
                }
            }
            Action::OpenInFfdec(id) => {
                if let Some(object) = self.registry.fetch(dynamic_root, id)
                    && let Some(class_name) = inspect::source_class_name(activation, object)
                {
                    self.requests.push(HostRequest::OpenInFfdec {
                        class_name,
                        swf_url: inspect::source_swf_url(activation, object),
                    });
                }
            }
            Action::AddClass(name) => match inspect::find_class_object(activation, &name) {
                Some(object) => {
                    let id = self.registry.id_for(activation.context, object);
                    self.select(id, true);
                    self.status.clear();
                }
                None => self.status = format!("No game class named {name:?}"),
            },
            Action::LoadClassNames => {
                self.class_names = Some(inspect::game_class_names(activation));
            }
            Action::Undo => self.undo(activation),
            Action::StartAi => self.start_ai_request(activation),
            Action::CancelAi | Action::RejectAi => self.ai.state = AiState::Idle,
            Action::ApplyAi => {
                if let AiState::Review {
                    changes, enabled, ..
                } = std::mem::take(&mut self.ai.state)
                {
                    let chosen: Vec<_> = changes
                        .into_iter()
                        .zip(enabled)
                        .filter_map(|(c, on)| on.then_some(c))
                        .collect();
                    self.apply_changes(activation, &chosen);
                }
            }
            Action::PreviewJson => {
                let roots = self.selected_objects(dynamic_root);
                let snapshot = inspect::build_snapshot(
                    activation,
                    &roots,
                    &self.ai.limits,
                    &mut self.registry,
                );
                self.ai.preview_json = Some(snapshot.to_llm_json(&self.ai.limits));
            }
        }
    }
}

fn show_raw(ui: &mut Ui, raw: &str) {
    if raw.is_empty() {
        return;
    }
    CollapsingHeader::new("Raw AI response")
        .id_salt("modkit_ai_raw")
        .show(ui, |ui| {
            ScrollArea::vertical()
                .id_salt("modkit_ai_raw_scroll")
                .max_height(160.0)
                .show(ui, |ui| {
                    ui.label(RichText::new(raw).monospace().color(Color32::GRAY));
                });
        });
}
