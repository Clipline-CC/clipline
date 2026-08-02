use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const REQUIRED_SURFACES: &[&str] = &[
    "surface:persistent-shell",
    "surface:local-library",
    "surface:cloud-library",
    "surface:review",
    "surface:settings-general",
    "surface:settings-capture",
    "surface:settings-recording",
    "surface:settings-games",
    "surface:settings-storage",
    "surface:settings-cloud",
    "surface:settings-hotkeys",
    "surface:settings-support",
];

const REQUIRED_DIALOGS: &[&str] = &[
    "dialog:delete-confirmation",
    "dialog:quit-confirmation",
    "dialog:update-available",
    "dialog:elevated-game-warning",
    "dialog:cloud-upload",
    "dialog:detected-games",
    "dialog:running-window",
    "dialog:file-rename",
    "dialog:game-plugin-settings",
    "dialog:shortcut-guide",
    "dialog:media-folder-picker",
    "dialog:replay-cache-folder-picker",
    "dialog:support-bundle-picker",
];

const REQUIRED_SHORTCUTS: &[&str] = &[
    "shortcut:global-save-replay",
    "shortcut:play-pause",
    "shortcut:seek-five-seconds",
    "shortcut:seek-one-second",
    "shortcut:seek-source-frames",
    "shortcut:seek-tenth-second",
    "shortcut:set-trim-in-out",
    "shortcut:marker-navigation",
    "shortcut:edit-point-navigation",
    "shortcut:timeline-zoom",
    "shortcut:timeline-fit",
    "shortcut:clip-boundary",
    "shortcut:toggle-snapping",
    "shortcut:fullscreen",
    "shortcut:escape-context",
    "shortcut:shortcut-guide",
    "shortcut:library-select-all",
    "shortcut:settings-tab-navigation",
];

const REQUIRED_GESTURES: &[&str] = &[
    "gesture:timeline-seek",
    "gesture:trim-edge-drag",
    "gesture:trim-selection-drag",
    "gesture:snap-bypass",
    "gesture:timeline-wheel-zoom",
    "gesture:timeline-pan",
    "gesture:overview-pan",
    "gesture:overview-edge-zoom",
    "gesture:marker-seek",
    "gesture:play-block-seek",
    "gesture:shift-fit-whole-clip",
    "gesture:shift-copy-original",
    "gesture:context-menu",
    "gesture:stage-overlay-activity",
];

const REQUIRED_TRAY_AND_LIFECYCLE: &[&str] = &[
    "tray:open",
    "tray:save-replay",
    "tray:open-diagnostics",
    "tray:quit",
    "tray:left-click-open",
    "lifecycle:normal-launch",
    "lifecycle:autostart-tray",
    "lifecycle:single-instance-reveal",
    "lifecycle:close-to-tray-or-quit",
    "lifecycle:minimize-to-tray-or-taskbar",
    "lifecycle:foreground-bootstrap-snapshot",
];

const REQUIRED_UPDATER_AND_PACKAGE: &[&str] = &[
    "updater:silent-check",
    "updater:manual-check",
    "updater:install",
    "updater:signature-verification",
    "package:regular-nsis",
    "package:standalone-nsis",
    "package:webview2-runtime",
    "package:ffmpeg-resource",
    "package:product-identity",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("app crate should be nested under workspace/apps")
        .to_path_buf()
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref())
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.as_ref().display()))
}

fn registered_commands(app_rs: &str) -> Vec<String> {
    let marker = "tauri::generate_handler![";
    let tail = app_rs
        .split_once(marker)
        .expect("production app must register a Tauri command handler")
        .1;
    let body = tail
        .split_once(']')
        .expect("Tauri command handler list must terminate")
        .0;

    body.lines()
        .map(str::trim)
        .map(|line| line.trim_end_matches(','))
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .map(|symbol| symbol.rsplit("::").next().unwrap().to_owned())
        .inspect(|command| {
            assert!(
                is_rust_identifier(command),
                "unexpected syntax in generate_handler! entry: {command}"
            );
        })
        .collect()
}

fn is_rust_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn rust_files_below(directory: &Path) -> Vec<PathBuf> {
    let mut output = Vec::new();
    for entry in fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to enumerate {}: {error}", directory.display()))
    {
        let path = entry.expect("read Rust source directory entry").path();
        if path.is_dir() {
            output.extend(rust_files_below(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
    output.sort();
    output
}

fn production_source(source: &str) -> &str {
    source.split("\n#[cfg(test)]").next().unwrap_or(source)
}

fn string_constants(source: &str) -> BTreeMap<String, String> {
    let mut output = BTreeMap::new();
    for line in source.lines() {
        let Some(const_index) = line.find("const ") else {
            continue;
        };
        let tail = &line[const_index + "const ".len()..];
        let Some(colon) = tail.find(':') else {
            continue;
        };
        let name = tail[..colon].trim();
        if !is_rust_identifier(name) || !tail[colon..].contains("&str") {
            continue;
        }
        let Some(equals) = tail.find('=') else {
            continue;
        };
        let value = tail[equals + 1..].trim();
        let Some(value) = value.strip_prefix('"') else {
            continue;
        };
        let Some(end) = value.find('"') else {
            continue;
        };
        output.insert(name.to_owned(), value[..end].to_owned());
    }
    output
}

fn call_argument(source_after_open_paren: &str, wanted_index: usize) -> Option<&str> {
    let mut start = 0;
    let mut index = 0;
    let mut depth = 0_u32;
    let mut quote = None;
    let mut escaped = false;

    for (offset, character) in source_after_open_paren.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '(' | '[' | '{' => depth += 1,
            ')' if depth == 0 => {
                return (index == wanted_index)
                    .then(|| source_after_open_paren[start..offset].trim())
            }
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                if index == wanted_index {
                    return Some(source_after_open_paren[start..offset].trim());
                }
                index += 1;
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    None
}

fn resolve_event_argument(
    expression: &str,
    constants: &BTreeMap<String, String>,
    path: &Path,
) -> String {
    if let Some(literal) = expression.strip_prefix('"') {
        let end = literal.find('"').unwrap_or_else(|| {
            panic!(
                "unterminated event string in {}: {expression}",
                path.display()
            )
        });
        return literal[..end].to_owned();
    }
    let name = expression.rsplit("::").next().unwrap_or(expression).trim();
    constants.get(name).cloned().unwrap_or_else(|| {
        panic!(
            "unresolved event argument in {}: {expression}; use a string constant or extend the migration extractor",
            path.display()
        )
    })
}

fn rust_event_names(root: &Path) -> BTreeSet<String> {
    let files = rust_files_below(&root.join("apps/clipline-app/src"));
    let sources = files
        .iter()
        .map(|path| (path.clone(), read(path)))
        .collect::<Vec<_>>();
    let constants = sources
        .iter()
        .flat_map(|(_, source)| string_constants(production_source(source)))
        .collect::<BTreeMap<_, _>>();
    let mut output = BTreeSet::new();

    for (path, source) in &sources {
        let source = production_source(source);
        for (call, argument_index) in [(".emit(", 0), (".emit_all(", 0), (".emit_to(", 1)] {
            let mut remaining = source;
            while let Some(index) = remaining.find(call) {
                remaining = &remaining[index + call.len()..];
                let argument = call_argument(remaining, argument_index).unwrap_or_else(|| {
                    panic!(
                        "could not parse {call} event argument in {}",
                        path.display()
                    )
                });
                if argument == "emission.name" && path.ends_with("src/desktop/tauri_sink.rs") {
                    continue;
                }
                output.insert(resolve_event_argument(argument, &constants, path));
            }
        }
    }
    output
}

fn javascript_string_arguments(source: &str, call: &str) -> BTreeSet<String> {
    let mut output = BTreeSet::new();
    let mut remaining = source;

    while let Some(index) = remaining.find(call) {
        remaining = &remaining[index + call.len()..];
        let trimmed = remaining.trim_start();
        let Some(value) = trimmed.strip_prefix('"') else {
            continue;
        };
        let Some(end) = value.find('"') else {
            continue;
        };
        output.insert(value[..end].to_owned());
        remaining = &value[end + 1..];
    }
    output
}

fn ledger_rows(ledger: &str) -> BTreeMap<String, Vec<String>> {
    let mut rows = BTreeMap::new();
    for line in ledger.lines().filter(|line| line.starts_with("| `")) {
        let cells = line
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            cells.len(),
            5,
            "ledger row must have token, contract, owner, acceptance, and status: {line}"
        );
        let token = cells[0].trim_matches('`').to_owned();
        assert!(
            !cells[1].is_empty(),
            "{token} must describe its current contract"
        );
        assert!(!cells[2].is_empty(), "{token} must name a target owner");
        assert!(
            cells[2].starts_with('M')
                && cells[2]
                    .chars()
                    .nth(1)
                    .is_some_and(|character| character.is_ascii_digit()),
            "{token} owner must start with a milestone such as M5: {}",
            cells[2]
        );
        assert!(
            !cells[3].is_empty(),
            "{token} must define acceptance evidence"
        );
        assert!(
            [
                "`not_started`",
                "`in_progress`",
                "`implemented`",
                "`verified`",
                "`waived`"
            ]
            .contains(&cells[4].as_str()),
            "{token} has an invalid migration status: {}",
            cells[4]
        );
        assert!(
            rows.insert(token.clone(), cells).is_none(),
            "duplicate Slint parity token: {token}"
        );
    }
    rows
}

fn assert_tokens(rows: &BTreeMap<String, Vec<String>>, tokens: impl IntoIterator<Item = String>) {
    let missing = tokens
        .into_iter()
        .filter(|token| !rows.contains_key(token))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "Slint parity ledger is missing tokens: {}",
        missing.join(", ")
    );
}

#[test]
fn parity_ledger_covers_the_shipping_frontend_boundary() {
    let root = workspace_root();
    let ledger = read(root.join("docs/slint/parity-ledger.md"));
    let rows = ledger_rows(&ledger);
    let app_rs = read(root.join("apps/clipline-app/src/app.rs"));

    let commands = registered_commands(&app_rs);
    assert_eq!(
        commands.len(),
        60,
        "review new/removed production commands and update the ledger baseline"
    );
    assert_tokens(
        &rows,
        commands
            .into_iter()
            .map(|command| format!("command:{command}")),
    );

    let mut events = rust_event_names(&root);
    events.extend(javascript_string_arguments(
        &read(root.join("apps/clipline-app/ui/main.js")),
        "listen(",
    ));
    assert_tokens(&rows, events.iter().map(|event| format!("event:{event}")));

    let code_commands = registered_commands(&app_rs)
        .into_iter()
        .map(|command| format!("command:{command}"))
        .collect::<BTreeSet<_>>();
    let code_events = events
        .into_iter()
        .map(|event| format!("event:{event}"))
        .collect::<BTreeSet<_>>();
    for token in rows.keys().filter(|token| token.starts_with("command:")) {
        assert!(
            code_commands.contains(token),
            "stale command row no longer registered in production: {token}"
        );
    }
    for token in rows.keys().filter(|token| token.starts_with("event:")) {
        assert!(
            code_events.contains(token),
            "stale event row no longer emitted or listened to: {token}"
        );
    }

    let required = REQUIRED_SURFACES
        .iter()
        .chain(REQUIRED_DIALOGS)
        .chain(REQUIRED_SHORTCUTS)
        .chain(REQUIRED_GESTURES)
        .chain(REQUIRED_TRAY_AND_LIFECYCLE)
        .chain(REQUIRED_UPDATER_AND_PACKAGE)
        .map(|token| (*token).to_owned());
    assert_tokens(&rows, required);

    assert!(
        rows.len() >= 140,
        "ledger unexpectedly lost migration surface rows: {} remain",
        rows.len()
    );

    assert!(
        ledger.contains("Baseline commit: `5eea6c3`"),
        "ledger must identify the audited develop baseline"
    );
    assert!(
        ledger.contains(
            "Status values: `not_started`, `in_progress`, `implemented`, `verified`, `waived`"
        ),
        "ledger must define explicit migration states and waiver handling"
    );
}
