use boa_engine::{Context, Source};
use std::fs;
use std::path::Path;

fn context() -> Context {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/window-picker-core.js");
    let source = fs::read_to_string(path).expect("read ui/window-picker-core.js");
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(&source))
        .expect("window-picker-core.js evaluates without DOM or Tauri globals");
    context
        .eval(Source::from_bytes(
            "var win = (handle, pid, minimized) => \
               ({ handle, process_id: pid, minimized: Boolean(minimized) });\
             var state = WindowPickerCore.initial();\
             var take = () => { const step = WindowPickerCore.next(state); state = step.state; return step.request; };\
             var done = (request) => { const step = WindowPickerCore.finish(state, request); state = step.state; return step.apply; };",
        ))
        .expect("test helpers");
    context
}

fn eval(context: &mut Context, expression: &str) -> String {
    context
        .eval(Source::from_bytes(expression))
        .unwrap_or_else(|error| panic!("eval `{expression}`: {error}"))
        .to_string(context)
        .expect("stringify result")
        .to_std_string_escaped()
}

#[test]
fn previews_are_requested_one_at_a_time_in_list_order() {
    let mut context = context();
    eval(
        &mut context,
        "state = WindowPickerCore.begin(state, [win(1, 10), win(2, 20), win(3, 30)])",
    );
    assert_eq!(eval(&mut context, "var a = take(); a.handle + ':' + a.processId"), "1:10");
    assert_eq!(eval(&mut context, "take()"), "null", "one native capture at a time");
    assert_eq!(eval(&mut context, "done(a)"), "true");
    assert_eq!(eval(&mut context, "var b = take(); b.handle"), "2");
    assert_eq!(eval(&mut context, "done(b)"), "true");
    assert_eq!(eval(&mut context, "var c = take(); c.handle"), "3");
    assert_eq!(eval(&mut context, "done(c)"), "true");
    assert_eq!(eval(&mut context, "take()"), "null", "queue drained");
}

#[test]
fn minimized_windows_get_icons_without_a_capture() {
    let mut context = context();
    eval(
        &mut context,
        "state = WindowPickerCore.begin(state, [win(1, 10, true), win(2, 20)])",
    );
    assert_eq!(eval(&mut context, "var a = take(); a.handle"), "2");
    assert_eq!(eval(&mut context, "done(a); take()"), "null");
}

#[test]
fn refresh_waits_for_the_running_capture_and_drops_its_result() {
    let mut context = context();
    eval(&mut context, "state = WindowPickerCore.begin(state, [win(1, 10), win(2, 20)])");
    eval(&mut context, "var old = take()");
    eval(&mut context, "state = WindowPickerCore.begin(state, [win(7, 70)])");
    assert_eq!(
        eval(&mut context, "take()"),
        "null",
        "a new capture must not start while the old one is still running"
    );
    assert_eq!(eval(&mut context, "done(old)"), "false", "stale result is dropped");
    assert_eq!(eval(&mut context, "var fresh = take(); fresh.handle"), "7");
    assert_eq!(eval(&mut context, "done(fresh)"), "true");
    assert_eq!(eval(&mut context, "take()"), "null", "the old queue is gone");
}

#[test]
fn closing_or_selecting_abandons_the_queue() {
    let mut context = context();
    eval(&mut context, "state = WindowPickerCore.begin(state, [win(1, 10), win(2, 20)])");
    eval(&mut context, "var running = take()");
    eval(&mut context, "state = WindowPickerCore.abandon(state)");
    assert_eq!(eval(&mut context, "done(running)"), "false");
    assert_eq!(eval(&mut context, "take()"), "null", "no requests after close");

    // Reopening starts fresh once nothing is running.
    eval(&mut context, "state = WindowPickerCore.begin(state, [win(5, 50)])");
    assert_eq!(eval(&mut context, "var reopened = take(); reopened.handle"), "5");
    assert_eq!(eval(&mut context, "done(reopened)"), "true");
}

#[test]
fn a_foreign_or_repeated_finish_does_not_unblock_the_queue() {
    let mut context = context();
    eval(&mut context, "state = WindowPickerCore.begin(state, [win(1, 10), win(2, 20)])");
    eval(&mut context, "var a = take()");
    assert_eq!(
        eval(&mut context, "done({ generation: -1, handle: 99, processId: 9 })"),
        "false"
    );
    assert_eq!(eval(&mut context, "take()"), "null", "the real capture is still running");
    assert_eq!(eval(&mut context, "done(a)"), "true");
    assert_eq!(eval(&mut context, "done(a)"), "false", "finishing twice is a no-op");
}

#[test]
fn icons_are_requested_once_per_process() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "JSON.stringify(WindowPickerCore.iconProcessIds([win(1, 10), win(2, 20), win(3, 10), win(4, 0)]))",
        ),
        "[10,20]"
    );
}
