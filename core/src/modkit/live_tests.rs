//! End-to-end checks against a real game, run headless.
//!
//! Needs a SWF, so it only runs when `RUFFLE_MODKIT_TEST_SWF` points to one
//! whose main movie defines a `Stats` class with `int` statics (Epic Boss
//! Fighter does). Everything else is generic.
//!
//! `RUFFLE_MODKIT_TEST_SWF=path/to/ebf.swf cargo test -p ruffle_core --features modkit --lib live_`

use super::inspect::{self, Registry};
use super::model::{Access, PropValue, SnapshotLimits, parse_patch};
use super::mods::{ModSource, ModStatus, ModsSnapshot, RuleSpec, Target};
use super::{AiState, EditRequest, HostRequest, HostResponse, Modkit};
use crate::avm2::{Activation, Object, TObject as _};
use crate::display_object::TDisplayObject;
use crate::player::{Player, PlayerBuilder};
use crate::tag_utils::movie_from_path;
use ruffle_common::duration::FloatDuration;
use std::sync::{Arc, Mutex};

fn load_game() -> Option<Arc<Mutex<Player>>> {
    load_game_with_mods(&[])
}

/// Like `load_game`, but the mods are installed before the first frame, so
/// objects the game creates while starting up are seen by their rules.
fn load_game_with_mods(mods: &[(&str, &str)]) -> Option<Arc<Mutex<Player>>> {
    let Ok(path) = std::env::var("RUFFLE_MODKIT_TEST_SWF") else {
        eprintln!("RUFFLE_MODKIT_TEST_SWF not set, skipping");
        return None;
    };
    let mut movie = movie_from_path(&path, None).expect("load SWF");
    // Epic Boss Fighter bench build: jump straight into a fight.
    movie.append_parameters([("benchBoss".to_string(), "3".to_string())]);
    let player = PlayerBuilder::new()
        .with_movie(movie)
        .with_viewport_dimensions(640, 480, 1.0)
        .with_autoplay(true)
        .build();
    if !mods.is_empty() {
        install(&player, mods);
    }
    run_frames(&player, 240);
    Some(player)
}

/// Installs `(file name, TOML text)` pairs. The game pretends to be the SWF
/// with hash `SWF_HASH`.
fn install(player: &Arc<Mutex<Player>>, mods: &[(&str, &str)]) {
    let sources = mods
        .iter()
        .map(|(file, text)| ModSource {
            file: file.to_string(),
            text: Ok(text.to_string()),
        })
        .collect();
    player.lock().unwrap().install_mods(
        sources,
        Some(SWF_HASH.to_string()),
        "file:///test/ebf.swf".to_string(),
        "test-mods".to_string(),
    );
}

const SWF_HASH: &str = "aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11";

fn mods_snapshot(player: &Arc<Mutex<Player>>) -> ModsSnapshot {
    with_activation(player, |activation| activation.avm2().mods.snapshot())
}

/// The first live tracked object whose class is called `class`.
fn tracked_of_class<'gc>(activation: &mut Activation<'_, 'gc>, class: &str) -> Object<'gc> {
    let mc = activation.gc();
    let objects = activation.avm2().mods.tracked_objects(mc);
    objects
        .into_iter()
        .find(|o| o.instance_class().name().local_name().to_string() == class)
        .unwrap_or_else(|| panic!("no tracked {class}"))
}

fn set_int<'gc>(activation: &mut Activation<'_, 'gc>, object: Object<'gc>, name: &str, v: i32) {
    let prop = inspect::read_props(activation, object, true)
        .into_iter()
        .find(|p| p.entry.name == name)
        .unwrap_or_else(|| panic!("{name} not found"));
    inspect::write_prop(activation, object, &prop.entry.access, &PropValue::Int(v)).expect("write");
}

fn run_frames(player: &Arc<Mutex<Player>>, frames: u32) {
    let mut player = player.lock().unwrap();
    for _ in 0..frames {
        player.preload(&mut crate::limits::ExecutionLimit::none());
        player.run_frame();
    }
}

fn with_activation<R>(
    player: &Arc<Mutex<Player>>,
    f: impl for<'gc> FnOnce(&mut Activation<'_, 'gc>) -> R,
) -> R {
    player
        .lock()
        .unwrap()
        .mutate_with_update_context(|context| {
            let domain = context.avm2.stage_domain();
            let mut activation = Activation::from_domain(context, domain);
            f(&mut activation)
        })
}

fn static_int<'gc>(activation: &mut Activation<'_, 'gc>, stats: Object<'gc>, name: &str) -> i32 {
    let prop = inspect::read_props(activation, stats, true)
        .into_iter()
        .find(|p| p.entry.name == name)
        .unwrap_or_else(|| panic!("Stats.{name} not found"));
    match prop.entry.value {
        Some(PropValue::Int(v)) => v,
        other => panic!("Stats.{name} is {other:?}"),
    }
}

/// Edit a static, keep playing, and check the game still sees the new value.
#[test]
fn live_edit_static_and_continue() {
    let Some(player) = load_game() else { return };

    let mut registry = Registry::default();
    let before = with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").expect("Stats class");
        let props = inspect::read_props(activation, stats, true);
        let bullet = props
            .iter()
            .find(|p| p.entry.name == "bulletNum")
            .expect("Stats.bulletNum");
        assert!(bullet.entry.is_editable(), "{:?}", bullet.entry);
        let before = static_int(activation, stats, "bulletNum");
        inspect::write_prop(activation, stats, &bullet.entry.access, &PropValue::Int(3))
            .expect("write");
        registry.id_for(activation.context, stats);
        before
    });
    assert_ne!(before, 3);

    run_frames(&player, 120);

    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "bulletNum"), 3);
    });
}

/// The whole AI flow minus the network: snapshot, patch, apply, undo.
#[test]
fn live_ai_patch_apply_and_undo() {
    let Some(player) = load_game() else { return };

    let mut modkit = Modkit::default();
    let (snapshot, stats_id, before) = with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        let before = static_int(activation, stats, "attackSpeed");
        let snapshot = inspect::build_snapshot(
            activation,
            &[stats],
            &SnapshotLimits::default(),
            &mut modkit.registry,
        );
        let id = modkit.registry.id_for(activation.context, stats);
        (snapshot, id, before)
    });
    let json = snapshot.to_llm_json(&SnapshotLimits::default());
    assert!(json.contains("\"attackSpeed\""), "{json}");

    // A bad patch is rejected as a whole.
    let bad = format!(r#"[{{"id":{stats_id},"set":{{"attackSpeed":1,"noSuchProp":2}}}}]"#);
    assert!(parse_patch(&bad, &snapshot).is_err());

    let target = before + 5;
    let patch = format!(r#"[{{"id":{stats_id},"set":{{"attackSpeed":{target}}}}}]"#);
    let changes = parse_patch(&patch, &snapshot).expect("valid patch");

    with_activation(&player, |activation| {
        modkit.apply_changes(activation, &changes);
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "attackSpeed"), target);
    });

    run_frames(&player, 60);

    with_activation(&player, |activation| {
        modkit.undo(activation);
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "attackSpeed"), before);
    });

    run_frames(&player, 30);
}

/// Picking on the stage finds game objects, and their display properties can
/// be changed while the game keeps running.
#[test]
fn live_pick_and_move_display_object() {
    let Some(player) = load_game() else { return };

    let mut registry = Registry::default();
    let picked = with_activation(&player, |activation| {
        // Scan a grid; the fight screen is full of game sprites.
        let mut found = None;
        'scan: for gy in 1..8 {
            for gx in 1..8 {
                let point = swf::Point::from_pixels(gx as f64 * 80.0, gy as f64 * 60.0);
                if let Some((object, _)) = inspect::pick(activation, point)
                    && !inspect::is_builtin_class(activation, object.instance_class())
                {
                    found = Some(object);
                    break 'scan;
                }
            }
        }
        let object = found.expect("no game object under any grid point");
        let props = inspect::read_props(activation, object, true);
        let x = props
            .iter()
            .find(|p| p.entry.name == "x")
            .expect("display object has x");
        assert!(matches!(
            x.entry.access,
            Access::Accessor { set: Some(_), .. }
        ));
        inspect::write_prop(
            activation,
            object,
            &x.entry.access,
            &PropValue::Number(123.0),
        )
        .expect("set x");
        let display = object.as_display_object().unwrap();
        assert_eq!(display.x().to_pixels(), 123.0);
        registry.id_for(activation.context, object)
    });

    run_frames(&player, 1);

    // The id still resolves after the game ran.
    player
        .lock()
        .unwrap()
        .mutate_with_update_context(|context| {
            assert!(registry.fetch(context.dynamic_root, picked).is_some());
        });
}

const HERO: &str = "com.entertainmentForge.hero.Hero";

/// The three triggers on one class. EBF's `Hero` sets its stats when it is
/// added to the stage, which is after its constructor: a `create` rule runs
/// too early to stick (but does run), a `first_frame` rule wins over the
/// game's set-up, and an `every_frame` rule keeps a value from drifting.
#[test]
fn live_instance_rules_create_and_every_frame() {
    let toml = format!(
        r#"
[[rule]]
class = "{HERO}"
on = "create"
[rule.set]
damage = 555

[[rule]]
class = "{HERO}"
on = "first_frame"
[rule.set]
damage = 777
bulletNum = 4

[[rule]]
class = "Hero"
on = "every_frame"
[rule.set]
healthMax = 99
"#
    );
    let Some(player) = load_game_with_mods(&[("hero.toml", &toml)]) else {
        return;
    };

    let snapshot = mods_snapshot(&player);
    let rules = &snapshot.mods[0].rules;
    assert_eq!(snapshot.mods[0].status, ModStatus::Active);
    assert!(rules[0].applied >= 1, "create rule: {:?}", rules[0]);
    assert!(rules[1].applied >= 2, "first_frame rule: {:?}", rules[1]);
    assert!(rules[2].applied >= 1, "every_frame rule: {:?}", rules[2]);
    for rule in rules {
        assert!(rule.errors.is_empty(), "{:?}", rule.errors);
    }
    assert!(snapshot.tracked >= 1);

    with_activation(&player, |activation| {
        let hero = tracked_of_class(activation, "Hero");
        // The game sets `damage = Stats.damage + 5` and so on after the
        // constructor; the first_frame rule is the last word.
        assert_eq!(static_int(activation, hero, "damage"), 777);
        assert_eq!(static_int(activation, hero, "bulletNum"), 4);
        assert_eq!(static_int(activation, hero, "healthMax"), 99);

        // The game (or a player poking at the editor) changes the value...
        set_int(activation, hero, "healthMax", 1);
        assert_eq!(static_int(activation, hero, "healthMax"), 1);
    });
    // ...and the next frame puts it back.
    run_frames(&player, 2);
    with_activation(&player, |activation| {
        let hero = tracked_of_class(activation, "Hero");
        assert_eq!(static_int(activation, hero, "healthMax"), 99);
        // Neither create nor first_frame rules hold the value.
        set_int(activation, hero, "damage", 5);
    });
    run_frames(&player, 30);
    with_activation(&player, |activation| {
        let hero = tracked_of_class(activation, "Hero");
        assert_eq!(static_int(activation, hero, "damage"), 5);
    });
}

/// Static rules edit the class object; `create` runs once, `every_frame`
/// keeps holding.
#[test]
fn live_static_rules() {
    let Some(player) = load_game() else { return };
    let toml = r#"
[[rule]]
class = "Stats"
target = "static"
on = "every_frame"
[rule.set]
attackSpeed = 11

[[rule]]
class = "Stats"
target = "static"
on = "create"
[rule.set]
bulletNum = 6
"#;
    install(&player, &[("stats.toml", toml)]);
    run_frames(&player, 3);

    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "attackSpeed"), 11);
        assert_eq!(static_int(activation, stats, "bulletNum"), 6);
        set_int(activation, stats, "attackSpeed", 2);
        set_int(activation, stats, "bulletNum", 1);
    });
    run_frames(&player, 5);
    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "attackSpeed"), 11);
        assert_eq!(static_int(activation, stats, "bulletNum"), 1);
    });

    let snapshot = mods_snapshot(&player);
    assert!(snapshot.mods[0].rules[0].applied >= 2);
    assert_eq!(snapshot.mods[0].rules[1].applied, 1);

    // Reinstalling runs a `create` rule again.
    install(&player, &[("stats.toml", toml)]);
    run_frames(&player, 3);
    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "bulletNum"), 6);
    });
}

/// Bad rules are reported, never crash the game, and mods of other SWFs or
/// disabled rules do nothing.
#[test]
fn live_bad_disabled_and_foreign_rules() {
    let other_swf = format!(
        r#"
[mod]
swf_sha256 = "{}"
[[rule]]
class = "Stats"
target = "static"
on = "every_frame"
[rule.set]
attackSpeed = 99
"#,
        "bb22".repeat(16)
    );
    let bad = r#"
[[rule]]
class = "Stats"
target = "static"
on = "every_frame"
[rule.set]
noSuchProp = 1
attackSpeed = "fast"
bulletNum = 4

[[rule]]
class = "NoSuchClass"
target = "static"
on = "every_frame"
[rule.set]
x = 1

[[rule]]
class = "Stats"
target = "static"
on = "every_frame"
[rule.set]
healthMax = 42
"#;
    let Some(player) = load_game() else { return };
    install(
        &player,
        &[
            ("other.toml", &other_swf),
            ("bad.toml", bad),
            ("broken.toml", "this is = = not toml"),
        ],
    );
    run_frames(&player, 70);

    let snapshot = mods_snapshot(&player);
    assert_eq!(snapshot.mods.len(), 3);
    let other = snapshot
        .mods
        .iter()
        .find(|m| m.file == "other.toml")
        .unwrap();
    assert_eq!(other.status, ModStatus::OtherSwf);
    assert_eq!(other.rules[0].applied, 0);
    let broken = snapshot
        .mods
        .iter()
        .find(|m| m.file == "broken.toml")
        .unwrap();
    assert!(
        matches!(broken.status, ModStatus::Invalid(_)),
        "{:?}",
        broken.status
    );

    let bad_index = snapshot
        .mods
        .iter()
        .position(|m| m.file == "bad.toml")
        .unwrap();
    let bad_mod = &snapshot.mods[bad_index];
    assert_eq!(bad_mod.status, ModStatus::Active);
    let rule = &bad_mod.rules[0];
    assert!(
        rule.errors.iter().any(|e| e.contains("noSuchProp")),
        "{:?}",
        rule.errors
    );
    assert!(
        rule.errors.iter().any(|e| e.contains("attackSpeed")),
        "{:?}",
        rule.errors
    );
    // The good property of the same rule still works.
    assert!(rule.applied >= 1);
    assert_eq!(bad_mod.rules[1].applied, 0);
    assert!(bad_mod.rules[2].applied >= 1);

    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_ne!(static_int(activation, stats, "attackSpeed"), 99);
        assert_eq!(static_int(activation, stats, "bulletNum"), 4);
        assert_eq!(static_int(activation, stats, "healthMax"), 42);
        set_int(activation, stats, "healthMax", 1);
    });

    // A disabled rule stops holding its value.
    with_activation(&player, |activation| {
        activation.avm2().mods.set_rule_enabled(bad_index, 2, false);
    });
    run_frames(&player, 5);
    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "healthMax"), 1);
    });
}

/// Edits one `int` property the way the UI does, so it lands on the undo stack.
fn edit_int<'gc>(
    modkit: &mut Modkit,
    activation: &mut Activation<'_, 'gc>,
    object: Object<'gc>,
    name: &str,
    value: i32,
) {
    let id = modkit.registry.id_for(activation.context, object);
    let prop = inspect::read_props(activation, object, true)
        .into_iter()
        .find(|p| p.entry.name == name)
        .unwrap_or_else(|| panic!("{name} not found"));
    let edit = EditRequest {
        id,
        prop: name.to_string(),
        access: prop.entry.access,
        value: PropValue::Int(value),
    };
    modkit.edit(activation, edit, None);
}

fn tick(player: &Arc<Mutex<Player>>, millis: f64) {
    player
        .lock()
        .unwrap()
        .tick(FloatDuration::from_millis(millis));
}

fn take_ai_request(modkit: &mut Modkit) -> (u64, String) {
    modkit
        .take_host_requests()
        .into_iter()
        .find_map(|request| match request {
            HostRequest::Ai { id, user, .. } => Some((id, user)),
            _ => None,
        })
        .expect("an AI request")
}

fn saved_rules(modkit: &mut Modkit) -> Vec<RuleSpec> {
    modkit
        .take_host_requests()
        .into_iter()
        .filter_map(|request| match request {
            HostRequest::SaveRule { rule } => Some(rule),
            _ => None,
        })
        .collect()
}

/// Pause, edit by hand, resume: the game stands still while paused, and the
/// edits are still there when it runs again.
#[test]
fn live_pause_edit_and_resume() {
    let toml = format!(
        r#"
[[rule]]
class = "{HERO}"
on = "every_frame"
[rule.set]
healthMax = 99
"#
    );
    let Some(player) = load_game_with_mods(&[("hero.toml", &toml)]) else {
        return;
    };
    let mut modkit = Modkit::default();
    modkit.open();

    // What the desktop frontend does when the Modkit window pauses.
    modkit.paused = true;
    assert!(modkit.should_suspend_player());
    player.lock().unwrap().set_is_playing(false);

    with_activation(&player, |activation| {
        let hero = tracked_of_class(activation, "Hero");
        edit_int(&mut modkit, activation, hero, "healthMax", 1);
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        edit_int(&mut modkit, activation, stats, "bulletNum", 3);
    });

    // The every-frame rule would put `healthMax` back if any frame ran.
    for _ in 0..5 {
        tick(&player, 100.0);
    }
    player.lock().unwrap().render();
    with_activation(&player, |activation| {
        let hero = tracked_of_class(activation, "Hero");
        assert_eq!(static_int(activation, hero, "healthMax"), 1);
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "bulletNum"), 3);
    });

    modkit.paused = false;
    assert!(!modkit.should_suspend_player());
    player.lock().unwrap().set_is_playing(true);
    for _ in 0..5 {
        tick(&player, 100.0);
    }
    with_activation(&player, |activation| {
        let hero = tracked_of_class(activation, "Hero");
        assert_eq!(static_int(activation, hero, "healthMax"), 99, "game ran");
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "bulletNum"), 3, "edit held");
    });
}

/// An AI request pauses the game until the patch is applied or dismissed,
/// unless the user paused already or switched the behaviour off.
#[test]
fn live_ai_request_pauses_until_applied() {
    let Some(player) = load_game() else { return };
    let mut modkit = Modkit::default();
    modkit.open();
    let (stats_id, before) = with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        let id = modkit.registry.id_for(activation.context, stats);
        modkit.select(id, false);
        (id, static_int(activation, stats, "attackSpeed"))
    });
    assert!(!modkit.should_suspend_player());

    // A failed request keeps the game paused, so the user can retry.
    with_activation(&player, |activation| modkit.start_ai_request(activation));
    assert!(modkit.should_suspend_player());
    let (id, user) = take_ai_request(&mut modkit);
    assert!(user.contains("attackSpeed"), "{user}");
    modkit.push_host_response(HostResponse::Ai {
        id,
        result: Err("boom".into()),
    });
    assert!(matches!(modkit.ai.state, AiState::Failed { .. }));
    assert!(modkit.should_suspend_player());
    modkit.dismiss_ai();
    assert!(!modkit.should_suspend_player());

    // Review keeps it paused; applying resumes and edits the game.
    with_activation(&player, |activation| modkit.start_ai_request(activation));
    let (id, _) = take_ai_request(&mut modkit);
    let target = before + 5;
    modkit.push_host_response(HostResponse::Ai {
        id,
        result: Ok(format!(
            r#"[{{"id":{stats_id},"set":{{"attackSpeed":{target}}}}}]"#
        )),
    });
    assert!(matches!(modkit.ai.state, AiState::Review { .. }));
    assert!(modkit.should_suspend_player());
    with_activation(&player, |activation| modkit.apply_ai(activation, false));
    assert!(!modkit.should_suspend_player());
    assert!(matches!(modkit.ai.state, AiState::Idle));
    assert!(modkit.take_host_requests().is_empty());
    run_frames(&player, 30);
    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        assert_eq!(static_int(activation, stats, "attackSpeed"), target);
    });

    // "Apply & save as rule" resumes too, and queues the rule.
    with_activation(&player, |activation| modkit.start_ai_request(activation));
    assert!(modkit.should_suspend_player());
    let (id, _) = take_ai_request(&mut modkit);
    modkit.push_host_response(HostResponse::Ai {
        id,
        result: Ok(format!(
            r#"[{{"id":{stats_id},"set":{{"attackSpeed":{}}}}}]"#,
            target + 1
        )),
    });
    with_activation(&player, |activation| modkit.apply_ai(activation, true));
    assert!(!modkit.should_suspend_player());
    let rules = saved_rules(&mut modkit);
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].target, Target::Static);
    assert!(rules[0].class.ends_with("Stats"), "{:?}", rules[0].class);
    assert_eq!(
        rules[0].set,
        vec![("attackSpeed".to_string(), (target + 1).into())]
    );

    // A pause the user chose is not ended by the AI.
    modkit.paused = true;
    with_activation(&player, |activation| modkit.start_ai_request(activation));
    take_ai_request(&mut modkit);
    modkit.dismiss_ai();
    assert!(modkit.should_suspend_player());
    modkit.paused = false;

    // Taking over during the request keeps the pause too.
    with_activation(&player, |activation| modkit.start_ai_request(activation));
    assert!(modkit.auto_paused);
    modkit.auto_paused = false; // what clicking the Pause toggle does
    take_ai_request(&mut modkit);
    modkit.dismiss_ai();
    assert!(modkit.should_suspend_player());
    modkit.paused = false;

    // Switched off in the settings: the game keeps running.
    modkit.settings.ai_auto_pause = false;
    with_activation(&player, |activation| modkit.start_ai_request(activation));
    assert!(!modkit.should_suspend_player());
}

/// Every edit of the session, by hand or by the AI, is saved as rules: one
/// value per property (the last one), none for properties changed back.
#[test]
fn live_save_session_edits() {
    let Some(player) = load_game() else { return };
    let mut modkit = Modkit::default();
    modkit.open();

    // Nothing edited yet.
    with_activation(&player, |activation| modkit.save_session_edits(activation));
    assert!(saved_rules(&mut modkit).is_empty());
    assert_eq!(modkit.status, "No edits to save");

    let (stats_id, speed, bullets) = with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        let id = modkit.registry.id_for(activation.context, stats);
        modkit.select(id, false);
        let speed = static_int(activation, stats, "attackSpeed");
        let bullets = static_int(activation, stats, "bulletNum");
        // A third property that is edited and changed back.
        let props = inspect::read_props(activation, stats, true);
        let other = props
            .iter()
            .find(|p| {
                p.entry.is_editable()
                    && matches!(p.entry.value, Some(PropValue::Int(_)))
                    && !["attackSpeed", "bulletNum"].contains(&p.entry.name.as_str())
            })
            .expect("another editable int in Stats");
        let Some(PropValue::Int(other_value)) = other.entry.value.clone() else {
            unreachable!()
        };
        let other_name = other.entry.name.clone();
        edit_int(&mut modkit, activation, stats, "attackSpeed", speed + 1);
        edit_int(&mut modkit, activation, stats, "bulletNum", bullets + 1);
        edit_int(&mut modkit, activation, stats, &other_name, other_value + 1);
        edit_int(&mut modkit, activation, stats, &other_name, other_value);
        (id, speed, bullets)
    });

    // The AI changes the same property once more.
    with_activation(&player, |activation| modkit.start_ai_request(activation));
    let (id, _) = take_ai_request(&mut modkit);
    modkit.push_host_response(HostResponse::Ai {
        id,
        result: Ok(format!(
            r#"[{{"id":{stats_id},"set":{{"attackSpeed":{}}}}}]"#,
            speed + 9
        )),
    });
    with_activation(&player, |activation| modkit.apply_ai(activation, false));
    assert!(modkit.take_host_requests().is_empty());

    let edits = modkit.session_edits();
    assert_eq!(edits.len(), 2, "{edits:?}");
    let speed_edit = edits.iter().find(|e| e.prop == "attackSpeed").unwrap();
    assert_eq!(speed_edit.old, PropValue::Int(speed));
    assert_eq!(speed_edit.new, PropValue::Int(speed + 9));

    with_activation(&player, |activation| modkit.save_session_edits(activation));
    let rules = saved_rules(&mut modkit);
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].target, Target::Static);
    let mut set = rules[0].set.clone();
    set.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        set,
        vec![
            ("attackSpeed".to_string(), (speed + 9).into()),
            ("bulletNum".to_string(), (bullets + 1).into()),
        ]
    );
    assert!(
        modkit.status.starts_with("2 edited properties"),
        "{}",
        modkit.status
    );
}

/// A round trip with a real model, in two runs because the core has no
/// network. With `RUFFLE_MODKIT_AI_DIR` set, the first run writes the request
/// to `system.txt` and `user.txt` there. Once something has answered into
/// `reply.txt`, the next run feeds the answer back and expects a patch that
/// passes validation.
#[test]
fn live_ai_real_model_round_trip() {
    let Ok(dir) = std::env::var("RUFFLE_MODKIT_AI_DIR") else {
        return;
    };
    let Some(player) = load_game() else { return };
    let dir = std::path::PathBuf::from(dir);
    let mut modkit = Modkit::default();
    modkit.open();
    modkit.ai.prompt = std::fs::read_to_string(dir.join("prompt.txt"))
        .unwrap_or_else(|_| "Make the player shoot much faster and fire more bullets".into());
    with_activation(&player, |activation| {
        let stats = inspect::find_class_object(activation, "Stats").unwrap();
        let id = modkit.registry.id_for(activation.context, stats);
        modkit.select(id, false);
        modkit.start_ai_request(activation);
    });
    let (id, system, user) = modkit
        .take_host_requests()
        .into_iter()
        .find_map(|request| match request {
            HostRequest::Ai { id, system, user } => Some((id, system, user)),
            _ => None,
        })
        .expect("an AI request");

    let Ok(reply) = std::fs::read_to_string(dir.join("reply.txt")) else {
        std::fs::write(dir.join("system.txt"), system).expect("write system.txt");
        std::fs::write(dir.join("user.txt"), user).expect("write user.txt");
        return;
    };
    modkit.push_host_response(HostResponse::Ai {
        id,
        result: Ok(reply),
    });
    match &modkit.ai.state {
        AiState::Review { changes, .. } => {
            assert!(!changes.is_empty(), "the model changed nothing");
            for change in changes {
                eprintln!("{} {:?} -> {:?}", change.prop, change.old, change.new);
            }
        }
        other => panic!("patch rejected: {other:?}"),
    }
    with_activation(&player, |activation| modkit.apply_ai(activation, false));
    assert!(!modkit.should_suspend_player());
    run_frames(&player, 30);
}

/// Request ids never repeat between players, and a status response is shown.
#[test]
fn request_ids_and_status_response() {
    let mut a = Modkit::default();
    let mut b = Modkit::default();
    let first = a.next_id();
    assert_ne!(first, b.next_id());
    assert_ne!(first, a.next_id());

    a.push_host_response(HostResponse::Status("Couldn't open X".into()));
    assert_eq!(a.status, "Couldn't open X");
    assert!(!a.adopt_pending);
}
