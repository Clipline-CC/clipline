use boa_engine::{Context, Source};
use std::fs;
use std::path::Path;

fn context() -> Context {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/window-lifecycle-core.js");
    let source = fs::read_to_string(path).expect("read ui/window-lifecycle-core.js");
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(&source))
        .expect("window-lifecycle-core.js evaluates without DOM or Tauri globals");
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
fn initial_state_is_pessimistically_backgrounded_until_native_state_arrives() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "JSON.stringify({\
               state: WindowLifecycleCore.initialState(),\
               work: WindowLifecycleCore.captureWork(WindowLifecycleCore.initialState())\
             })",
        ),
        r#"{"state":{"known":false,"backgrounded":true,"nativeRevision":null,"generation":0,"dirty":true},"work":null}"#
    );
}

#[test]
fn first_foreground_snapshot_enters_a_generation_and_requests_initial_refresh() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const transition = WindowLifecycleCore.applySnapshot(\
               WindowLifecycleCore.initialState(),\
               { revision: 0, backgrounded: false }\
             );\
             JSON.stringify(transition)",
        ),
        r#"{"state":{"known":true,"backgrounded":false,"nativeRevision":0,"generation":1,"dirty":false},"accepted":true,"enteredBackground":false,"enteredForeground":true,"refreshRequired":true}"#
    );
}

#[test]
fn background_refresh_requests_coalesce_until_one_foreground_refresh() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "let state = WindowLifecycleCore.applySnapshot(\
               WindowLifecycleCore.initialState(),\
               { revision: 0, backgrounded: true }\
             ).state;\
             let immediate = 0;\
             for (let i = 0; i < 500; i += 1) {\
               const requested = WindowLifecycleCore.requestRefresh(state);\
               state = requested.state;\
               if (requested.refreshNow) immediate += 1;\
             }\
             const foreground = WindowLifecycleCore.applySnapshot(\
               state,\
               { revision: 1, backgrounded: false }\
             );\
             const repeated = WindowLifecycleCore.applySnapshot(\
               foreground.state,\
               { revision: 1, backgrounded: false }\
             );\
             JSON.stringify({ immediate, foreground, repeated })",
        ),
        r#"{"immediate":0,"foreground":{"state":{"known":true,"backgrounded":false,"nativeRevision":1,"generation":2,"dirty":false},"accepted":true,"enteredBackground":false,"enteredForeground":true,"refreshRequired":true},"repeated":{"state":{"known":true,"backgrounded":false,"nativeRevision":1,"generation":2,"dirty":false},"accepted":true,"enteredBackground":false,"enteredForeground":false,"refreshRequired":false}}"#
    );
}

#[test]
fn background_transition_and_newer_snapshot_reject_stale_async_work() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "let state = WindowLifecycleCore.applySnapshot(\
               WindowLifecycleCore.initialState(),\
               { revision: 4, backgrounded: false }\
             ).state;\
             const oldWork = WindowLifecycleCore.captureWork(state);\
             state = WindowLifecycleCore.applySnapshot(\
               state,\
               { revision: 5, backgrounded: true }\
             ).state;\
             const staleSnapshot = WindowLifecycleCore.applySnapshot(\
               state,\
               { revision: 4, backgrounded: false }\
             );\
             const oldStillCurrent = WindowLifecycleCore.isWorkCurrent(\
               staleSnapshot.state,\
               oldWork\
             );\
             const foreground = WindowLifecycleCore.applySnapshot(\
               staleSnapshot.state,\
               { revision: 6, backgrounded: false }\
             );\
             const newWork = WindowLifecycleCore.captureWork(foreground.state);\
             JSON.stringify({\
               staleAccepted: staleSnapshot.accepted,\
               staleState: staleSnapshot.state,\
               oldStillCurrent,\
               foreground,\
               newStillCurrent: WindowLifecycleCore.isWorkCurrent(foreground.state, newWork)\
             })",
        ),
        r#"{"staleAccepted":false,"staleState":{"known":true,"backgrounded":true,"nativeRevision":5,"generation":2,"dirty":true},"oldStillCurrent":false,"foreground":{"state":{"known":true,"backgrounded":false,"nativeRevision":6,"generation":3,"dirty":false},"accepted":true,"enteredBackground":false,"enteredForeground":true,"refreshRequired":true},"newStillCurrent":true}"#
    );
}

#[test]
fn duplicate_snapshots_are_idempotent_and_conflicting_equal_revisions_are_rejected() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const first = WindowLifecycleCore.applySnapshot(\
               WindowLifecycleCore.initialState(),\
               { revision: 7, backgrounded: false }\
             );\
             const work = WindowLifecycleCore.captureWork(first.state);\
             const duplicate = WindowLifecycleCore.applySnapshot(\
               first.state,\
               { revision: 7, backgrounded: false }\
             );\
             const conflict = WindowLifecycleCore.applySnapshot(\
               duplicate.state,\
               { revision: 7, backgrounded: true }\
             );\
             const newer = WindowLifecycleCore.applySnapshot(\
               conflict.state,\
               { revision: 8, backgrounded: false }\
             );\
             JSON.stringify({\
               duplicate,\
               conflictAccepted: conflict.accepted,\
               conflictState: conflict.state,\
               oldWorkAfterNewRevision: WindowLifecycleCore.isWorkCurrent(newer.state, work),\
               newerGeneration: newer.state.generation\
             })",
        ),
        r#"{"duplicate":{"state":{"known":true,"backgrounded":false,"nativeRevision":7,"generation":1,"dirty":false},"accepted":true,"enteredBackground":false,"enteredForeground":false,"refreshRequired":false},"conflictAccepted":false,"conflictState":{"known":true,"backgrounded":false,"nativeRevision":7,"generation":1,"dirty":false},"oldWorkAfterNewRevision":false,"newerGeneration":2}"#
    );
}

#[test]
fn foreground_revision_gap_reports_a_missed_background_and_forces_refresh() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const visible = WindowLifecycleCore.applySnapshot(\
               WindowLifecycleCore.initialState(),\
               { revision: 4, backgrounded: false }\
             ).state;\
             const transition = WindowLifecycleCore.applySnapshot(\
               visible,\
               { revision: 6, backgrounded: false }\
             );\
             JSON.stringify(transition)",
        ),
        r#"{"state":{"known":true,"backgrounded":false,"nativeRevision":6,"generation":2,"dirty":false},"accepted":true,"enteredBackground":false,"enteredForeground":false,"refreshRequired":true,"missedBackground":true}"#
    );
}

#[test]
fn visible_refresh_requests_run_immediately_without_latching_dirty_state() {
    let mut context = context();
    assert_eq!(
        eval(
            &mut context,
            "const visible = WindowLifecycleCore.applySnapshot(\
               WindowLifecycleCore.initialState(),\
               { revision: 0, backgrounded: false }\
             ).state;\
             JSON.stringify(WindowLifecycleCore.requestRefresh(visible))",
        ),
        r#"{"state":{"known":true,"backgrounded":false,"nativeRevision":0,"generation":1,"dirty":false},"refreshNow":true}"#
    );
}
