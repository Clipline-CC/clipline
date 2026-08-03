use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ClipPathIdentity, LocalClipItem, MAX_CATALOG_PAGE_ROWS, MAX_CATALOG_STRING_BYTES,
    MAX_DECODED_PAGE_IMAGES, MAX_POSTER_RESULT_ENTRIES,
};

pub const DEFAULT_GALLERY_PAGE_SIZE: usize = MAX_CATALOG_PAGE_ROWS;
pub const MISSING_POSTER_RUNTIME_ERROR: &str = "ffmpeg is not available for poster extraction";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GalleryStateError {
    #[error("gallery identity is {actual} bytes; maximum is {maximum}")]
    IdentityTooLong { actual: usize, maximum: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GalleryPageState {
    page: usize,
    identity: String,
    page_size: usize,
}

impl<'de> Deserialize<'de> for GalleryPageState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawGalleryPageState {
            page: usize,
            identity: String,
            page_size: usize,
        }

        let raw = RawGalleryPageState::deserialize(deserializer)?;
        if raw.identity.len() > MAX_CATALOG_STRING_BYTES {
            return Err(serde::de::Error::custom(
                "gallery identity exceeds its byte limit",
            ));
        }
        if raw.page_size == 0 || raw.page_size > MAX_CATALOG_PAGE_ROWS {
            return Err(serde::de::Error::custom(
                "gallery page size is out of bounds",
            ));
        }
        Ok(Self {
            page: raw.page,
            identity: raw.identity,
            page_size: raw.page_size,
        })
    }
}

impl Default for GalleryPageState {
    fn default() -> Self {
        Self::new(DEFAULT_GALLERY_PAGE_SIZE)
    }
}

impl GalleryPageState {
    #[must_use]
    pub fn new(requested_page_size: usize) -> Self {
        Self {
            page: 0,
            identity: String::new(),
            page_size: validated_page_size(requested_page_size),
        }
    }

    #[must_use]
    pub const fn page(&self) -> usize {
        self.page
    }

    #[must_use]
    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn update(
        &mut self,
        identity: impl Into<String>,
        total: usize,
        requested_page_size: Option<usize>,
    ) -> Result<(), GalleryStateError> {
        let identity = identity.into();
        if identity.len() > MAX_CATALOG_STRING_BYTES {
            return Err(GalleryStateError::IdentityTooLong {
                actual: identity.len(),
                maximum: MAX_CATALOG_STRING_BYTES,
            });
        }
        let page_size = requested_page_size
            .map(validated_page_size)
            .unwrap_or(self.page_size);
        let changed = self.identity != identity || self.page_size != page_size;
        self.page = if changed {
            0
        } else {
            clamp_page(self.page, total, page_size)
        };
        self.identity = identity;
        self.page_size = page_size;
        Ok(())
    }

    pub fn set_page(&mut self, requested_page: usize, total: usize) {
        self.page = clamp_page(requested_page, total, self.page_size);
    }

    #[must_use]
    pub fn info(&self, total: usize) -> GalleryPageInfo {
        page_info(self.page, total, self.page_size)
    }

    #[must_use]
    pub fn window_items<T: Clone>(&self, items: &[T]) -> GalleryWindow<T> {
        let info = self.info(items.len());
        GalleryWindow {
            page: info.page,
            page_count: info.page_count,
            page_size: info.page_size,
            total: info.total,
            start: info.start,
            end: info.end,
            has_previous: info.has_previous,
            has_next: info.has_next,
            items: items[info.start..info.end].to_vec(),
        }
    }

    #[must_use]
    pub fn window_groups<T: Clone>(&self, groups: &[GalleryGroup<T>]) -> GroupedGalleryWindow<T> {
        let total = groups.iter().fold(0_usize, |count, group| {
            count.saturating_add(group.items.len())
        });
        let info = self.info(total);
        let mut visible_groups = Vec::new();
        let mut offset = 0_usize;

        for group in groups {
            let group_start = offset;
            let group_end = group_start.saturating_add(group.items.len());
            offset = group_end;
            let visible_start = info.start.max(group_start);
            let visible_end = info.end.min(group_end);
            if visible_start >= visible_end {
                continue;
            }
            let start_in_group = visible_start - group_start;
            let end_in_group = visible_end - group_start;
            visible_groups.push(VisibleGalleryGroup {
                label: group.label.clone(),
                total_count: group.items.len(),
                start_in_group,
                items: group.items[start_in_group..end_in_group].to_vec(),
            });
        }

        GroupedGalleryWindow {
            page: info.page,
            page_count: info.page_count,
            page_size: info.page_size,
            total: info.total,
            start: info.start,
            end: info.end,
            has_previous: info.has_previous,
            has_next: info.has_next,
            groups: visible_groups,
        }
    }
}

#[must_use]
pub const fn validated_page_size(requested: usize) -> usize {
    if requested == 0 || requested > MAX_CATALOG_PAGE_ROWS {
        DEFAULT_GALLERY_PAGE_SIZE
    } else {
        requested
    }
}

#[must_use]
pub fn page_count(total: usize, page_size: usize) -> usize {
    let size = validated_page_size(page_size);
    total / size + usize::from(!total.is_multiple_of(size))
}

#[must_use]
pub fn clamp_page(page: usize, total: usize, page_size: usize) -> usize {
    let pages = page_count(total, page_size);
    if pages == 0 {
        0
    } else if page >= pages {
        pages - 1
    } else {
        page
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GalleryPageInfo {
    pub page: usize,
    pub page_count: usize,
    pub page_size: usize,
    pub total: usize,
    pub start: usize,
    pub end: usize,
    pub has_previous: bool,
    pub has_next: bool,
}

#[must_use]
pub fn page_info(page: usize, total: usize, page_size: usize) -> GalleryPageInfo {
    let size = validated_page_size(page_size);
    let pages = page_count(total, size);
    let page = clamp_page(page, total, size);
    let start = if pages == 0 {
        0
    } else {
        page.saturating_mul(size)
    };
    let end = total.min(start.saturating_add(size));
    GalleryPageInfo {
        page,
        page_count: pages,
        page_size: size,
        total,
        start,
        end,
        has_previous: page > 0,
        has_next: page.saturating_add(1) < pages,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryWindow<T> {
    pub page: usize,
    pub page_count: usize,
    pub page_size: usize,
    pub total: usize,
    pub start: usize,
    pub end: usize,
    pub has_previous: bool,
    pub has_next: bool,
    pub items: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryGroup<T> {
    pub label: Option<String>,
    pub items: Vec<T>,
}

impl<T> GalleryGroup<T> {
    #[must_use]
    pub const fn new(label: Option<String>, items: Vec<T>) -> Self {
        Self { label, items }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleGalleryGroup<T> {
    pub label: Option<String>,
    pub total_count: usize,
    pub start_in_group: usize,
    pub items: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedGalleryWindow<T> {
    pub page: usize,
    pub page_count: usize,
    pub page_size: usize,
    pub total: usize,
    pub start: usize,
    pub end: usize,
    pub has_previous: bool,
    pub has_next: bool,
    pub groups: Vec<VisibleGalleryGroup<T>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DecodedImageWindow {
    start: usize,
    end: usize,
}

impl<'de> Deserialize<'de> for DecodedImageWindow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawDecodedImageWindow {
            start: usize,
            end: usize,
        }

        let raw = RawDecodedImageWindow::deserialize(deserializer)?;
        if raw.start > raw.end || raw.end - raw.start > MAX_DECODED_PAGE_IMAGES {
            return Err(serde::de::Error::custom(
                "decoded image window is invalid or exceeds its row limit",
            ));
        }
        Ok(Self {
            start: raw.start,
            end: raw.end,
        })
    }
}

impl DecodedImageWindow {
    #[must_use]
    pub fn around(
        total_rows: usize,
        visible_start: usize,
        visible_count: usize,
        requested_overscan: usize,
    ) -> Self {
        let visible_start = visible_start.min(total_rows);
        let visible_end = total_rows.min(visible_start.saturating_add(visible_count));
        let mut start = visible_start.saturating_sub(requested_overscan);
        let mut end = total_rows.min(visible_end.saturating_add(requested_overscan));
        if end.saturating_sub(start) > MAX_DECODED_PAGE_IMAGES {
            if visible_end.saturating_sub(visible_start) >= MAX_DECODED_PAGE_IMAGES {
                start = visible_start;
                end = total_rows.min(start.saturating_add(MAX_DECODED_PAGE_IMAGES));
            } else {
                let mut excess = end - start - MAX_DECODED_PAGE_IMAGES;
                let trim_before = excess.min(visible_start - start);
                start += trim_before;
                excess -= trim_before;
                end -= excess;
            }
        }
        Self { start, end }
    }

    #[must_use]
    pub const fn start(self) -> usize {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> usize {
        self.end
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug, Clone)]
pub struct DeterministicLru<K, V> {
    limit: usize,
    entries: VecDeque<(K, V)>,
}

impl<K, V> DeterministicLru<K, V> {
    #[must_use]
    pub fn new(requested_limit: usize) -> Self {
        Self {
            limit: requested_limit.clamp(1, MAX_POSTER_RESULT_ENTRIES),
            entries: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

impl<K: PartialEq, V> DeterministicLru<K, V> {
    #[must_use]
    pub fn peek<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate.borrow() == key)
            .map(|(_, value)| value)
    }

    #[must_use]
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        let position = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate.borrow() == key)?;
        let entry = self.entries.remove(position)?;
        self.entries.push_back(entry);
        self.entries.back().map(|(_, value)| value)
    }

    #[must_use]
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        self.entries
            .iter()
            .any(|(candidate, _)| candidate.borrow() == key)
    }

    pub fn insert(&mut self, key: K, value: V) -> Vec<K> {
        if let Some(position) = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == &key)
        {
            self.entries.remove(position);
        }
        self.entries.push_back((key, value));
        let mut evicted = Vec::new();
        while self.entries.len() > self.limit {
            if let Some((key, _)) = self.entries.pop_front() {
                evicted.push(key);
            }
        }
        evicted
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: PartialEq + ?Sized,
    {
        let position = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate.borrow() == key)?;
        self.entries.remove(position).map(|(_, value)| value)
    }
}

#[must_use]
pub fn poster_runtime_unavailable(error: &str) -> bool {
    error
        .trim()
        .eq_ignore_ascii_case(MISSING_POSTER_RUNTIME_ERROR)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalClipFilter {
    #[default]
    All,
    Replay,
    Session,
    Trim,
    Marked,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalClipSort {
    #[default]
    Newest,
    Oldest,
    Largest,
    Marks,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalClipGrouping {
    #[default]
    Smart,
    Day,
    Game,
    Session,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDay {
    pub key: String,
    pub label: String,
}

pub trait LocalDayResolver {
    fn today_start_unix(&self) -> u64;
    fn resolve_day(&self, timestamp: u64) -> LocalDay;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalGalleryOptions {
    pub filter: LocalClipFilter,
    pub sort: LocalClipSort,
    pub grouping: LocalClipGrouping,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalGalleryResult<'a> {
    pub items: Vec<&'a LocalClipItem>,
    pub groups: Vec<GalleryGroup<&'a LocalClipItem>>,
    pub max_modified_unix: u64,
}

#[derive(Debug)]
struct SortableLocal<'a> {
    item: &'a LocalClipItem,
    identity: Option<ClipPathIdentity>,
}

#[must_use]
pub fn build_local_gallery<'a>(
    clips: &'a [LocalClipItem],
    options: &LocalGalleryOptions,
    days: &dyn LocalDayResolver,
) -> LocalGalleryResult<'a> {
    let query = options.query.trim().to_lowercase();
    let mut max_modified_unix = 0_u64;
    let mut sortable: Vec<_> = clips
        .iter()
        .filter(|clip| filter_matches(clip, options.filter) && search_matches(clip, &query))
        .map(|item| {
            max_modified_unix = max_modified_unix.max(item.modified_unix);
            SortableLocal {
                item,
                identity: item.path_identity(),
            }
        })
        .collect();
    sortable.sort_by(|left, right| compare_local(left, right, options.sort));
    let items: Vec<_> = sortable.into_iter().map(|entry| entry.item).collect();
    let groups = group_local(&items, options.grouping, days);
    LocalGalleryResult {
        items,
        groups,
        max_modified_unix,
    }
}

#[must_use]
pub fn local_clip_kind(clip: &LocalClipItem) -> LocalClipFilter {
    match clip.kind.trim() {
        "replay" => LocalClipFilter::Replay,
        "session" => LocalClipFilter::Session,
        "trim" => LocalClipFilter::Trim,
        _ if clip.name.contains("_trim_") => LocalClipFilter::Trim,
        _ if clip.name.starts_with("session_") => LocalClipFilter::Session,
        _ => LocalClipFilter::Replay,
    }
}

#[must_use]
pub fn local_clip_display_title(clip: &LocalClipItem) -> String {
    let custom = clip.title.as_deref().unwrap_or_default().trim();
    if !custom.is_empty() {
        return custom.to_owned();
    }
    let name = clip.name.trim();
    for extension in [".mp4", ".mov", ".mkv", ".webm"] {
        if name
            .get(name.len().saturating_sub(extension.len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(extension))
        {
            return name[..name.len() - extension.len()].trim().to_owned();
        }
    }
    name.to_owned()
}

fn filter_matches(clip: &LocalClipItem, filter: LocalClipFilter) -> bool {
    match filter {
        LocalClipFilter::All => true,
        LocalClipFilter::Marked => clip.marker_count != 0,
        expected => local_clip_kind(clip) == expected,
    }
}

fn search_matches(clip: &LocalClipItem, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let game = clip.game.as_ref().map_or("", |game| game.name.as_str());
    format!(
        "{} {} {} {}",
        local_clip_display_title(clip),
        clip.name,
        clip.session.as_deref().unwrap_or_default(),
        game,
    )
    .to_lowercase()
    .contains(query)
}

fn compare_local(
    left: &SortableLocal<'_>,
    right: &SortableLocal<'_>,
    sort: LocalClipSort,
) -> Ordering {
    let primary = match sort {
        LocalClipSort::Newest => right.item.modified_unix.cmp(&left.item.modified_unix),
        LocalClipSort::Oldest => left.item.modified_unix.cmp(&right.item.modified_unix),
        LocalClipSort::Largest => right.item.size_mb.total_cmp(&left.item.size_mb),
        LocalClipSort::Marks => right.item.marker_count.cmp(&left.item.marker_count),
    };
    let recency_tie = match sort {
        LocalClipSort::Oldest => Ordering::Equal,
        _ => right.item.modified_unix.cmp(&left.item.modified_unix),
    };
    primary
        .then(recency_tie)
        .then_with(|| compare_path_identity(left, right))
        .then_with(|| left.item.path.cmp(&right.item.path))
        .then_with(|| left.item.name.cmp(&right.item.name))
}

fn compare_path_identity(left: &SortableLocal<'_>, right: &SortableLocal<'_>) -> Ordering {
    match (&left.identity, &right.identity) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn group_local<'a>(
    items: &[&'a LocalClipItem],
    grouping: LocalClipGrouping,
    days: &dyn LocalDayResolver,
) -> Vec<GalleryGroup<&'a LocalClipItem>> {
    match grouping {
        LocalClipGrouping::None => vec![GalleryGroup::new(None, items.to_vec())],
        LocalClipGrouping::Smart => smart_groups(items, days.today_start_unix()),
        LocalClipGrouping::Day => bucket_groups(
            items,
            |item| {
                let day = days.resolve_day(item.modified_unix);
                (day.key, day.label)
            },
            true,
        ),
        LocalClipGrouping::Game => bucket_groups(
            items,
            |item| {
                let label = item
                    .game
                    .as_ref()
                    .map(|game| game.name.trim())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("No game detected")
                    .to_owned();
                (label.clone(), label)
            },
            true,
        ),
        LocalClipGrouping::Session => bucket_groups(
            items,
            |item| {
                let label = item
                    .session
                    .as_deref()
                    .map(str::trim)
                    .filter(|session| !session.is_empty())
                    .unwrap_or("Earlier")
                    .to_owned();
                (label.clone(), label)
            },
            true,
        ),
    }
}

#[derive(Debug)]
struct GroupBucket<'a> {
    label: String,
    max_modified_unix: u64,
    items: Vec<&'a LocalClipItem>,
}

fn bucket_groups<'a>(
    items: &[&'a LocalClipItem],
    mut key_and_label: impl FnMut(&LocalClipItem) -> (String, String),
    sort_items_newest: bool,
) -> Vec<GalleryGroup<&'a LocalClipItem>> {
    let mut positions = BTreeMap::<String, usize>::new();
    let mut buckets = Vec::<GroupBucket<'a>>::new();
    for &item in items {
        let (key, label) = key_and_label(item);
        let position = if let Some(position) = positions.get(&key).copied() {
            position
        } else {
            let position = buckets.len();
            positions.insert(key, position);
            buckets.push(GroupBucket {
                label,
                max_modified_unix: 0,
                items: Vec::new(),
            });
            position
        };
        let bucket = &mut buckets[position];
        bucket.max_modified_unix = bucket.max_modified_unix.max(item.modified_unix);
        bucket.items.push(item);
    }
    if sort_items_newest {
        for bucket in &mut buckets {
            bucket.items.sort_by(|left, right| {
                right
                    .modified_unix
                    .cmp(&left.modified_unix)
                    .then_with(|| stable_item_path(left).cmp(&stable_item_path(right)))
                    .then_with(|| left.path.cmp(&right.path))
            });
        }
    }
    buckets.sort_by(|left, right| {
        right
            .max_modified_unix
            .cmp(&left.max_modified_unix)
            .then_with(|| left.label.cmp(&right.label))
    });
    buckets
        .into_iter()
        .map(|bucket| GalleryGroup::new(Some(bucket.label), bucket.items))
        .collect()
}

fn stable_item_path(item: &LocalClipItem) -> Option<ClipPathIdentity> {
    item.path_identity()
}

fn smart_groups<'a>(
    items: &[&'a LocalClipItem],
    today_start: u64,
) -> Vec<GalleryGroup<&'a LocalClipItem>> {
    let yesterday_start = today_start.saturating_sub(86_400);
    let week_start = today_start.saturating_sub(7 * 86_400);
    let mut today = Vec::new();
    let mut yesterday = Vec::new();
    let mut this_week = Vec::new();
    let mut earlier = Vec::new();
    for &item in items {
        match item.modified_unix {
            timestamp if timestamp >= today_start => today.push(item),
            timestamp if timestamp >= yesterday_start => yesterday.push(item),
            timestamp if timestamp >= week_start => this_week.push(item),
            _ => earlier.push(item),
        }
    }
    let mut groups = [
        ("Today", today),
        ("Yesterday", yesterday),
        ("Earlier this week", this_week),
        ("Earlier", earlier),
    ];
    for (_, items) in &mut groups {
        sort_group_newest(items);
    }
    groups
        .into_iter()
        .filter_map(|(label, items)| {
            (!items.is_empty()).then(|| GalleryGroup::new(Some(label.to_owned()), items))
        })
        .collect()
}

fn sort_group_newest(items: &mut [&LocalClipItem]) {
    items.sort_by(|left, right| {
        right
            .modified_unix
            .cmp(&left.modified_unix)
            .then_with(|| stable_item_path(left).cmp(&stable_item_path(right)))
            .then_with(|| left.path.cmp(&right.path))
    });
}
