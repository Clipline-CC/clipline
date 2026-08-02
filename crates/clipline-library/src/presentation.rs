use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{MAX_CATALOG_STRING_BYTES, MAX_CLIP_DETAIL_MARKERS};

pub const MAX_PRESENTATION_ENTRIES: usize = 128;
pub const MAX_GALLERY_CARD_PLAYS: usize = 256;
pub const MAX_GALLERY_CARD_STATS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PresentationError {
    #[error("{field} contains {actual} bytes or entries; maximum is {maximum}")]
    TooLarge {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("{field} is invalid")]
    Invalid { field: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryMarker {
    pub kind: String,
}

impl GalleryMarker {
    #[must_use]
    pub fn new(kind: impl Into<String>) -> Self {
        Self { kind: kind.into() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerKindPresentation {
    pub key: String,
    pub category: String,
    pub glyph: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerCategoryPresentation {
    pub key: String,
    pub singular: String,
    pub plural: String,
    pub glyph: String,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerPresentation {
    pub kinds: Vec<MarkerKindPresentation>,
    pub categories: Vec<MarkerCategoryPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerStyle {
    pub glyph: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

pub fn marker_style(
    kind: &str,
    presentation: Option<&MarkerPresentation>,
) -> Result<MarkerStyle, PresentationError> {
    validate_marker_inputs(&[GalleryMarker::new(kind)], presentation)?;
    let category = marker_category(kind, presentation);
    let meta = marker_category_meta(&category, presentation);
    let glyph = find_kind(kind, presentation)
        .map(|configured| configured.glyph.trim())
        .filter(|glyph| !glyph.is_empty())
        .unwrap_or(meta.glyph)
        .to_owned();
    Ok(MarkerStyle {
        glyph,
        category,
        color: meta.color.map(str::to_owned),
    })
}

pub fn marker_digest(
    markers: &[GalleryMarker],
    presentation: Option<&MarkerPresentation>,
) -> Result<String, PresentationError> {
    validate_marker_inputs(markers, presentation)?;
    let mut counts = BTreeMap::<String, usize>::new();
    for marker in markers {
        let category = marker_category(&marker.kind, presentation);
        *counts.entry(category).or_default() += 1;
    }

    let mut order = Vec::new();
    if let Some(presentation) = presentation {
        order.extend(
            presentation
                .categories
                .iter()
                .map(|category| category.key.as_str()),
        );
    }
    for category in DEFAULT_CATEGORIES {
        if !order.contains(&category.key) {
            order.push(category.key);
        }
    }
    let parts: Vec<_> = order
        .into_iter()
        .filter_map(|category| {
            let count = counts.get(category).copied()?;
            let meta = marker_category_meta(category, presentation);
            let label = if count == 1 {
                meta.singular
            } else {
                meta.plural
            };
            Some(format!("{count} {label}"))
        })
        .collect();
    Ok(parts.join(" · "))
}

#[derive(Debug, Clone, Copy)]
struct DefaultMarkerCategory {
    key: &'static str,
    singular: &'static str,
    plural: &'static str,
    glyph: &'static str,
    color: &'static str,
}

const DEFAULT_CATEGORIES: &[DefaultMarkerCategory] = &[
    DefaultMarkerCategory {
        key: "kill",
        singular: "kill",
        plural: "kills",
        glyph: "✕",
        color: "#ff9b9e",
    },
    DefaultMarkerCategory {
        key: "assist",
        singular: "assist",
        plural: "assists",
        glyph: "+",
        color: "#7bdff2",
    },
    DefaultMarkerCategory {
        key: "death",
        singular: "death",
        plural: "deaths",
        glyph: "✕",
        color: "#b9a7c4",
    },
    DefaultMarkerCategory {
        key: "spree",
        singular: "spree",
        plural: "sprees",
        glyph: "★",
        color: "#ffc77d",
    },
    DefaultMarkerCategory {
        key: "objective",
        singular: "objective",
        plural: "objectives",
        glyph: "◆",
        color: "#cdb2ff",
    },
    DefaultMarkerCategory {
        key: "structure",
        singular: "structure",
        plural: "structures",
        glyph: "▣",
        color: "#f0cd7a",
    },
    DefaultMarkerCategory {
        key: "info",
        singular: "event",
        plural: "events",
        glyph: "•",
        color: "#c0b4aa",
    },
];

struct MarkerCategoryMeta<'a> {
    singular: &'a str,
    plural: &'a str,
    glyph: &'a str,
    color: Option<&'a str>,
}

fn marker_category(kind: &str, presentation: Option<&MarkerPresentation>) -> String {
    find_kind(kind, presentation)
        .map(|configured| configured.category.trim())
        .filter(|category| !category.is_empty())
        .unwrap_or_else(|| default_marker_category(kind))
        .to_owned()
}

fn default_marker_category(kind: &str) -> &'static str {
    match kind {
        "ChampionKill" => "kill",
        "ChampionAssist" => "assist",
        "ChampionDeath" => "death",
        "FirstBlood" | "Multikill" | "Ace" => "spree",
        "DragonKill" | "HeraldKill" | "BaronKill" => "objective",
        "TurretKilled" | "InhibKilled" | "FirstBrick" => "structure",
        _ => "info",
    }
}

fn find_kind<'a>(
    kind: &str,
    presentation: Option<&'a MarkerPresentation>,
) -> Option<&'a MarkerKindPresentation> {
    presentation?.kinds.iter().find(|entry| entry.key == kind)
}

fn marker_category_meta<'a>(
    category: &str,
    presentation: Option<&'a MarkerPresentation>,
) -> MarkerCategoryMeta<'a> {
    let fallback = DEFAULT_CATEGORIES
        .iter()
        .find(|entry| entry.key == category)
        .unwrap_or_else(|| DEFAULT_CATEGORIES.last().expect("info category"));
    let configured = presentation.and_then(|presentation| {
        presentation
            .categories
            .iter()
            .find(|entry| entry.key == category)
    });
    MarkerCategoryMeta {
        singular: configured
            .map(|entry| entry.singular.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback.singular),
        plural: configured
            .map(|entry| entry.plural.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback.plural),
        glyph: configured
            .map(|entry| entry.glyph.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback.glyph),
        color: configured
            .and_then(|entry| entry.color.as_deref())
            .map(str::trim)
            .filter(|color| !color.is_empty())
            .or(Some(fallback.color)),
    }
}

fn validate_marker_inputs(
    markers: &[GalleryMarker],
    presentation: Option<&MarkerPresentation>,
) -> Result<(), PresentationError> {
    check_count("markers", markers.len(), MAX_CLIP_DETAIL_MARKERS)?;
    for marker in markers {
        check_string("marker.kind", &marker.kind)?;
    }
    let Some(presentation) = presentation else {
        return Ok(());
    };
    check_count(
        "presentation.kinds",
        presentation.kinds.len(),
        MAX_PRESENTATION_ENTRIES,
    )?;
    check_count(
        "presentation.categories",
        presentation.categories.len(),
        MAX_PRESENTATION_ENTRIES,
    )?;
    let mut kind_keys = BTreeSet::new();
    for entry in &presentation.kinds {
        validate_key("presentation.kind.key", &entry.key)?;
        validate_key("presentation.kind.category", &entry.category)?;
        check_string("presentation.kind.glyph", &entry.glyph)?;
        if !kind_keys.insert(entry.key.as_str()) {
            return Err(PresentationError::Invalid {
                field: "presentation.kind.duplicate",
            });
        }
    }
    let mut category_keys = BTreeSet::new();
    for entry in &presentation.categories {
        validate_key("presentation.category.key", &entry.key)?;
        check_string("presentation.category.singular", &entry.singular)?;
        check_string("presentation.category.plural", &entry.plural)?;
        check_string("presentation.category.glyph", &entry.glyph)?;
        check_optional_string("presentation.category.color", entry.color.as_deref())?;
        if !category_keys.insert(entry.key.as_str()) {
            return Err(PresentationError::Invalid {
                field: "presentation.category.duplicate",
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerCardSummary {
    pub champion_name: String,
    pub kills: u32,
    pub deaths: u32,
    pub assists: u32,
    pub creep_score: Option<u32>,
    pub game_time_s: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GalleryPlay {
    pub passed: bool,
    pub rank: String,
    pub pp: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GalleryCardInput {
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
    pub fallback_title: String,
    pub player_summary: Option<PlayerCardSummary>,
    pub plays: Vec<GalleryPlay>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GallerySummaryMode {
    #[default]
    None,
    PlayerSummaryKda,
    OsuSetPlays,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryCardTitlePolicy {
    #[default]
    Clip,
    Summary,
    SummaryForFullSession,
    OsuSessionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GalleryCardStat {
    Kda,
    CsPerMin { label: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardTitleFormat {
    pub separator: String,
    pub stats: Vec<GalleryCardStat>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetAlias {
    pub alias: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GalleryCardIconConfig {
    Asset {
        url: String,
        label: String,
    },
    Portrait {
        label: String,
        aliases: Vec<AssetAlias>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardConfig {
    pub title: GalleryCardTitlePolicy,
    pub title_format: Option<GalleryCardTitleFormat>,
    pub icon: Option<GalleryCardIconConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryPresentation {
    pub summary: GallerySummaryMode,
    pub legacy_full_session_summary: bool,
    pub card: GalleryCardConfig,
    pub data_dragon_version: Option<String>,
    pub markers: MarkerPresentation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardIcon {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryCardPreview {
    pub title: String,
    #[serde(rename = "titleSource")]
    pub title_source: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<GalleryCardIcon>,
}

pub fn gallery_card_preview(
    input: &GalleryCardInput,
    presentation: &GalleryPresentation,
) -> Result<GalleryCardPreview, PresentationError> {
    validate_card_input(input, presentation)?;
    let summary_label = match presentation.summary {
        GallerySummaryMode::PlayerSummaryKda => input
            .player_summary
            .as_ref()
            .map(player_summary_label)
            .unwrap_or_default(),
        GallerySummaryMode::OsuSetPlays => play_summary(&input.plays),
        GallerySummaryMode::None => String::new(),
    };
    let detail_summary = match presentation.summary {
        GallerySummaryMode::OsuSetPlays => play_result_summary(&input.plays),
        _ => summary_label.clone(),
    };
    let formatted_summary = input
        .player_summary
        .as_ref()
        .zip(presentation.card.title_format.as_ref())
        .map(|(summary, format)| player_summary_stats_label(summary, format))
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| summary_label.clone());
    let fallback = input.fallback_title.trim();
    let custom_title = input.title.as_deref().unwrap_or_default().trim();
    let clip_name = input.name.trim();
    let clip_display_title = if custom_title.is_empty() {
        strip_media_extension(clip_name)
    } else {
        custom_title.to_owned()
    };
    let policy = if presentation.legacy_full_session_summary
        && presentation.card.title == GalleryCardTitlePolicy::Clip
    {
        GalleryCardTitlePolicy::SummaryForFullSession
    } else {
        presentation.card.title
    };
    let uses_clip_title = policy == GalleryCardTitlePolicy::Clip
        || (policy == GalleryCardTitlePolicy::OsuSessionSummary && input.kind != "session");
    let uses_summary_title = !formatted_summary.is_empty()
        && matches!(
            (policy, input.kind.as_str()),
            (GalleryCardTitlePolicy::Summary, _)
                | (GalleryCardTitlePolicy::SummaryForFullSession, "session")
                | (GalleryCardTitlePolicy::OsuSessionSummary, "session")
        );
    let clip_title = if uses_clip_title && !clip_display_title.is_empty() {
        clip_display_title
    } else {
        fallback.to_owned()
    };
    Ok(GalleryCardPreview {
        title: if uses_summary_title {
            formatted_summary
        } else {
            clip_title
        },
        title_source: if uses_summary_title {
            "summary"
        } else {
            "clip"
        }
        .into(),
        summary: detail_summary,
        icon: gallery_card_icon(input.player_summary.as_ref(), presentation)?,
    })
}

fn gallery_card_icon(
    summary: Option<&PlayerCardSummary>,
    presentation: &GalleryPresentation,
) -> Result<Option<GalleryCardIcon>, PresentationError> {
    let Some(config) = presentation.card.icon.as_ref() else {
        return Ok(None);
    };
    match config {
        GalleryCardIconConfig::Asset { url, label } => {
            let url = url.trim();
            if url.is_empty() {
                return Ok(None);
            }
            Ok(Some(GalleryCardIcon {
                kind: "asset".into(),
                url: url.into(),
                label: label.trim().into(),
            }))
        }
        GalleryCardIconConfig::Portrait { label, aliases } => {
            let Some(summary) = summary else {
                return Ok(None);
            };
            let champion = summary.champion_name.trim();
            let Some(version) = presentation
                .data_dragon_version
                .as_deref()
                .filter(|version| valid_data_dragon_version(version))
            else {
                return Ok(None);
            };
            if champion.is_empty() {
                return Ok(None);
            }
            let key = data_dragon_champion_key(champion, aliases);
            if key.is_empty() {
                return Ok(None);
            }
            let url =
                format!("https://ddragon.leagueoflegends.com/cdn/{version}/img/champion/{key}.png");
            check_string("gallery.icon.output_url", &url)?;
            Ok(Some(GalleryCardIcon {
                kind: "portrait".into(),
                url,
                label: if champion.is_empty() {
                    label.trim()
                } else {
                    champion
                }
                .into(),
            }))
        }
    }
}

fn player_summary_label(summary: &PlayerCardSummary) -> String {
    let champion = summary.champion_name.trim();
    if champion.is_empty() {
        String::new()
    } else {
        format!("{champion} | {}", player_summary_kda(summary))
    }
}

fn player_summary_kda(summary: &PlayerCardSummary) -> String {
    format!("{}/{}/{}", summary.kills, summary.deaths, summary.assists)
}

fn player_summary_stats_label(
    summary: &PlayerCardSummary,
    format: &GalleryCardTitleFormat,
) -> String {
    let mut parts = Vec::new();
    for stat in &format.stats {
        match stat {
            GalleryCardStat::Kda => parts.push(player_summary_kda(summary)),
            GalleryCardStat::CsPerMin { label } => {
                if let (Some(creep_score), Some(game_time_s)) =
                    (summary.creep_score, summary.game_time_s)
                {
                    if game_time_s > 0 {
                        let suffix = if label.trim().is_empty() {
                            "CS/min"
                        } else {
                            label.trim()
                        };
                        parts.push(format!(
                            "{:.1} {suffix}",
                            f64::from(creep_score) / (f64::from(game_time_s) / 60.0)
                        ));
                    }
                }
            }
        }
    }
    let separator = if format.separator.is_empty() {
        " | "
    } else {
        &format.separator
    };
    parts.join(separator)
}

fn play_summary(plays: &[GalleryPlay]) -> String {
    match plays.len() {
        0 => "no submitted plays".into(),
        1 => "1 submitted play".into(),
        count => format!("{count} submitted plays"),
    }
}

fn play_result_summary(plays: &[GalleryPlay]) -> String {
    let passed = plays.iter().filter(|play| play.passed).count();
    let incomplete = plays
        .iter()
        .filter(|play| {
            !play.passed
                && !play.pp.is_some_and(|pp| pp.is_finite() && pp > 0.0)
                && !play.rank.trim().eq_ignore_ascii_case("F")
        })
        .count();
    let failed = plays.len().saturating_sub(passed + incomplete);
    let mut parts = Vec::new();
    if passed != 0 {
        parts.push(format!(
            "{passed} {}",
            if passed == 1 { "pass" } else { "passes" }
        ));
    }
    if incomplete != 0 {
        parts.push(format!("{incomplete} incomplete"));
    }
    if failed != 0 {
        parts.push(format!(
            "{failed} {}",
            if failed == 1 { "fail" } else { "fails" }
        ));
    }
    parts.join(" · ")
}

fn strip_media_extension(name: &str) -> String {
    for extension in [".mp4", ".mov", ".mkv", ".webm"] {
        if name
            .get(name.len().saturating_sub(extension.len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(extension))
        {
            let stem = name[..name.len() - extension.len()].trim();
            return if stem.is_empty() { name.trim() } else { stem }.to_owned();
        }
    }
    name.trim().to_owned()
}

fn data_dragon_champion_key(champion: &str, aliases: &[AssetAlias]) -> String {
    let lookup = alphanumeric_lookup(champion);
    if let Some(alias) = aliases
        .iter()
        .find(|alias| alphanumeric_lookup(&alias.alias) == lookup)
    {
        if alias
            .key
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
        {
            return alias.key.clone();
        }
    }
    champion
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                let mut result = first.to_ascii_uppercase().to_string();
                result.push_str(characters.as_str());
                result
            })
        })
        .collect()
}

fn alphanumeric_lookup(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn valid_data_dragon_version(version: &str) -> bool {
    let mut segments = version.split('.');
    (0..3).all(|_| {
        segments
            .next()
            .is_some_and(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    }) && segments.next().is_none()
}

fn validate_card_input(
    input: &GalleryCardInput,
    presentation: &GalleryPresentation,
) -> Result<(), PresentationError> {
    check_string("gallery.name", &input.name)?;
    check_optional_string("gallery.title", input.title.as_deref())?;
    check_string("gallery.kind", &input.kind)?;
    check_string("gallery.fallback_title", &input.fallback_title)?;
    check_count("gallery.plays", input.plays.len(), MAX_GALLERY_CARD_PLAYS)?;
    if let Some(summary) = &input.player_summary {
        check_string("gallery.summary.champion_name", &summary.champion_name)?;
    }
    for play in &input.plays {
        check_string("gallery.play.rank", &play.rank)?;
        if play.pp.is_some_and(|pp| !pp.is_finite()) {
            return Err(PresentationError::Invalid {
                field: "gallery.play.pp",
            });
        }
    }
    if let Some(format) = &presentation.card.title_format {
        check_string("gallery.card.separator", &format.separator)?;
        check_count(
            "gallery.card.stats",
            format.stats.len(),
            MAX_GALLERY_CARD_STATS,
        )?;
        for stat in &format.stats {
            if let GalleryCardStat::CsPerMin { label } = stat {
                check_string("gallery.card.stat.label", label)?;
            }
        }
    }
    if let Some(icon) = &presentation.card.icon {
        match icon {
            GalleryCardIconConfig::Asset { url, label } => {
                check_string("gallery.card.icon.url", url)?;
                check_string("gallery.card.icon.label", label)?;
            }
            GalleryCardIconConfig::Portrait { label, aliases } => {
                check_string("gallery.card.icon.label", label)?;
                check_count(
                    "gallery.card.icon.aliases",
                    aliases.len(),
                    MAX_PRESENTATION_ENTRIES,
                )?;
                for alias in aliases {
                    check_string("gallery.card.icon.alias", &alias.alias)?;
                    check_string("gallery.card.icon.alias_key", &alias.key)?;
                }
            }
        }
    }
    check_optional_string(
        "gallery.data_dragon_version",
        presentation.data_dragon_version.as_deref(),
    )?;
    validate_marker_inputs(&[], Some(&presentation.markers))
}

fn validate_key(field: &'static str, value: &str) -> Result<(), PresentationError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_alphabetic()
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        || matches!(value, "constructor" | "prototype")
    {
        Err(PresentationError::Invalid { field })
    } else {
        Ok(())
    }
}

fn check_optional_string(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), PresentationError> {
    value.map_or(Ok(()), |value| check_string(field, value))
}

fn check_string(field: &'static str, value: &str) -> Result<(), PresentationError> {
    check_count(field, value.len(), MAX_CATALOG_STRING_BYTES)
}

fn check_count(
    field: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), PresentationError> {
    if actual > maximum {
        Err(PresentationError::TooLarge {
            field,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}
