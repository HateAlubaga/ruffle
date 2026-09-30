//! Frontend side of Ruffle Modkit: settings persistence, the Modkit menu,
//! LLM requests and the FFDec bridge.
//!
//! The core only queues `HostRequest`s; everything that touches the network,
//! other processes or the file system happens here.

mod ai;
mod ffdec;

use egui::{Button, Key, KeyboardShortcut, Modifiers, Widget};
use ruffle_core::Player;
use ruffle_core::modkit::{
    AiProvider, HostRequest, HostResponse, MAX_SPEED, MIN_SPEED, ModkitSettings,
};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

const SETTINGS_FILE: &str = "modkit.json";

const SHORTCUT_OPEN: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::M);
const SHORTCUT_SLOWER: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND, Key::OpenBracket);
const SHORTCUT_FASTER: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND, Key::CloseBracket);
const SHORTCUT_NORMAL_SPEED: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND, Key::Backslash);

const SPEED_STEPS: &[f64] = &[0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0];

pub struct ModkitHost {
    settings: ModkitSettings,
    settings_path: PathBuf,
    responses_tx: Sender<HostResponse>,
    responses_rx: Receiver<HostResponse>,
}

impl ModkitHost {
    pub fn new(config_dir: PathBuf) -> Self {
        let settings_path = config_dir.join(SETTINGS_FILE);
        let settings = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|text| settings_from_json(&text))
            .unwrap_or_default();
        let (responses_tx, responses_rx) = channel();
        Self {
            settings,
            settings_path,
            responses_tx,
            responses_rx,
        }
    }

    /// Syncs settings, starts queued requests and delivers finished ones.
    /// Call once per UI frame after `Player::show_debug_ui`.
    pub fn pump(&mut self, player: &mut Player) {
        let mut debug_ui = player.debug_ui();
        let modkit = debug_ui.modkit();

        if let Some(changed) = modkit.take_changed_settings() {
            self.settings = changed;
            if let Err(e) = std::fs::write(&self.settings_path, settings_to_json(&self.settings)) {
                tracing::warn!("Couldn't save Modkit settings: {e}");
            }
        } else if modkit.settings() != &self.settings {
            // A new player starts with default settings.
            modkit.set_settings(self.settings.clone());
        }

        for request in modkit.take_host_requests() {
            self.start(request);
        }
        while let Ok(response) = self.responses_rx.try_recv() {
            modkit.push_host_response(response);
        }
    }

    fn start(&self, request: HostRequest) {
        let tx = self.responses_tx.clone();
        match request {
            HostRequest::Ai { id, system, user } => {
                let provider = ai::provider_from_settings(&self.settings);
                tokio::spawn(async move {
                    let result = match provider {
                        Ok(provider) => provider.complete(system, user).await,
                        Err(e) => Err(e),
                    };
                    let _ = tx.send(HostResponse::Ai { id, result });
                });
            }
            HostRequest::ClassSource {
                id,
                class_name,
                swf_url,
            } => {
                let ffdec_path = self.settings.ffdec_path.clone();
                tokio::task::spawn_blocking(move || {
                    let result = ffdec::export_class_source(&ffdec_path, &swf_url, &class_name);
                    let _ = tx.send(HostResponse::ClassSource { id, result });
                });
            }
            HostRequest::OpenInFfdec {
                class_name,
                swf_url,
            } => {
                if let Err(e) = ffdec::open_gui(&self.settings.ffdec_path, &swf_url) {
                    tracing::warn!("Couldn't open {class_name} in FFDec: {e}");
                }
            }
        }
    }
}

fn settings_to_json(settings: &ModkitSettings) -> String {
    let json = serde_json::json!({
        "ffdec_path": settings.ffdec_path,
        "ai_provider": settings.ai_provider.id(),
        "ai_base_url": settings.ai_base_url,
        "ai_model": settings.ai_model,
        "ai_api_key": settings.ai_api_key,
    });
    serde_json::to_string_pretty(&json).unwrap_or_default()
}

fn settings_from_json(text: &str) -> Option<ModkitSettings> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let string = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(ModkitSettings {
        ffdec_path: string("ffdec_path"),
        ai_provider: AiProvider::from_id(&string("ai_provider")).unwrap_or_default(),
        ai_base_url: string("ai_base_url"),
        ai_model: string("ai_model"),
        ai_api_key: string("ai_api_key"),
    })
}

fn step_speed(player: &mut Player, up: bool) {
    let current = player.speed();
    let next = if up {
        SPEED_STEPS.iter().copied().find(|s| *s > current + 1e-9)
    } else {
        SPEED_STEPS
            .iter()
            .rev()
            .copied()
            .find(|s| *s < current - 1e-9)
    };
    if let Some(next) = next {
        player.set_speed(next);
    }
}

pub fn consume_shortcuts(egui_ctx: &egui::Context, player: Option<&mut Player>) {
    let Some(player) = player else {
        return;
    };
    if egui_ctx.input_mut(|input| input.consume_shortcut(&SHORTCUT_OPEN)) {
        player.debug_ui().modkit().open();
    }
    if egui_ctx.input_mut(|input| input.consume_shortcut(&SHORTCUT_SLOWER)) {
        step_speed(player, false);
    }
    if egui_ctx.input_mut(|input| input.consume_shortcut(&SHORTCUT_FASTER)) {
        step_speed(player, true);
    }
    if egui_ctx.input_mut(|input| input.consume_shortcut(&SHORTCUT_NORMAL_SPEED)) {
        player.set_speed(1.0);
    }
}

/// The "Modkit" menu in the menu bar.
pub fn menu(ui: &mut egui::Ui, player: &mut Option<&mut Player>) {
    ui.menu_button("Modkit", |ui| {
        ui.add_enabled_ui(player.is_some(), |ui| {
            if Button::new("Open Modkit window")
                .shortcut_text(ui.ctx().format_shortcut(&SHORTCUT_OPEN))
                .ui(ui)
                .clicked()
            {
                ui.close();
                if let Some(player) = player {
                    player.debug_ui().modkit().open();
                }
            }
            ui.separator();
            let current = player.as_ref().map(|p| p.speed()).unwrap_or(1.0);
            ui.label(format!("Speed ×{current} ({MIN_SPEED}–{MAX_SPEED})"));
            for &speed in SPEED_STEPS {
                let label = format!("×{speed}");
                let mut button = Button::selectable((current - speed).abs() < 1e-9, label);
                if speed == 1.0 {
                    button = button.shortcut_text(ui.ctx().format_shortcut(&SHORTCUT_NORMAL_SPEED));
                }
                if button.ui(ui).clicked()
                    && let Some(player) = player
                {
                    player.set_speed(speed);
                }
            }
            ui.weak(format!(
                "{} / {} to step",
                ui.ctx().format_shortcut(&SHORTCUT_SLOWER),
                ui.ctx().format_shortcut(&SHORTCUT_FASTER)
            ));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let settings = ModkitSettings {
            ffdec_path: r"C:\FFDec\ffdec-cli.exe".into(),
            ai_provider: AiProvider::Anthropic,
            ai_base_url: "https://example.com".into(),
            ai_model: "m".into(),
            ai_api_key: "k".into(),
        };
        let json = settings_to_json(&settings);
        assert_eq!(settings_from_json(&json), Some(settings));
        assert_eq!(settings_from_json("{}"), Some(ModkitSettings::default()));
        assert_eq!(settings_from_json("nope"), None);
    }
}
