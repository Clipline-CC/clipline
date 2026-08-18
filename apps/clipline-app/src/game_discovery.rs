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

pub(crate) fn is_noise_window(window: &CapturableWindow) -> bool {
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
        for manifest in read_steam_manifests(&steamapps_dir) {
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

/// Manifest-only Steam listing for the live game detector: app id, display
/// name, and install directory. Unlike `steam_apps_from_roots`, this never
/// infers executable paths — detector lookup is install-dir prefix matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SteamLaunchApp {
    pub(crate) app_id: u32,
    pub(crate) name: String,
    pub(crate) install_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct SteamLaunchCatalog {
    pub(crate) apps: Vec<SteamLaunchApp>,
    /// Every discovered `steamapps\\common` root; used to tell a
    /// Steam-rooted miss (refresh candidate) from an unrelated window.
    pub(crate) common_roots: Vec<PathBuf>,
    pub(crate) loaded_at: std::time::Instant,
}

impl SteamLaunchCatalog {
    pub(crate) fn scan() -> Self {
        Self::scan_from_roots(&steam_install_roots().unwrap_or_default())
    }

    pub(crate) fn scan_from_roots(roots: &[PathBuf]) -> Self {
        let mut apps = Vec::new();
        let mut common_roots = Vec::new();
        for root in roots {
            let Ok(libraries) = steam_libraries_from_root(root) else {
                continue;
            };
            for library in libraries {
                let steamapps_dir = library.join("steamapps");
                add_unique_path(&mut common_roots, steamapps_dir.join("common"));
                for manifest in read_steam_manifests(&steamapps_dir) {
                    apps.push(SteamLaunchApp {
                        app_id: manifest.app_id,
                        name: manifest.name,
                        install_dir: steamapps_dir
                            .join("common")
                            .join(&manifest.install_dir_name),
                    });
                }
            }
        }
        Self {
            apps,
            common_roots,
            loaded_at: std::time::Instant::now(),
        }
    }

    /// First catalog app whose install directory contains `exe_path`.
    pub(crate) fn find_by_exe_path(&self, exe_path: &str) -> Option<&SteamLaunchApp> {
        self.apps
            .iter()
            .find(|app| is_path_within(exe_path, &app.install_dir))
    }

    pub(crate) fn is_steam_rooted(&self, exe_path: &str) -> bool {
        self.common_roots
            .iter()
            .any(|root| is_path_within(exe_path, root))
    }
}

fn read_steam_manifests(steamapps_dir: &Path) -> Vec<SteamAppManifest> {
    let Ok(entries) = std::fs::read_dir(steamapps_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("appmanifest_") && name.ends_with(".acf"))
        })
        .filter_map(|path| {
            let contents = std::fs::read_to_string(&path).ok()?;
            let entries = parse_vdf(&contents).ok()?;
            steam_app_from_manifest(&entries)
        })
        .collect()
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
mod tests {
    use std::path::PathBuf;

    use clipline_capture::windows::CapturableWindow;
    use clipline_test_utils::TestDir;

    use crate::settings::CustomGameSettings;

    use super::*;

    fn window(title: &str, exe_name: &str, exe_path: Option<&str>) -> CapturableWindow {
        window_with_pid(101, title, exe_name, exe_path)
    }

    fn window_with_pid(
        process_id: u32,
        title: &str,
        exe_name: &str,
        exe_path: Option<&str>,
    ) -> CapturableWindow {
        CapturableWindow {
            handle: process_id as isize,
            title: title.into(),
            process_id,
            exe_name: exe_name.into(),
            exe_path: exe_path.map(str::to_string),
        }
    }

    fn vdf_path(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "\\\\")
    }

    fn steam_app(
        app_id: u32,
        name: &str,
        install_dir: &str,
        exe_name: &str,
        process_path: &str,
    ) -> SteamApp {
        SteamApp {
            app_id,
            name: name.into(),
            install_dir: PathBuf::from(install_dir),
            exe_name: Some(exe_name.into()),
            process_path: Some(PathBuf::from(process_path)),
        }
    }

    #[test]
    fn parses_libraryfolders_paths_from_keyvalue_vdf() {
        let input = r#"
            "libraryfolders"
            {
                "0"
                {
                    "path" "C:\\Program Files (x86)\\Steam"
                    "apps"
                    {
                        "570" "12345"
                    }
                }
                "1"
                {
                    "path" "D:\\SteamLibrary"
                }
                "2" "E:\\LegacySteamLibrary"
            }
        "#;

        let parsed = parse_vdf(input).expect("libraryfolders parses");
        assert_eq!(
            library_paths_from_vdf(&parsed),
            vec![
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from(r"D:\SteamLibrary"),
                PathBuf::from(r"E:\LegacySteamLibrary"),
            ]
        );
    }

    #[test]
    fn parses_appmanifest_core_fields() {
        let input = r#"
            "AppState"
            {
                "appid" "646570"
                "name" "Slay the Spire"
                "installdir" "SlayTheSpire"
                "StateFlags" "4"
            }
        "#;

        let parsed = parse_vdf(input).expect("appmanifest parses");
        let manifest = steam_app_from_manifest(&parsed).expect("manifest fields");

        assert_eq!(manifest.app_id, 646570);
        assert_eq!(manifest.name, "Slay the Spire");
        assert_eq!(manifest.install_dir_name, "SlayTheSpire");
    }

    #[test]
    fn malformed_vdf_returns_error() {
        let err = parse_vdf(r#""libraryfolders" { "0" { "path" "C:\\Steam""#)
            .expect_err("unclosed object should fail");
        assert!(
            err.contains("unterminated object"),
            "unexpected parse error: {err}"
        );
    }

    #[test]
    fn executable_score_adds_contains_bonus_for_exact_game_name() {
        let install_dir = PathBuf::from(r"C:\Games\InstallFolder");
        let exe_path = install_dir.join("Game Name.exe");

        assert_eq!(executable_score(&exe_path, &install_dir, "Game Name"), 145);
    }

    #[test]
    fn helper_filter_keeps_real_crash_named_games() {
        assert!(!is_helper_exe_name("Crashlands.exe"));
        assert!(!is_helper_exe_name("CrashBandicoot.exe"));
        assert!(is_helper_exe_name("UnityCrashHandler64.exe"));
        assert!(is_helper_exe_name("crashpad_handler.exe"));
        assert!(is_helper_exe_name("SkyrimSELauncher.exe"));
    }

    #[test]
    fn executable_inference_keeps_crash_named_game_exe() {
        let dir = TestDir::new("clipline-game-discovery", "crash-game-exe");
        let install_dir = dir.path().join("Crashlands");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("UnityCrashHandler64.exe"), b"").unwrap();
        std::fs::write(install_dir.join("Crashlands.exe"), b"").unwrap();

        let exe = infer_executable_path(&install_dir, "Crashlands")
            .expect("crash-named game executable should be considered");

        assert_eq!(
            exe.file_name().and_then(|name| name.to_str()),
            Some("Crashlands.exe")
        );
    }

    #[test]
    fn executable_inference_skips_launcher_exes() {
        let dir = TestDir::new("clipline-game-discovery", "launcher-exe");
        let install_dir = dir.path().join("SkyrimSE");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("SkyrimSE.exe"), b"").unwrap();
        std::fs::write(install_dir.join("SkyrimSELauncher.exe"), b"").unwrap();

        let exe = infer_executable_path(&install_dir, "Skyrim Special Edition")
            .expect("game executable should be selected");

        assert_eq!(
            exe.file_name().and_then(|name| name.to_str()),
            Some("SkyrimSE.exe")
        );
    }

    #[test]
    fn steam_catalog_reads_manifests_and_infers_best_executable() {
        let dir = TestDir::new("clipline-game-discovery", "steam-catalog");
        let steam_root = dir.path().join("Steam");
        let library = dir.path().join("Library");
        std::fs::create_dir_all(steam_root.join("steamapps")).unwrap();
        std::fs::create_dir_all(library.join("steamapps/common/SlayTheSpire")).unwrap();
        std::fs::write(
            steam_root.join("steamapps/libraryfolders.vdf"),
            format!(
                r#""libraryfolders" {{ "0" {{ "path" "{}" }} "1" {{ "path" "{}" }} }}"#,
                vdf_path(&steam_root),
                vdf_path(&library)
            ),
        )
        .unwrap();
        std::fs::write(
            library.join("steamapps/appmanifest_646570.acf"),
            r#""AppState" { "appid" "646570" "name" "Slay the Spire" "installdir" "SlayTheSpire" }"#,
        )
        .unwrap();
        std::fs::write(
            library.join("steamapps/common/SlayTheSpire/UnityCrashHandler64.exe"),
            b"",
        )
        .unwrap();
        std::fs::write(
            library.join("steamapps/common/SlayTheSpire/SlayTheSpire.exe"),
            b"",
        )
        .unwrap();

        let apps = steam_apps_from_roots(&[steam_root]).expect("steam scan");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].app_id, 646570);
        assert_eq!(apps[0].name, "Slay the Spire");
        assert_eq!(
            apps[0].install_dir,
            library.join("steamapps/common/SlayTheSpire")
        );
        assert_eq!(apps[0].exe_name.as_deref(), Some("SlayTheSpire.exe"));
    }

    #[test]
    fn steam_launch_catalog_is_manifest_only_and_matches_nested_executables() {
        let dir = TestDir::new("clipline-game-discovery", "launch-catalog");
        let steam_root = dir.path().join("Steam");
        std::fs::create_dir_all(steam_root.join("steamapps/common/Friendslop")).unwrap();
        std::fs::write(
            steam_root.join("steamapps/libraryfolders.vdf"),
            format!(
                r#""libraryfolders" {{ "0" {{ "path" "{}" }} }}"#,
                vdf_path(&steam_root)
            ),
        )
        .unwrap();
        std::fs::write(
            steam_root.join("steamapps/appmanifest_427520.acf"),
            r#""AppState" { "appid" "427520" "name" "Friendslop" "installdir" "Friendslop" }"#,
        )
        .unwrap();
        // No .exe files exist on disk: the detector catalog never infers them.

        let catalog = SteamLaunchCatalog::scan_from_roots(std::slice::from_ref(&steam_root));
        let exe = steam_root.join("steamapps/common/Friendslop/Friendslop/Binaries/Win64/FriendslopGame.exe");
        let app = catalog
            .find_by_exe_path(&exe.to_string_lossy())
            .expect("install-dir prefix should match a nested executable");
        assert_eq!(app.app_id, 427520);
        assert_eq!(app.name, "Friendslop");
        assert_eq!(
            app.install_dir,
            steam_root.join("steamapps/common/Friendslop")
        );
        assert!(catalog.is_steam_rooted(
            &steam_root
                .join("steamapps/common/OtherGame/OtherGame.exe")
                .to_string_lossy()
        ));
        assert!(!catalog.is_steam_rooted(
            &dir.path().join("Elsewhere/Game.exe").to_string_lossy()
        ));
    }

    #[test]
    fn steam_catalog_skips_malformed_manifest_and_continues() {
        let dir = TestDir::new("clipline-game-discovery", "steam-malformed");
        let steam_root = dir.path().join("Steam");
        let app_dir = steam_root.join("steamapps/common/Factorio");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            steam_root.join("steamapps/libraryfolders.vdf"),
            format!(
                r#""libraryfolders" {{ "0" {{ "path" "{}" }} }}"#,
                vdf_path(&steam_root)
            ),
        )
        .unwrap();
        std::fs::write(
            steam_root.join("steamapps/appmanifest_bad.acf"),
            r#""AppState" { "appid" "#,
        )
        .unwrap();
        std::fs::write(
            steam_root.join("steamapps/appmanifest_427520.acf"),
            r#""AppState" { "appid" "427520" "name" "Factorio" "installdir" "Factorio" }"#,
        )
        .unwrap();
        std::fs::write(app_dir.join("factorio.exe"), b"").unwrap();

        let apps = steam_apps_from_roots(&[steam_root]).expect("steam scan");
        assert_eq!(
            apps.iter().map(|app| app.name.as_str()).collect::<Vec<_>>(),
            vec!["Factorio"]
        );
    }

    #[test]
    fn steam_catalog_scans_root_when_libraryfolders_is_malformed() {
        let dir = TestDir::new("clipline-game-discovery", "steam-bad-libraryfolders");
        let steam_root = dir.path().join("Steam");
        let app_dir = steam_root.join("steamapps/common/Hades");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            steam_root.join("steamapps/libraryfolders.vdf"),
            r#""libraryfolders" { "0" { "path" "#,
        )
        .unwrap();
        std::fs::write(
            steam_root.join("steamapps/appmanifest_1145360.acf"),
            r#""AppState" { "appid" "1145360" "name" "Hades" "installdir" "Hades" }"#,
        )
        .unwrap();
        std::fs::write(app_dir.join("Hades.exe"), b"").unwrap();

        let apps = steam_apps_from_roots(&[steam_root]).expect("steam scan");
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Hades");
        assert_eq!(apps[0].exe_name.as_deref(), Some("Hades.exe"));
    }

    #[test]
    fn running_window_under_steam_install_upgrades_installed_candidate() {
        let steam = SteamApp {
            app_id: 646570,
            name: "Slay the Spire".into(),
            install_dir: PathBuf::from(r"C:\Steam\steamapps\common\SlayTheSpire"),
            exe_name: Some("SlayTheSpire.exe".into()),
            process_path: Some(PathBuf::from(
                r"C:\Steam\steamapps\common\SlayTheSpire\SlayTheSpire.exe",
            )),
        };
        let candidates = candidates_from_sources(
            vec![steam],
            vec![window(
                "Slay the Spire",
                "SlayTheSpire.exe",
                Some(r"C:\Steam\steamapps\common\SlayTheSpire\SlayTheSpire.exe"),
            )],
            &[],
            |_| None,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].source,
            DetectedGameSource::SteamAndRunningWindow
        );
        assert_eq!(candidates[0].window_title, "Slay the Spire");
        assert_eq!(candidates[0].confidence, 95);
    }

    #[test]
    fn ignores_running_non_steam_windows() {
        let candidates = candidates_from_sources(
            Vec::new(),
            vec![window(
                "FINAL FANTASY XIV",
                "ffxiv_dx11.exe",
                Some(r"D:\Games\FFXIV\ffxiv_dx11.exe"),
            )],
            &[],
            |_| None,
        );

        assert!(candidates.is_empty());
    }

    #[test]
    fn running_window_under_steam_install_adds_missing_installed_candidate_path() {
        let steam = SteamApp {
            app_id: 427520,
            name: "Factorio".into(),
            install_dir: PathBuf::from(r"D:\Steam\steamapps\common\Factorio"),
            exe_name: None,
            process_path: None,
        };
        let candidates = candidates_from_sources(
            vec![steam],
            vec![window(
                "Factorio",
                "factorio.exe",
                Some(r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe"),
            )],
            &[],
            |_| None,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].source,
            DetectedGameSource::SteamAndRunningWindow
        );
        assert_eq!(candidates[0].steam_app_id, Some(427520));
        assert_eq!(candidates[0].name, "Factorio");
        assert_eq!(candidates[0].exe_name, "factorio.exe");
        assert_eq!(
            candidates[0].process_path.as_deref(),
            Some(r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe")
        );
    }

    #[test]
    fn dedupes_against_existing_custom_games() {
        let existing = CustomGameSettings {
            id: "custom-factorio".into(),
            legacy_ids: Vec::new(),
            name: "factorio".into(),
            enabled: true,
            exe_name: "factorio.exe".into(),
            process_path: Some(r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe".into()),
            window_title: "Factorio".into(),
            recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
            icon: None,
        };
        let candidates = candidates_from_sources(
            vec![steam_app(
                427520,
                "Factorio",
                r"D:\Steam\steamapps\common\Factorio",
                "factorio.exe",
                r"D:\Steam\steamapps\common\Factorio\bin\x64\factorio.exe",
            )],
            Vec::new(),
            &[existing],
            |_| None,
        );

        assert!(candidates.is_empty());
    }

    #[test]
    fn keeps_candidate_when_existing_custom_game_has_same_exe_but_different_path() {
        let existing = CustomGameSettings {
            id: "custom-game-a".into(),
            legacy_ids: Vec::new(),
            name: "Game A".into(),
            enabled: true,
            exe_name: "game.exe".into(),
            process_path: Some(r"D:\Games\A\game.exe".into()),
            window_title: "Game A".into(),
            recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
            icon: None,
        };
        let candidates = candidates_from_sources(
            vec![steam_app(
                200,
                "Game B",
                r"E:\Games\B",
                "game.exe",
                r"E:\Games\B\game.exe",
            )],
            Vec::new(),
            &[existing],
            |_| None,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].process_path.as_deref(),
            Some(r"E:\Games\B\game.exe")
        );
    }

    #[test]
    fn keeps_candidate_when_existing_custom_game_has_same_name_but_different_path() {
        let existing = CustomGameSettings {
            id: "custom-hades-epic".into(),
            legacy_ids: Vec::new(),
            name: "Hades".into(),
            enabled: true,
            exe_name: "Hades.exe".into(),
            process_path: Some(r"D:\Epic\Hades\Hades.exe".into()),
            window_title: "Hades".into(),
            recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
            icon: None,
        };
        let candidates = candidates_from_sources(
            vec![steam_app(
                1145360,
                "Hades",
                r"E:\Steam\steamapps\common\Hades",
                "Hades.exe",
                r"E:\Steam\steamapps\common\Hades\Hades.exe",
            )],
            Vec::new(),
            &[existing],
            |_| None,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].process_path.as_deref(),
            Some(r"E:\Steam\steamapps\common\Hades\Hades.exe")
        );
    }

    #[test]
    fn keeps_discovered_candidates_with_same_exe_but_different_paths() {
        let candidates = candidates_from_sources(
            vec![
                steam_app(
                    100,
                    "Game A",
                    r"D:\Games\A",
                    "game.exe",
                    r"D:\Games\A\game.exe",
                ),
                steam_app(
                    200,
                    "Game B",
                    r"E:\Games\B",
                    "game.exe",
                    r"E:\Games\B\game.exe",
                ),
            ],
            Vec::new(),
            &[],
            |_| None,
        );

        let mut paths = candidates
            .iter()
            .filter_map(|candidate| candidate.process_path.as_deref())
            .collect::<Vec<_>>();
        paths.sort_unstable();
        assert_eq!(paths, vec![r"D:\Games\A\game.exe", r"E:\Games\B\game.exe"]);
    }

    #[test]
    fn still_dedupes_existing_custom_game_by_exe_when_existing_path_is_missing() {
        let existing = CustomGameSettings {
            id: "custom-game".into(),
            legacy_ids: Vec::new(),
            name: "Configured Game".into(),
            enabled: true,
            exe_name: "game.exe".into(),
            process_path: None,
            window_title: "Configured Game".into(),
            recording_mode: crate::settings::GameRecordingMode::ReplaysOnly,
            icon: None,
        };
        let candidates = candidates_from_sources(
            vec![steam_app(
                200,
                "Detected Game",
                r"E:\Games\B",
                "game.exe",
                r"E:\Games\B\game.exe",
            )],
            Vec::new(),
            &[existing],
            |_| None,
        );

        assert!(candidates.is_empty());
    }
}
