use std::path::Path;

pub(super) fn inferred_clip_kind_for_path(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if std::path::Path::new(&name)
        .extension()
        .is_some_and(|ext| ext == "png")
    {
        return "screenshot";
    }
    if name.contains("_trim_") {
        "trim"
    } else if name.starts_with("session_") {
        "session"
    } else {
        "replay"
    }
}

pub(super) fn normalized_clip_title(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("clip name cannot be empty".into());
    }
    if trimmed.chars().any(|ch| ch.is_ascii_control()) {
        return Err("clip name contains a control character".into());
    }
    Ok(trimmed.to_string())
}

pub(super) fn normalized_clip_file_name(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("clip name cannot be empty".into());
    }
    if trimmed.contains(['/', '\\']) {
        return Err("clip name cannot contain folders".into());
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_ascii_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err("clip name contains a character Windows cannot use in filenames".into());
    }

    // Keep an explicit .png suffix (screenshots); anything else becomes .mp4.
    let lower = trimmed.to_ascii_lowercase();
    let suffix = if lower.ends_with(".mp4") || lower.ends_with(".png") {
        &lower[lower.len() - 4..]
    } else {
        ".mp4"
    };
    let stem = if lower.ends_with(".mp4") || lower.ends_with(".png") {
        &trimmed[..trimmed.len() - 4]
    } else {
        trimmed
    }
    .trim();
    if stem.is_empty() || stem == "." || stem == ".." {
        return Err("clip name cannot be empty".into());
    }
    if stem.ends_with(['.', ' ']) {
        return Err("clip name cannot end with a dot or space".into());
    }
    if is_reserved_windows_file_name(stem) {
        return Err("clip name is reserved by Windows".into());
    }
    Ok(format!("{stem}{suffix}"))
}

pub(crate) fn is_reserved_windows_file_name(stem: &str) -> bool {
    let base = stem
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit()
            && base.as_bytes()[3] != b'0')
}
