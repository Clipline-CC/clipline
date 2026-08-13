//! Behavioral tests for Discord-style Library search tokens in
//! `ui/gallery-search-core.js`.

use boa_engine::{Context, Source};
use std::fs;
use std::path::Path;

fn context() -> Context {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/gallery-search-core.js");
    let source = fs::read_to_string(path).expect("read ui/gallery-search-core.js");
    let mut context = Context::default();
    context
        .eval(Source::from_bytes(&source))
        .expect("gallery-search-core.js evaluates without DOM or Tauri globals");
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

fn eval_json(context: &mut Context, expression: &str) -> String {
    eval(context, &format!("JSON.stringify({expression})"))
}

#[test]
fn empty_and_prefix_input_offer_the_lol_type_filter() {
    let mut ctx = context();
    assert_eq!(
        eval_json(&mut ctx, "GallerySearchCore.inspect('').kind"),
        "\"empty\""
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "GallerySearchCore.matchingFilters('').map((item) => item.key)"
        ),
        "[\"lol-type\"]"
    );
    assert_eq!(
        eval_json(&mut ctx, "GallerySearchCore.inspect('lol').kind"),
        "\"filters\""
    );
    assert_eq!(
        eval_json(&mut ctx, "GallerySearchCore.inspect('LoL Type').kind"),
        "\"filters\""
    );
}

#[test]
fn colon_after_lol_type_opens_value_suggestions_including_replay() {
    let mut ctx = context();
    assert_eq!(
        eval_json(&mut ctx, "GallerySearchCore.inspect('LoL Type:').kind"),
        "\"values\""
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "GallerySearchCore.inspect('lol type: ar').valueDraft"
        ),
        "\"ar\""
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "GallerySearchCore.matchingValues('lol-type', 'ar', null).map((item) => item.value)"
        ),
        "[\"aram\",\"arena\"]"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "GallerySearchCore.matchingValues('lol-type', 'aram', null).map((item) => item.value)"
        ),
        "[\"aram\"]"
    );
    assert_eq!(
        eval_json(
            &mut ctx,
            "GallerySearchCore.matchingValues('lol-type', '', null).some((item) => item.value === 'replay')"
        ),
        "true"
    );
    assert_eq!(
        eval(
            &mut ctx,
            "GallerySearchCore.chipText('lol-type', 'replay')"
        ),
        "LoL Type: Replay"
    );
}

#[test]
fn present_categories_limit_value_suggestions() {
    let mut ctx = context();
    assert_eq!(
        eval_json(
            &mut ctx,
            "GallerySearchCore.matchingValues('lol-type', '', ['aram', 'replay']).map((item) => item.value)"
        ),
        "[\"aram\",\"replay\"]"
    );
}

#[test]
fn ordinary_search_text_is_not_a_filter_prefix() {
    let mut ctx = context();
    assert_eq!(
        eval_json(&mut ctx, "GallerySearchCore.inspect('jinx').kind"),
        "\"query\""
    );
    assert_eq!(
        eval_json(&mut ctx, "GallerySearchCore.matchingFilters('jinx')"),
        "[]"
    );
}
