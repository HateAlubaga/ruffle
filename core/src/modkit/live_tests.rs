//! End-to-end checks against a real game, run headless.
//!
//! Needs a SWF, so it only runs when `RUFFLE_MODKIT_TEST_SWF` points to one
//! whose main movie defines a `Stats` class with `int` statics (Epic Boss
//! Fighter does). Everything else is generic.
//!
//! `RUFFLE_MODKIT_TEST_SWF=path/to/ebf.swf cargo test -p ruffle_core --features modkit --lib live_`

use super::Modkit;
use super::inspect::{self, Registry};
use super::model::{Access, PropValue, SnapshotLimits, parse_patch};
use crate::avm2::{Activation, Object, TObject as _};
use crate::display_object::TDisplayObject;
use crate::player::{Player, PlayerBuilder};
use crate::tag_utils::movie_from_path;
use std::sync::{Arc, Mutex};

fn load_game() -> Option<Arc<Mutex<Player>>> {
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
    run_frames(&player, 240);
    Some(player)
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
    let prop = inspect::read_props(activation, stats)
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
        let props = inspect::read_props(activation, stats);
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
        let props = inspect::read_props(activation, object);
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
