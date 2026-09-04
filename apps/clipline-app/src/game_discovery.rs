use std::path::{Path, PathBuf};

use clipline_capture::windows::{enumerate_capturable_windows, CapturableWindow};

use crate::game_icon;
use crate::settings::CustomGameSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectedGameSource {
    Steam,
    RunningWindow,
    SteamAndRunningWindow,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DetectedGameCandidate {
    pub id_hint: String,
    pub name: String,
    pub source: DetectedGameSource,
    pub steam_app_id: Option<u32>,
    pub install_dir: Option<String>,
    pub exe_name: String,
    pub process_path: Option<String>,
    pub window_title: String,
    pub icon: Option<String>,
    pub confidence: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SteamAppManifest {
    pub app_id: u32,
    pub name: String,
    pub install_dir_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SteamApp {
    app_id: u32,
    name: String,
    install_dir: PathBuf,
    exe_name: Option<String>,
    process_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VdfEntry {
    Pair { key: String, value: String },
    Object { key: String, entries: Vec<VdfEntry> },
}

pub fn detect_installed_games(
    existing_custom_games: &[CustomGameSettings],
) -> Vec<DetectedGameCandidate> {
    let steam_apps = steam_install_roots()
        .and_then(|roots| steam_apps_from_roots(&roots))
        .unwrap_or_default();
    candidates_from_sources(
        steam_apps,
        enumerate_capturable_windows(),
        existing_custom_games,
        game_icon::extract_exe_icon_data_url,
    )
}

fn candidates_from_sources<F>(
    steam_apps: Vec<SteamApp>,
    windows: Vec<CapturableWindow>,
    existing_custom_games: &[CustomGameSettings],
    icon_for_path: F,
) -> Vec<DetectedGameCandidate>
where
    F: Fn(&str) -> Option<String>,
{
    let mut candidates = Vec::new();

    for app in &steam_apps {
        if let (Some(exe_name), Some(process_path)) = (&app.exe_name, &app.process_path) {
            let process_path = process_path.to_string_lossy().into_owned();
            let icon = icon_for_path(&process_path);
            candidates.push(DetectedGameCandidate {
                id_hint: format!("steam-{}", app.app_id),
                name: app.name.clone(),
                source: DetectedGameSource::Steam,
                steam_app_id: Some(app.app_id),
                install_dir: Some(app.install_dir.to_string_lossy().into_owned()),
                exe_name: exe_name.clone(),
                process_path: Some(process_path),
                window_title: String::new(),
                icon,
                confidence: 75,
            });
        }
    }

    for window in windows
        .into_iter()
        .filter(|window| !is_noise_window(window))
    {
        let Some(window_candidate) = candidate_from_window(&window, &icon_for_path) else {
            continue;
        };
        let window_path = window_candidate.process_path.as_deref();
        if let Some(existing) = candidates
            .iter_mut()
            .find(|candidate| paths_match(candidate.process_path.as_deref(), window_path))
        {
            upgrade_candidate_with_window(existing, &window_candidate);
            continue;
        }
        if let Some(app) = window_path.and_then(|path| {
            steam_apps
                .iter()
                .find(|app| is_path_within(path, &app.install_dir))
        }) {
            candidates.push(candidate_from_steam_window(
                app,
                &window_candidate,
                &icon_for_path,
            ));
        }
    }

    dedupe_candidates(candidates, existing_custom_games)
}

fn candidate_from_window<F>(
    window: &CapturableWindow,
    icon_for_path: &F,
) -> Option<DetectedGameCandidate>
where
    F: Fn(&str) -> Option<String>,
{
    let name = game_name_from_window(window);
    let exe_name = window.exe_name.trim().to_owned();
    let process_path = window
        .exe_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned);
    let window_title = window.title.trim().to_owned();
    let id_hint = process_path
        .as_ref()
        .map(|path| format!("window-{}", normalize_path_string(path)))
        .unwrap_or_else(|| {
            format!(
                "window-{}-{}",
                window.process_id,
                normalized_name_key(&format!("{name}{exe_name}{window_title}"))
            )
        });
    let icon = process_path.as_deref().and_then(icon_for_path);
    let confidence = if process_path.is_some() { 90 } else { 60 };
    let candidate = DetectedGameCandidate {
        id_hint,
        name,
        source: DetectedGameSource::RunningWindow,
        steam_app_id: None,
        install_dir: None,
        exe_name,
        process_path,
        window_title,
        icon,
        confidence,
    };
    has_match_identity(&candidate).then_some(candidate)
}

fn candidate_from_steam_window<F>(
    app: &SteamApp,
    window: &DetectedGameCandidate,
    icon_for_path: &F,
) -> DetectedGameCandidate
where
    F: Fn(&str) -> Option<String>,
{
    let process_path = window.process_path.clone();
    let icon = process_path
        .as_deref()
        .and_then(icon_for_path)
        .or_else(|| window.icon.clone());
    DetectedGameCandidate {
        id_hint: format!("steam-{}", app.app_id),
        name: app.name.clone(),
        source: DetectedGameSource::SteamAndRunningWindow,
        steam_app_id: Some(app.app_id),
        install_dir: Some(app.install_dir.to_string_lossy().into_owned()),
        exe_name: app
            .exe_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| window.exe_name.clone()),
        process_path,
        window_title: window.window_title.clone(),
        icon,
        confidence: 95,
    }
}

fn upgrade_candidate_with_window(
    candidate: &mut DetectedGameCandidate,
    window: &DetectedGameCandidate,
) {
    if matches!(candidate.source, DetectedGameSource::Steam) {
        candidate.source = DetectedGameSource::SteamAndRunningWindow;
    }
    if candidate.window_title.trim().is_empty() {
        candidate.window_title = window.window_title.clone();
    }
    if candidate.exe_name.trim().is_empty() {
        candidate.exe_name = window.exe_name.clone();
    }
    if candidate.process_path.is_none() {
        candidate.process_path = window.process_path.clone();
    }
    if candidate.icon.is_none() {
        candidate.icon = window.icon.clone();
    }
    candidate.confidence = candidate.confidence.max(95);
}

fn paths_match(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => normalize_path_string(left) == normalize_path_string(right),
        _ => false,
    }
}

fn dedupe_candidates(
    candidates: Vec<DetectedGameCandidate>,
    existing_custom_games: &[CustomGameSettings],
) -> Vec<DetectedGameCandidate> {
    let mut candidates: Vec<_> = candidates
        .into_iter()
        .filter(has_match_identity)
        .filter(|candidate| {
            !existing_custom_games
                .iter()
                .any(|existing| matches_existing_custom_game(candidate, existing))
        })
        .collect();

    candidates.sort_by(|left, right| {
        sort_bucket(left)
            .cmp(&sort_bucket(right))
            .then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            })
            .then_with(|| left.id_hint.cmp(&right.id_hint))
    });

    let mut deduped = Vec::new();
    for candidate in candidates {
        if !deduped
            .iter()
            .any(|existing| same_candidate(existing, &candidate))
        {
            deduped.push(candidate);
        }
    }
    deduped
}

fn has_match_identity(candidate: &DetectedGameCandidate) -> bool {
    usable_process_path(candidate.process_path.as_deref()).is_some()
        || !candidate.exe_name.trim().is_empty()
        || !normalized_name_key(&candidate.name).is_empty()
}

fn matches_existing_custom_game(
    candidate: &DetectedGameCandidate,
    existing: &CustomGameSettings,
) -> bool {
    let candidate_path = usable_process_path(candidate.process_path.as_deref());
    let existing_path = usable_process_path(existing.process_path.as_deref());
    if let (Some(candidate_path), Some(existing_path)) = (&candidate_path, &existing_path) {
        return candidate_path == existing_path;
    }

    let candidate_exe = candidate.exe_name.trim();
    let existing_exe = existing.exe_name.trim();
    if !candidate_exe.is_empty()
        && !existing_exe.is_empty()
        && candidate_exe.eq_ignore_ascii_case(existing_exe)
    {
        return true;
    }

    let candidate_name = normalized_name_key(&candidate.name);
    let existing_name = normalized_name_key(&existing.name);
    !candidate_name.is_empty() && candidate_name == existing_name
}

fn same_candidate(left: &DetectedGameCandidate, right: &DetectedGameCandidate) -> bool {
    let left_path = usable_process_path(left.process_path.as_deref());
    let right_path = usable_process_path(right.process_path.as_deref());
    if let (Some(left_path), Some(right_path)) = (&left_path, &right_path) {
        return left_path == right_path;
    }

    !left.exe_name.trim().is_empty()
        && !right.exe_name.trim().is_empty()
        && left
            .exe_name
            .trim()
            .eq_ignore_ascii_case(right.exe_name.trim())
}

fn usable_process_path(path: Option<&str>) -> Option<String> {
    path.map(normalize_path_string)
        .filter(|path| !path.is_empty())
}

fn sort_bucket(candidate: &DetectedGameCandidate) -> u8 {
    match candidate.source {
        DetectedGameSource::RunningWindow => {
            if candidate
                .process_path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
            {
                0
            } else {
                3
            }
        }
        DetectedGameSource::SteamAndRunningWindow => 1,
        DetectedGameSource::Steam => 2,
    }
}

fn is_path_within(path: &str, parent: &Path) -> bool {
    let path = normalize_path_string(path);
    let parent = normalize_path_string(&parent.to_string_lossy());
    if path.is_empty() || parent.is_empty() {
        return false;
    }
    path == parent
        || path
            .strip_prefix(&parent)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn normalize_path_string(path: &str) -> String {
    let mut normalized = path.trim().replace('/', "\\").to_ascii_lowercase();
    while normalized.ends_with('\\') && !normalized.ends_with(":\\") {
        normalized.pop();
    }
    normalized
}

fn game_name_from_window(window: &CapturableWindow) -> String {
    let exe_name = window.exe_name.trim();
    if !exe_name.is_empty() {
        let lower = exe_name.to_ascii_lowercase();
        if lower.ends_with(".exe") {
            return exe_name[..exe_name.len() - 4].to_owned();
        }
        return exe_name.to_owned();
    }
    window.title.trim().to_owned()
}

fn is_noise_window(window: &CapturableWindow) -> bool {
    let exe_name = window.exe_name.trim().to_ascii_lowercase();
    let title = window.title.trim().to_ascii_lowercase();
    is_helper_exe_name(&exe_name)
        || matches!(
            exe_name.as_str(),
            "arc.exe"
                | "battle.net.exe"
                | "brave.exe"
                | "chrome.exe"
                | "discord.exe"
                | "eadesktop.exe"
                | "ealauncher.exe"
                | "epicgameslauncher.exe"
                | "firefox.exe"
                | "goggalaxy.exe"
                | "itch.exe"
                | "leagueclientux.exe"
                | "librewolf.exe"
                | "msedge.exe"
                | "opera.exe"
                | "origin.exe"
                | "riotclientservices.exe"
                | "riotclientux.exe"
                | "slack.exe"
                | "steam.exe"
                | "steamwebhelper.exe"
                | "teams.exe"
                | "ubisoftconnect.exe"
                | "uplay.exe"
                | "vivaldi.exe"
                | "waterfox.exe"
                | "zoom.exe"
        )
        || title.contains("launcher")
        || title.contains("updater")
}

fn steam_install_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    if let Some(path) = query_reg_sz(r"HKCU\Software\Valve\Steam", "SteamPath") {
        add_unique_path(&mut roots, PathBuf::from(path.replace('/', "\\")));
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        add_unique_path(&mut roots, PathBuf::from(program_files_x86).join("Steam"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        add_unique_path(&mut roots, PathBuf::from(program_files).join("Steam"));
    }
    Ok(roots.into_iter().filter(|path| path.exists()).collect())
}

fn query_reg_sz(key: &str, value_name: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("reg.exe")
        .args(["query", key, "/v", value_name])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_reg_sz_output(&String::from_utf8_lossy(&output.stdout), value_name)
}

fn parse_reg_sz_output(output: &str, value_name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let name = fields.next()?;
        let kind = fields.next()?;
        if !name.eq_ignore_ascii_case(value_name) || !kind.eq_ignore_ascii_case("REG_SZ") {
            return None;
        }
        let value = fields.collect::<Vec<_>>().join(" ");
        (!value.is_empty()).then_some(value)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VdfToken {
    String(String),
    Open,
    Close,
}

fn parse_vdf(input: &str) -> Result<Vec<VdfEntry>, String> {
    let tokens = tokenize_vdf(input)?;
    let mut index = 0;
    parse_vdf_entries(&tokens, &mut index, false)
}

fn tokenize_vdf(input: &str) -> Result<Vec<VdfToken>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '"' => {
                let mut value = String::new();
                let mut terminated = false;
                while let Some((_, string_ch)) = chars.next() {
                    match string_ch {
                        '"' => {
                            terminated = true;
                            break;
                        }
                        '\\' => {
                            let Some((_, escaped)) = chars.next() else {
                                return Err("unterminated string".into());
                            };
                            value.push(match escaped {
                                '\\' => '\\',
                                '"' => '"',
                                'n' => '\n',
                                'r' => '\r',
                                't' => '\t',
                                other => other,
                            });
                        }
                        other => value.push(other),
                    }
                }
                if !terminated {
                    return Err("unterminated string".into());
                }
                tokens.push(VdfToken::String(value));
            }
            '{' => tokens.push(VdfToken::Open),
            '}' => tokens.push(VdfToken::Close),
            '/' if chars.peek().is_some_and(|(_, next)| *next == '/') => {
                for (_, comment_ch) in chars.by_ref() {
                    if comment_ch == '\n' {
                        break;
                    }
                }
            }
            ch if ch.is_whitespace() => {}
            other => {
                return Err(format!(
                    "unexpected VDF character {other:?} at byte {offset}"
                ));
            }
        }
    }
    Ok(tokens)
}

fn parse_vdf_entries(
    tokens: &[VdfToken],
    index: &mut usize,
    in_object: bool,
) -> Result<Vec<VdfEntry>, String> {
    let mut entries = Vec::new();
    while let Some(token) = tokens.get(*index) {
        match token {
            VdfToken::Close => {
                *index += 1;
                if in_object {
                    return Ok(entries);
                }
                return Err("unexpected object close".into());
            }
            VdfToken::Open => return Err("unexpected object open".into()),
            VdfToken::String(key) => {
                let key = key.clone();
                *index += 1;
                let Some(value_token) = tokens.get(*index) else {
                    return Err(format!("missing value for key {key:?}"));
                };
                match value_token {
                    VdfToken::String(value) => {
                        entries.push(VdfEntry::Pair {
                            key,
                            value: value.clone(),
                        });
                        *index += 1;
                    }
                    VdfToken::Open => {
                        *index += 1;
                        let object_entries = parse_vdf_entries(tokens, index, true)?;
                        entries.push(VdfEntry::Object {
                            key,
                            entries: object_entries,
                        });
                    }
                    VdfToken::Close => return Err(format!("missing value for key {key:?}")),
                }
            }
        }
    }
    if in_object {
        Err("unterminated object".into())
    } else {
        Ok(entries)
    }
}

fn library_paths_from_vdf(entries: &[VdfEntry]) -> Vec<PathBuf> {
    let Some(libraryfolders) = find_object(entries, "libraryfolders") else {
        return Vec::new();
    };
    libraryfolders
        .iter()
        .filter_map(|entry| match entry {
            VdfEntry::Pair { value, .. } => Some(PathBuf::from(value)),
            VdfEntry::Object { entries, .. } => find_pair(entries, "path").map(PathBuf::from),
        })
        .collect()
}

fn steam_app_from_manifest(entries: &[VdfEntry]) -> Option<SteamAppManifest> {
    let app_state = find_object(entries, "AppState")?;
    Some(SteamAppManifest {
        app_id: find_pair(app_state, "appid")?.parse().ok()?,
        name: find_pair(app_state, "name")?.to_owned(),
        install_dir_name: find_pair(app_state, "installdir")?.to_owned(),
    })
}

fn steam_apps_from_roots(steam_roots: &[PathBuf]) -> Result<Vec<SteamApp>, String> {
    let mut libraries = Vec::new();
    for root in steam_roots {
        for library in steam_libraries_from_root(root)? {
            add_unique_path(&mut libraries, library);
        }
    }

    let mut apps = Vec::new();
    for library in libraries {
        let steamapps_dir = library.join("steamapps");
        let Ok(entries) = std::fs::read_dir(&steamapps_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !file_name.starts_with("appmanifest_") || !file_name.ends_with(".acf") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(manifest) = parse_vdf(&contents)
                .ok()
                .and_then(|entries| steam_app_from_manifest(&entries))
            else {
                continue;
            };
            let install_dir = steamapps_dir
                .join("common")
                .join(&manifest.install_dir_name);
            let process_path = infer_executable_path(&install_dir, &manifest.name);
            let exe_name = process_path
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_owned);
            apps.push(SteamApp {
                app_id: manifest.app_id,
                name: manifest.name,
                install_dir,
                exe_name,
                process_path,
            });
        }
    }

    apps.sort_by_key(|app| app.name.to_lowercase());
    Ok(apps)
}

fn steam_libraries_from_root(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut libraries = vec![root.to_path_buf()];
    let libraryfolders = root.join("steamapps").join("libraryfolders.vdf");
    let contents = match std::fs::read_to_string(&libraryfolders) {
        Ok(contents) => contents,
        Err(_) => return Ok(libraries),
    };
    let Ok(entries) = parse_vdf(&contents) else {
        return Ok(libraries);
    };
    for library in library_paths_from_vdf(&entries) {
        add_unique_path(&mut libraries, library);
    }
    Ok(libraries)
}

fn add_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    let key = path_key(&path);
    if !paths.iter().any(|existing| path_key(existing) == key) {
        paths.push(path);
    }
}

fn infer_executable_path(install_dir: &Path, game_name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    collect_exe_candidates(install_dir, 0, &mut candidates);
    candidates.sort_by_key(|path| path_key(path));
    candidates
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !is_helper_exe_name(name))
        })
        .max_by_key(|path| executable_score(path, install_dir, game_name))
}

fn collect_exe_candidates(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 || out.len() >= 200 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= 200 {
            return;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_exe_candidates(&path, depth + 1, out);
            continue;
        }
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            out.push(path);
        }
    }
}

fn executable_score(path: &Path, install_dir: &Path, game_name: &str) -> u16 {
    let mut score = 10;
    let exe_key = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(normalized_name_key)
        .unwrap_or_default();
    let game_key = normalized_name_key(game_name);
    let install_key = install_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(normalized_name_key)
        .unwrap_or_default();

    if !game_key.is_empty() && exe_key == game_key {
        score += 80;
    }
    if !install_key.is_empty() && exe_key == install_key {
        score += 70;
    }
    if !game_key.is_empty() && (exe_key.contains(&game_key) || game_key.contains(&exe_key)) {
        score += 35;
    }
    if path.parent().is_some_and(|parent| parent == install_dir) {
        score += 20;
    }
    score
}

fn is_helper_exe_name(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    let key = normalized_name_key(name);
    if key == "crash"
        || key.contains("crashhandler")
        || key.contains("crashpad")
        || key.contains("crashreporter")
        || key == "launcher"
        || key.ends_with("launcher")
    {
        return true;
    }
    [
        "redist",
        "redistributable",
        "vcredist",
        "dxsetup",
        "setup",
        "install",
        "unins",
        "uninstall",
        "updater",
        "launcherhelper",
        "helper",
    ]
    .iter()
    .any(|helper| key.contains(helper))
}

fn normalized_name_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

fn find_object<'a>(entries: &'a [VdfEntry], key: &str) -> Option<&'a [VdfEntry]> {
    entries.iter().find_map(|entry| match entry {
        VdfEntry::Object {
            key: entry_key,
            entries,
        } if entry_key.eq_ignore_ascii_case(key) => Some(entries.as_slice()),
        _ => None,
    })
}

fn find_pair<'a>(entries: &'a [VdfEntry], key: &str) -> Option<&'a str> {
    entries.iter().find_map(|entry| match entry {
        VdfEntry::Pair {
            key: entry_key,
            value,
        } if entry_key.eq_ignore_ascii_case(key) => Some(value.as_str()),
        _ => None,
    })
}

#[cfg(test)]
mod tests;
