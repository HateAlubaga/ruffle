//! Frontend side of Ruffle Modkit: settings persistence, the Modkit menu,
//! LLM requests, the FFDec bridge and mod files.
//!
//! The core only queues `HostRequest`s; everything that touches the network,
//! other processes or the file system happens here.

mod ai;
mod ffdec;
mod mods;

use egui::{Button, Key, KeyboardShortcut, Modifiers, Widget};
use ruffle_core::Player;
use ruffle_core::modkit::mods::RuleSpec;
use ruffle_core::modkit::{
    AiProvider, HostRequest, HostResponse, MAX_SPEED, MIN_SPEED, ModkitSettings,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

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

/// How often the mods folder is checked for edits.
const MODS_POLL_EVERY: Duration = Duration::from_secs(1);

pub struct ModkitHost {
    config_dir: PathBuf,
    settings: ModkitSettings,
    settings_path: PathBuf,
    responses_tx: Sender<HostResponse>,
    responses_rx: Receiver<HostResponse>,
    hashes: mods::HashCache,
    mods_polled: Instant,
    /// State of the mods folder at the last install or poll.
    mods_seen: Option<mods::Fingerprint>,
}

/// Installs the mods for a movie that was just requested. Call it right after
/// `fetch_root_movie`, before the first frame.
pub fn install_initial(player: &mut Player, config_dir: &Path, movie_url: &str) {
    let loaded = mods::reload(
        player,
        config_dir,
        &mut mods::HashCache::default(),
        movie_url,
    );
    if loaded.files > 0 {
        tracing::info!(
            "Loaded {} mod file(s) from {}",
            loaded.files,
            mods::mods_dir(config_dir).display()
        );
    }
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
            config_dir,
            settings,
            settings_path,
            responses_tx,
            responses_rx,
            hashes: mods::HashCache::default(),
            mods_polled: Instant::now(),
            mods_seen: None,
        }
    }

    /// Syncs settings, starts queued requests and delivers finished ones.
    /// Call once per UI frame after `Player::show_debug_ui`.
    pub fn pump(&mut self, player: &mut Player) {
        let (requests, movie_url) = {
            let mut debug_ui = player.debug_ui();
            let modkit = debug_ui.modkit();
            if let Some(changed) = modkit.take_changed_settings() {
                self.settings = changed;
                if let Err(e) =
                    std::fs::write(&self.settings_path, settings_to_json(&self.settings))
                {
                    tracing::warn!("Couldn't save Modkit settings: {e}");
                }
            } else if modkit.settings() != &self.settings {
                // A new player starts with default settings.
                modkit.set_settings(self.settings.clone());
            }
            (
                modkit.take_host_requests(),
                modkit.mods_loaded_for().map(str::to_owned),
            )
        };

        // Mod operations reinstall the mods, which needs the player without
        // the debug UI borrowed.
        let mut responses = Vec::new();
        for request in requests {
            let result = match request {
                HostRequest::SaveRule { rule } => {
                    self.with_movie(&movie_url, |host, url| host.save_rule(player, url, &rule))
                }
                HostRequest::RemoveRule { file, rule } => self
                    .with_movie(&movie_url, |host, url| {
                        host.remove_rule(player, url, &file, &rule)
                    }),
                HostRequest::ReloadMods => self.with_movie(&movie_url, |host, url| {
                    Ok(host.reload(player, url, "Reloaded mods"))
                }),
                other => {
                    self.start(other);
                    continue;
                }
            };
            responses.push(HostResponse::Mods(result));
        }
        if let Some(url) = &movie_url
            && let Some(response) = self.poll_mods(player, url)
        {
            responses.push(response);
        }

        let mut debug_ui = player.debug_ui();
        let modkit = debug_ui.modkit();
        for response in responses {
            modkit.push_host_response(response);
        }
        while let Ok(response) = self.responses_rx.try_recv() {
            modkit.push_host_response(response);
        }
    }

    fn with_movie(
        &mut self,
        movie_url: &Option<String>,
        action: impl FnOnce(&mut Self, &str) -> Result<String, String>,
    ) -> Result<String, String> {
        match movie_url {
            Some(url) => action(self, url),
            None => Err("no movie is loaded".to_string()),
        }
    }

    fn mods_dir(&self) -> PathBuf {
        mods::mods_dir(&self.config_dir)
    }

    fn reload(&mut self, player: &mut Player, movie_url: &str, what: &str) -> String {
        let loaded = mods::reload(player, &self.config_dir, &mut self.hashes, movie_url);
        self.mods_seen = Some(loaded.fingerprint);
        self.mods_polled = Instant::now();
        format!("{what} ({} mod file(s))", loaded.files)
    }

    fn save_rule(
        &mut self,
        player: &mut Player,
        movie_url: &str,
        rule: &RuleSpec,
    ) -> Result<String, String> {
        let path = ffdec::swf_path(movie_url)
            .map_err(|e| format!("a rule needs a mod file bound to a local SWF, but {e}"))?;
        let sha = self.hashes.hash(&path)?;
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let saved = mods::save_rule(&self.mods_dir(), &stem, &sha, rule)?;
        let what = format!(
            "{} rule for {} in {}",
            if saved.replaced { "Updated" } else { "Saved" },
            rule.normalized_class(),
            saved.file
        );
        Ok(self.reload(player, movie_url, &what))
    }

    fn remove_rule(
        &mut self,
        player: &mut Player,
        movie_url: &str,
        file: &str,
        rule: &RuleSpec,
    ) -> Result<String, String> {
        mods::remove_rule(&self.mods_dir(), file, rule)?;
        let what = format!("Removed rule for {} from {file}", rule.normalized_class());
        Ok(self.reload(player, movie_url, &what))
    }

    /// Reloads the mods when a file in the mods folder was edited.
    fn poll_mods(&mut self, player: &mut Player, movie_url: &str) -> Option<HostResponse> {
        if self.mods_polled.elapsed() < MODS_POLL_EVERY {
            return None;
        }
        self.mods_polled = Instant::now();
        let now = mods::fingerprint(&self.mods_dir());
        match &self.mods_seen {
            Some(seen) if *seen == now => None,
            // The first look only records the state the mods were loaded in.
            None => {
                self.mods_seen = Some(now);
                None
            }
            Some(_) => Some(HostResponse::Mods(Ok(self.reload(
                player,
                movie_url,
                "Mod files changed, reloaded",
            )))),
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
            HostRequest::SaveRule { .. }
            | HostRequest::RemoveRule { .. }
            | HostRequest::ReloadMods => {
                unreachable!("mod requests are handled by pump, they need the player")
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
        "ai_auto_pause": settings.ai_auto_pause,
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
        ai_auto_pause: json
            .get("ai_auto_pause")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
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
            ai_auto_pause: false,
        };
        let json = settings_to_json(&settings);
        assert_eq!(settings_from_json(&json), Some(settings));
        assert_eq!(settings_from_json("{}"), Some(ModkitSettings::default()));
        assert_eq!(settings_from_json("nope"), None);
    }
}
