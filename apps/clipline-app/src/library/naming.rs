use std::path::Path;

pub(crate) fn inferred_clip_kind_for_path(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.contains("_trim_") {
        "trim"
    } else if name.starts_with("session_") {
        "session"
    } else {
        "replay"
    }
}

pub(crate) fn normalized_clip_title(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("clip name cannot be empty".into());
    }
    if trimmed.chars().any(|ch| ch.is_ascii_control()) {
        return Err("clip name contains a control character".into());
    }
    Ok(trimmed.to_string())
}

pub(crate) fn normalized_clip_file_name(input: &str) -> Result<String, String> {
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

    let suffix = trimmed
        .get(trimmed.len().saturating_sub(4)..)
        .filter(|suffix| suffix.eq_ignore_ascii_case(".mp4"));
    let stem = if suffix.is_some() {
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
    Ok(format!("{stem}.mp4"))
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


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalized_clip_file_name_adds_mp4_and_preserves_valid_text() {
        assert_eq!(
            normalized_clip_file_name("Ranked win").unwrap(),
            "Ranked win.mp4"
        );
        assert_eq!(
            normalized_clip_file_name("Ranked win.Mp4").unwrap(),
            "Ranked win.mp4"
        );
        assert_eq!(
            normalized_clip_file_name("solo.queue.vod").unwrap(),
            "solo.queue.vod.mp4"
        );
    }
    #[test]
    fn normalized_clip_file_name_rejects_paths_reserved_names_and_invalid_chars() {
        for name in [
            "",
            "..",
            "folder/clip",
            r"folder\clip",
            "bad:name",
            "clip?",
            "clip.",
            "CON",
            "LPT1.mp4",
        ] {
            assert!(
                normalized_clip_file_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }
    #[test]
    fn inferred_clip_kind_only_matches_generated_filename_patterns() {
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("trimming-practice.mp4")),
            "replay"
        );
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("obsession.mp4")),
            "replay"
        );
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("clip_1_trim_001000_002000.mp4")),
            "trim"
        );
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("session_1781377615.mp4")),
            "session"
        );
    }
}
