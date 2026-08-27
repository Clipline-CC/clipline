//! Behavioral tests for the pure region-selection logic in ui/region-core.js.
//!
//! Same Boa harness as player_core.rs: the file must evaluate with no DOM and
//! no Tauri globals so it runs identically on both CI OSes.

use boa_engine::{Context, Source};
use std::fs;
use std::path::Path;

fn region_core_context() -> Context {
    let mut context = Context::default();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/region-core.js");
    let source = fs::read_to_string(path).expect("read ui/region-core.js");
    context
        .eval(Source::from_bytes(&source))
        .expect("region-core.js evaluates without DOM or Tauri globals");
    context
}

fn eval(context: &mut Context, expr: impl AsRef<str>) -> String {
    let value = context
        .eval(Source::from_bytes(expr.as_ref()))
        .unwrap_or_else(|err| panic!("eval failed: {err}"));
    value
        .to_string(context)
        .unwrap_or_else(|err| panic!("stringify failed: {err}"))
        .to_std_string_escaped()
}

fn eval_json(context: &mut Context, expr: impl AsRef<str>) -> String {
    eval(context, format!("JSON.stringify({})", expr.as_ref()))
}

const FRAME: &str = "{x: 0, y: 0, width: 1920, height: 1080}";

#[
    test
]
fn drags_in_any_direction_normalize_to_a_positive_clamped_rect() {
    let mut ctx = region_core_context();
    assert_eq!(
        eval_json(&mut ctx, "RegionCore.dragResult({x:100,y:50},{x:360,y:210},".to_owned() + FRAME + ")"),
        r#"{"x":100,"y":50,"width":260,"height":160}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "RegionCore.dragResult({x:360,y:210},{x:100,y:50},".to_owned() + FRAME + ")"),
        r#"{"x":100,"y":50,"width":260,"height":160}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "RegionCore.dragResult({x:-500,y:-500},{x:10,y:10},".to_owned() + FRAME + ")"),
        r#"{"x":0,"y":0,"width":10,"height":10}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "RegionCore.dragResult({x:1900,y:1070},{x:9999,y:9999},".to_owned() + FRAME + ")"),
        r#"{"x":1900,"y":1070,"width":20,"height":10}"#
    );
}

#[
    test
]
fn a_click_without_a_drag_cancels_instead_of_selecting() {
    let mut ctx = region_core_context();
    assert_eq!(
        eval(&mut ctx, "RegionCore.dragResult({x:40,y:40},{x:41,y:41},".to_owned() + FRAME + ")"),
        "null"
    );
    assert_eq!(
        eval(&mut ctx, "RegionCore.dragResult({x:40,y:40},{x:42,y:42},".to_owned() + FRAME + ")"),
        "null"
    );
    assert_ne!(
        eval(&mut ctx, "RegionCore.dragResult({x:40,y:40},{x:43,y:43},".to_owned() + FRAME + ")"),
        "null"
    );
}

#[
    test
]
fn esc_is_reported_as_cancel_by_the_key_check() {
    let mut ctx = region_core_context();
    assert_eq!(eval(&mut ctx, "RegionCore.escapeCancels('Escape')"), "true");
    assert_eq!(eval(&mut ctx, "RegionCore.escapeCancels('Enter')"), "false");
}

#[
    test
]
fn the_readout_matches_the_physical_pixel_rect() {
    let mut ctx = region_core_context();
    assert_eq!(
        eval(&mut ctx, "RegionCore.readout({x:8,y:9,width:1920,height:1080})"),
        "1920 x 1080"
    );
}

#[
    test
]
fn snapping_picks_the_topmost_containing_candidate_and_stays_in_frame() {
    let mut ctx = region_core_context();
    let candidates = "[{x:100,y:100,width:400,height:300},{x:150,y:150,width:200,height:100}]";
    assert_eq!(
        eval_json(&mut ctx, "RegionCore.snapRect({x:200,y:180},".to_owned() + candidates + "," + FRAME + ")"),
        r#"{"x":150,"y":150,"width":200,"height":100}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "RegionCore.snapRect({x:120,y:120},".to_owned() + candidates + "," + FRAME + ")"),
        r#"{"x":100,"y":100,"width":400,"height":300}"#
    );
    assert_eq!(
        eval_json(&mut ctx, "RegionCore.snapRect({x:5,y:5},".to_owned() + candidates + "," + FRAME + ")"),
        "null"
    );
    // A candidate hanging off the monitor edge snaps clamped into the frame.
    assert_eq!(
        eval_json(
            &mut ctx,
            "RegionCore.snapRect({x:1900,y:500},[{x:1880,y:480,width:400,height:300}],".to_owned() + FRAME + ")"
        ),
        r#"{"x":1880,"y":480,"width":40,"height":300}"#
    );
}
