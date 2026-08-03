use std::fs;
use std::path::{Path, PathBuf};

fn package_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(relative: &str) -> String {
    let path = package_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn library_surface_is_controller_owned_bounded_and_source_aware() {
    let library = read("ui/library.slint");
    let app = read("ui/app.slint");

    for contract in [
        "export struct LibraryItem",
        "export enum CatalogSource",
        "export enum CatalogFilter",
        "export enum CatalogSort",
        "export enum CatalogGroup",
        "CatalogFilter { all, replay, session, trim, marked }",
        "CatalogSort { newest, oldest, largest, marks }",
        "CatalogGroup { smart, day, game, session, ungrouped }",
        "in property <[LibraryItem]> rows: []",
        "private property <int> max-visible-rows: 60",
        "index < root.max-visible-rows",
        "source == CatalogSource.cloud",
        "selected-count",
        "range-text",
        "game-badge",
        "marker-badge",
        "outcome-badge",
        "upload-badge",
        "row.active",
        "row.selected",
        "accessible-role: list",
        "accessible-role: list-item",
        "accessible-checked: row.selected",
        "accessible-expanded: root.context-row == index",
    ] {
        assert!(
            library.contains(contract),
            "missing library surface contract: {contract}"
        );
    }

    for control in [
        "text: \"Local\"",
        "text: \"Cloud\"",
        "accessible-label: \"Search clips\"",
        "accessible-label: \"Clip type filter\"",
        "accessible-label: \"Sort clips\"",
        "accessible-label: \"Group clips\"",
        "text: \"Previous\"",
        "text: \"Next\"",
        "? \"Exit selection\" : \"Select multiple\"",
        "text: \"Select page\"",
        "text: \"Clear\"",
        "text: \"Open\"",
        "text: \"Rename title\"",
        "text: \"Rename file\"",
        "text: \"Delete\"",
        "text: \"Upload\"",
        "text: \"Reveal in Explorer\"",
        "text: \"Copy public link\"",
        "text: \"Open public link\"",
        "text: \"Cancel upload\"",
    ] {
        assert!(
            library.contains(control),
            "missing library control: {control}"
        );
    }

    for callback in [
        "callback catalog-set-source(source: CatalogSource)",
        "callback catalog-set-query(query: string)",
        "callback catalog-set-filter(filter: CatalogFilter)",
        "callback catalog-set-sort(sort: CatalogSort)",
        "callback catalog-set-group(group: CatalogGroup)",
        "callback catalog-previous-page()",
        "callback catalog-next-page()",
        "callback catalog-refresh()",
        "callback catalog-open-row(row-index: int)",
        "callback catalog-toggle-row(row-index: int)",
        "callback catalog-open-context(row-index: int)",
        "callback catalog-enter-selection()",
        "callback catalog-exit-selection()",
        "callback catalog-select-visible()",
        "callback catalog-clear-selection()",
        "callback catalog-delete-selection()",
        "callback catalog-reveal-context()",
        "callback catalog-upload-context()",
        "callback catalog-rename-title-context()",
        "callback catalog-rename-file-context()",
        "callback catalog-delete-context()",
        "callback catalog-cancel-upload-context()",
        "callback catalog-copy-link-context()",
        "callback catalog-open-link-context()",
    ] {
        assert!(
            app.contains(callback),
            "missing root callback contract: {callback}"
        );
    }

    assert!(!app.contains("catalog-revision"));
    assert!(app.contains("in-out property <int> catalog-focused-row"));
    assert!(app.contains("in property <[LibraryItem]> library-items: []"));
    assert!(!library.contains("representative"));
    assert!(!library.contains("Clip 07"));
}

#[test]
fn dialogs_cover_bounded_upload_and_destructive_flows() {
    let dialogs = read("ui/dialogs.slint");
    let app = read("ui/app.slint");

    for contract in [
        "export enum CatalogDialogKind",
        "export enum CatalogDialogTextField",
        "export enum CatalogUploadVisibility",
        "export struct CatalogAudioTrack",
        "export struct CatalogDialogModel",
        "title-max-utf16: 140",
        "description-max-utf16: 5000",
        "delete-local-after-upload",
        "in property <[CatalogAudioTrack]> audio-tracks: []",
        "accessible-role: form",
        "accessible-label: \"Catalog dialog\"",
        "modal-scrim := TouchArea",
        "pointer-event(event)",
        "accessible-label: \"Upload title\"",
        "accessible-label: \"Upload description\"",
        "accessible-label: \"Upload visibility\"",
        "accessible-label: \"Include audio track \" + track.label",
        "visible: root.dialog.kind == CatalogDialogKind.partial-delete",
    ] {
        assert!(
            dialogs.contains(contract),
            "missing dialog contract: {contract}"
        );
    }

    for callback in [
        "callback catalog-confirm-dialog()",
        "callback catalog-cancel-dialog()",
        "callback catalog-set-dialog-text(field: CatalogDialogTextField, value: string)",
        "callback catalog-set-upload-visibility(visibility: CatalogUploadVisibility)",
        "callback catalog-set-audio-track(track-index: int, selected: bool)",
    ] {
        assert!(
            app.contains(callback),
            "missing dialog callback: {callback}"
        );
    }

    assert!(
        !dialogs.contains("delete-local-after-upload <=>"),
        "delete-local-after-upload is a saved Cloud setting, not an editable per-upload control"
    );
}

#[test]
fn dialog_scrim_is_full_window_and_underlying_pointer_actions_are_modal() {
    let dialogs = read("ui/dialogs.slint");
    let library = read("ui/library.slint");
    let app = read("ui/app.slint");
    let dialog_instance = app
        .split_once("CatalogDialogs {")
        .map(|(_, instance)| instance)
        .expect("root must instantiate the modal dialog layer");

    for contract in [
        "modal-scrim := TouchArea",
        "width: parent.width",
        "height: parent.height",
        "enabled: root.dialog.open",
        "pointer-event(event) => { }",
    ] {
        assert!(
            dialogs.contains(contract),
            "missing modal scrim contract: {contract}"
        );
    }
    for contract in ["x: 0", "y: 0", "width: root.width", "height: root.height"] {
        assert!(
            dialog_instance.contains(contract),
            "dialog is not full-window: {contract}"
        );
    }
    assert!(app.contains("dialog-open: root.catalog-dialog.open"));
    for contract in [
        "root.selection-mode && root.source == CatalogSource.local && !root.dialog-open",
        "accessible-action-default => { if (!root.dialog-open)",
        "enabled: !root.dialog-open",
        "root.context-row >= 0 && !root.dialog-open",
    ] {
        assert!(
            library.contains(contract),
            "underlying pointer path is not modal: {contract}"
        );
    }
}
