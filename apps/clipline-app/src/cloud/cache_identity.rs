pub(super) fn validate_cloud_cache_component<'a>(
    value: &'a str,
    label: &str,
) -> Result<&'a str, String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!("{label} contains unsupported characters"));
    }
    Ok(trimmed)
}
