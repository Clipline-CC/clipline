//! Bounded native Cloud-thumbnail decode and UI ownership.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::sync::{mpsc, Arc, Mutex};

use clipline_library::cache::{
    CachedCloudAsset, CancellationProbe, CloudCancellation, CLOUD_THUMBNAIL_MAX_BYTES,
};
use clipline_library::cache_identity::CloudAssetKind;
use clipline_library::{
    CatalogItemIdentity, CatalogResult, CloudThumbnailDescriptor, CloudThumbnailManifest,
    CloudThumbnailOwner, PosterStatus, MAX_CATALOG_STRING_BYTES, MAX_DECODED_PAGE_IMAGES,
    MAX_POSTER_DECODED_PIXELS, MAX_POSTER_DIMENSION, MAX_POSTER_RGB_BYTES,
};
use slint::{Model as _, Rgb8Pixel, SharedPixelBuffer};
use thiserror::Error;

const CLOUD_THUMBNAIL_DECODE_WORKERS: usize = 2;
const CLOUD_THUMBNAIL_DECODE_QUEUE_CAPACITY: usize = MAX_DECODED_PAGE_IMAGES;
const _: () = assert!(CLOUD_THUMBNAIL_DECODE_WORKERS > 0);
const _: () = assert!(CLOUD_THUMBNAIL_DECODE_QUEUE_CAPACITY >= CLOUD_THUMBNAIL_DECODE_WORKERS);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloudThumbnailImageError {
    #[error("Cloud thumbnail manifest is invalid")]
    InvalidManifest,
    #[error("Cloud thumbnail decode ticket space is exhausted")]
    TicketExhausted,
    #[error("Cloud thumbnail completion does not match its issued ticket")]
    CompletionMismatch,
}

#[derive(Debug, Clone)]
pub struct CloudThumbnailDecodeWork {
    pub owner: CloudThumbnailOwner,
    ticket: u64,
    pub cancellation: CloudCancellation,
}

#[derive(Debug)]
pub struct CloudThumbnailControllerUpdate<Image> {
    pub queued: Vec<CloudThumbnailDecodeWork>,
    pub canceled: Vec<CloudCancellation>,
    pub released: Vec<Image>,
}

impl<Image> Default for CloudThumbnailControllerUpdate<Image> {
    fn default() -> Self {
        Self {
            queued: Vec::new(),
            canceled: Vec::new(),
            released: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AcceptedCloudThumbnail<Image> {
    pub row: usize,
    pub image: Image,
    pub report_status: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionState {
    Ready,
    Missing,
    Failed,
}

struct IssuedDecode {
    owner: CloudThumbnailOwner,
    cancellation: CloudCancellation,
    canceled: bool,
}

struct RetainedImage<Image> {
    owner: CloudThumbnailOwner,
    image: Image,
}

/// Pure UI-thread owner for one Cloud page's decoded image window.
///
/// Issued work continues to consume capacity after cancellation until its
/// exact ticket completes. This keeps `issued + retained <= 32` even during
/// rapid page and viewport churn.
pub struct CloudThumbnailImageController<Image: Clone> {
    manifest: Option<CloudThumbnailManifest>,
    ready: BTreeSet<CloudThumbnailDescriptor>,
    viewport_start: usize,
    issued: BTreeMap<u64, IssuedDecode>,
    retained: BTreeMap<CloudThumbnailDescriptor, RetainedImage<Image>>,
    completed: BTreeMap<CloudThumbnailDescriptor, CompletionState>,
    next_ticket: Option<u64>,
}

impl<Image: Clone> Default for CloudThumbnailImageController<Image> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Image: Clone> CloudThumbnailImageController<Image> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            manifest: None,
            ready: BTreeSet::new(),
            viewport_start: 0,
            issued: BTreeMap::new(),
            retained: BTreeMap::new(),
            completed: BTreeMap::new(),
            next_ticket: Some(1),
        }
    }

    pub fn sync(
        &mut self,
        manifest: Option<&CloudThumbnailManifest>,
        ready: &[CloudThumbnailDescriptor],
        viewport_start: usize,
    ) -> Result<CloudThumbnailControllerUpdate<Image>, CloudThumbnailImageError> {
        if let Some(manifest) = manifest {
            manifest
                .validate_bounds()
                .map_err(|_| CloudThumbnailImageError::InvalidManifest)?;
            if ready.iter().any(|descriptor| {
                !manifest
                    .owners()
                    .iter()
                    .any(|owner| owner.descriptor == *descriptor)
            }) {
                return Err(CloudThumbnailImageError::InvalidManifest);
            }
        } else if !ready.is_empty() {
            return Err(CloudThumbnailImageError::InvalidManifest);
        }
        let ready_set: BTreeSet<_> = ready.iter().cloned().collect();
        if ready_set.len() != ready.len() {
            return Err(CloudThumbnailImageError::InvalidManifest);
        }

        let mut update = CloudThumbnailControllerUpdate::default();
        let changed = self.manifest.as_ref() != manifest;
        if changed {
            self.cancel_all(&mut update);
            update.released.extend(
                std::mem::take(&mut self.retained)
                    .into_values()
                    .map(|entry| entry.image),
            );
            self.completed.clear();
            self.manifest = manifest.cloned();
        }
        self.ready = ready_set;
        self.viewport_start = viewport_start;

        let visible = self.visible_descriptors();
        let current_owners: Vec<_> = self
            .manifest
            .as_ref()
            .into_iter()
            .flat_map(CloudThumbnailManifest::owners)
            .cloned()
            .collect();
        for issued in self.issued.values_mut() {
            let current = current_owners.contains(&issued.owner)
                && visible.contains(&issued.owner.descriptor)
                && self.ready.contains(&issued.owner.descriptor);
            if !current && !issued.canceled {
                issued.canceled = true;
                issued.cancellation.cancel();
                update.canceled.push(issued.cancellation.clone());
            }
        }
        self.retained.retain(|descriptor, entry| {
            let keep = current_owners.contains(&entry.owner)
                && visible.contains(descriptor)
                && self.ready.contains(descriptor);
            if !keep {
                update.released.push(entry.image.clone());
            }
            keep
        });

        self.queue_available(&visible, &mut update)?;
        debug_assert!(self.ownership_count() <= MAX_DECODED_PAGE_IMAGES);
        Ok(update)
    }

    fn visible_descriptors(&self) -> BTreeSet<CloudThumbnailDescriptor> {
        let Some(manifest) = self.manifest.as_ref() else {
            return BTreeSet::new();
        };
        let start = self.viewport_start.min(manifest.owners().len());
        let end = start
            .saturating_add(MAX_DECODED_PAGE_IMAGES)
            .min(manifest.owners().len());
        manifest.owners()[start..end]
            .iter()
            .map(|owner| owner.descriptor.clone())
            .collect()
    }

    fn cancel_all(&mut self, update: &mut CloudThumbnailControllerUpdate<Image>) {
        for issued in self.issued.values_mut() {
            if !issued.canceled {
                issued.canceled = true;
                issued.cancellation.cancel();
                update.canceled.push(issued.cancellation.clone());
            }
        }
    }

    fn queue_available(
        &mut self,
        visible: &BTreeSet<CloudThumbnailDescriptor>,
        update: &mut CloudThumbnailControllerUpdate<Image>,
    ) -> Result<(), CloudThumbnailImageError> {
        let Some(manifest) = self.manifest.as_ref() else {
            return Ok(());
        };
        let owners = manifest.owners().to_vec();
        for owner in owners {
            if self.ownership_count() >= MAX_DECODED_PAGE_IMAGES {
                break;
            }
            if !visible.contains(&owner.descriptor) || !self.ready.contains(&owner.descriptor) {
                continue;
            }
            if self
                .retained
                .get(&owner.descriptor)
                .is_some_and(|entry| entry.owner == owner)
                || self.issued.values().any(|issued| issued.owner == owner)
                || matches!(
                    self.completed.get(&owner.descriptor),
                    Some(CompletionState::Missing | CompletionState::Failed)
                )
            {
                continue;
            }
            let ticket = self
                .next_ticket
                .ok_or(CloudThumbnailImageError::TicketExhausted)?;
            self.next_ticket = ticket.checked_add(1);
            let cancellation = CloudCancellation::default();
            self.issued.insert(
                ticket,
                IssuedDecode {
                    owner: owner.clone(),
                    cancellation: cancellation.clone(),
                    canceled: false,
                },
            );
            update.queued.push(CloudThumbnailDecodeWork {
                owner,
                ticket,
                cancellation,
            });
        }
        Ok(())
    }

    fn take_issued(
        &mut self,
        work: &CloudThumbnailDecodeWork,
    ) -> Result<Option<IssuedDecode>, CloudThumbnailImageError> {
        let Some(issued) = self.issued.remove(&work.ticket) else {
            return Ok(None);
        };
        if issued.owner != work.owner {
            self.issued.insert(work.ticket, issued);
            return Err(CloudThumbnailImageError::CompletionMismatch);
        }
        Ok(Some(issued))
    }

    fn is_current(&self, work: &CloudThumbnailDecodeWork, issued: &IssuedDecode) -> bool {
        !issued.canceled
            && !work.cancellation.is_cancelled()
            && self
                .manifest
                .as_ref()
                .is_some_and(|manifest| manifest.owners().iter().any(|owner| owner == &work.owner))
            && self.visible_descriptors().contains(&work.owner.descriptor)
            && self.ready.contains(&work.owner.descriptor)
    }

    pub fn accept_ready_with(
        &mut self,
        work: &CloudThumbnailDecodeWork,
        construct: impl FnOnce() -> Image,
    ) -> Result<Option<AcceptedCloudThumbnail<Image>>, CloudThumbnailImageError> {
        let Some(issued) = self.take_issued(work)? else {
            return Ok(None);
        };
        if !self.is_current(work, &issued) {
            return Ok(None);
        }
        let Some(row) = self.manifest.as_ref().and_then(|manifest| {
            manifest
                .owners()
                .iter()
                .position(|owner| owner == &work.owner)
        }) else {
            return Ok(None);
        };
        let report_status = !self.completed.contains_key(&work.owner.descriptor);
        let image = construct();
        self.retained.insert(
            work.owner.descriptor.clone(),
            RetainedImage {
                owner: work.owner.clone(),
                image: image.clone(),
            },
        );
        self.completed
            .insert(work.owner.descriptor.clone(), CompletionState::Ready);
        debug_assert!(self.ownership_count() <= MAX_DECODED_PAGE_IMAGES);
        Ok(Some(AcceptedCloudThumbnail {
            row,
            image,
            report_status,
        }))
    }

    pub fn accept_missing(
        &mut self,
        work: &CloudThumbnailDecodeWork,
    ) -> Result<bool, CloudThumbnailImageError> {
        self.accept_terminal(work, CompletionState::Missing)
    }

    pub fn accept_failed(
        &mut self,
        work: &CloudThumbnailDecodeWork,
    ) -> Result<bool, CloudThumbnailImageError> {
        self.accept_terminal(work, CompletionState::Failed)
    }

    fn accept_terminal(
        &mut self,
        work: &CloudThumbnailDecodeWork,
        state: CompletionState,
    ) -> Result<bool, CloudThumbnailImageError> {
        let Some(issued) = self.take_issued(work)? else {
            return Ok(false);
        };
        if !self.is_current(work, &issued) {
            return Ok(false);
        }
        let report = !self.completed.contains_key(&work.owner.descriptor);
        self.completed.insert(work.owner.descriptor.clone(), state);
        Ok(report)
    }

    pub fn accept_abandoned(
        &mut self,
        work: &CloudThumbnailDecodeWork,
    ) -> Result<bool, CloudThumbnailImageError> {
        Ok(self.take_issued(work)?.is_some())
    }

    #[must_use]
    pub fn retained_image(&self, identity: &CatalogItemIdentity) -> Option<&Image> {
        self.retained
            .iter()
            .find_map(|(descriptor, entry)| (&descriptor.item == identity).then_some(&entry.image))
    }

    #[must_use]
    pub fn retained_count(&self) -> usize {
        self.retained.len()
    }

    #[must_use]
    pub fn ownership_count(&self) -> usize {
        self.issued.len().saturating_add(self.retained.len())
    }
}

#[must_use]
pub const fn cloud_thumbnail_decode_pool_shape() -> (usize, usize) {
    (
        CLOUD_THUMBNAIL_DECODE_WORKERS,
        CLOUD_THUMBNAIL_DECODE_QUEUE_CAPACITY,
    )
}

pub fn validate_cloud_thumbnail_dimensions(
    width: u32,
    height: u32,
) -> Result<usize, CloudThumbnailImageError> {
    if width == 0 || height == 0 || width > MAX_POSTER_DIMENSION || height > MAX_POSTER_DIMENSION {
        return Err(CloudThumbnailImageError::InvalidManifest);
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(CloudThumbnailImageError::InvalidManifest)?;
    if pixels > MAX_POSTER_DECODED_PIXELS {
        return Err(CloudThumbnailImageError::InvalidManifest);
    }
    let bytes = pixels
        .checked_mul(3)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(CloudThumbnailImageError::InvalidManifest)?;
    if bytes > MAX_POSTER_RGB_BYTES {
        return Err(CloudThumbnailImageError::InvalidManifest);
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CloudThumbnailFileError {
    #[error("Cloud thumbnail cache entry is not a thumbnail")]
    InvalidAsset,
    #[error("Cloud thumbnail file could not be opened safely")]
    Open,
    #[error("Cloud thumbnail file identity changed")]
    Identity,
    #[error("Cloud thumbnail encoded bytes exceed their limit")]
    EncodedTooLarge,
    #[error("Cloud thumbnail encoded bytes changed after cache acceptance")]
    EncodedChanged,
    #[error("Cloud thumbnail encoded bytes could not be read")]
    Read,
    #[error("Cloud thumbnail JPEG is corrupt or exceeds decoded bounds")]
    Decode,
}

impl CloudThumbnailFileError {
    #[must_use]
    pub const fn invalidates_cache(self) -> bool {
        matches!(
            self,
            Self::EncodedTooLarge | Self::EncodedChanged | Self::Decode
        )
    }
}

/// Read and decode one exact cache entry while the caller retains its
/// transient pin. The file handle and decoder are gone before pixels return.
pub fn decode_cached_cloud_thumbnail(
    cached: &CachedCloudAsset,
) -> Result<SharedPixelBuffer<Rgb8Pixel>, CloudThumbnailFileError> {
    if cached.asset().kind() != CloudAssetKind::Thumbnail {
        return Err(CloudThumbnailFileError::InvalidAsset);
    }
    if cached.bytes() > CLOUD_THUMBNAIL_MAX_BYTES {
        return Err(CloudThumbnailFileError::EncodedTooLarge);
    }
    let file = clipline_shell::open_regular_file_nofollow(cached.path())
        .map_err(|_| CloudThumbnailFileError::Open)?;
    let identity = clipline_shell::opened_file_identity(&file)
        .map_err(|_| CloudThumbnailFileError::Identity)?;
    if identity != cached.identity() {
        return Err(CloudThumbnailFileError::Identity);
    }
    let encoded_len = file
        .metadata()
        .map_err(|_| CloudThumbnailFileError::Read)?
        .len();
    if encoded_len > CLOUD_THUMBNAIL_MAX_BYTES {
        return Err(CloudThumbnailFileError::EncodedTooLarge);
    }
    if encoded_len != cached.bytes() {
        return Err(CloudThumbnailFileError::EncodedChanged);
    }
    let capacity =
        usize::try_from(encoded_len).map_err(|_| CloudThumbnailFileError::EncodedTooLarge)?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| CloudThumbnailFileError::EncodedTooLarge)?;
    file.take(CLOUD_THUMBNAIL_MAX_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|_| CloudThumbnailFileError::Read)?;
    if encoded.len() as u64 > CLOUD_THUMBNAIL_MAX_BYTES {
        return Err(CloudThumbnailFileError::EncodedTooLarge);
    }
    if encoded.len() as u64 != encoded_len {
        return Err(CloudThumbnailFileError::EncodedChanged);
    }
    crate::poster::decode_bounded_jpeg_bytes(encoded).map_err(|_| CloudThumbnailFileError::Decode)
}

pub enum CloudThumbnailDecodeOutcome {
    Ready {
        pixels: SharedPixelBuffer<Rgb8Pixel>,
    },
    Missing,
    Stale,
    Failed(String),
}

/// Blocking cache/decode boundary executed only by the fixed worker pool.
pub trait CloudThumbnailDecodePort: Send + Sync + 'static {
    fn decode(
        &self,
        owner: &CloudThumbnailOwner,
        cancellation: &CloudCancellation,
    ) -> CloudThumbnailDecodeOutcome;
}

struct CloudThumbnailDecodeJob {
    work: CloudThumbnailDecodeWork,
}

struct CloudThumbnailDecodeCompletion {
    work: CloudThumbnailDecodeWork,
    outcome: CloudThumbnailDecodeOutcome,
}

struct CloudThumbnailDecodePool {
    sender: Option<mpsc::SyncSender<CloudThumbnailDecodeJob>>,
    completions: mpsc::Receiver<CloudThumbnailDecodeCompletion>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl CloudThumbnailDecodePool {
    fn start(port: Arc<dyn CloudThumbnailDecodePort>) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(CLOUD_THUMBNAIL_DECODE_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let (completion_sender, completions) =
            mpsc::sync_channel(CLOUD_THUMBNAIL_DECODE_QUEUE_CAPACITY);
        let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        workers
            .try_reserve_exact(CLOUD_THUMBNAIL_DECODE_WORKERS)
            .map_err(|_| "reserve Cloud thumbnail decoder workers".to_owned())?;
        for index in 0..CLOUD_THUMBNAIL_DECODE_WORKERS {
            let worker_receiver = Arc::clone(&receiver);
            let completion_sender = completion_sender.clone();
            let port = Arc::clone(&port);
            let worker = match std::thread::Builder::new()
                .name(format!("clipline-cloud-thumbnail-{index}"))
                .spawn(move || loop {
                    let job = worker_receiver
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .recv();
                    let Ok(CloudThumbnailDecodeJob { work }) = job else {
                        return;
                    };
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        port.decode(&work.owner, &work.cancellation)
                    }))
                    .unwrap_or_else(|_| {
                        CloudThumbnailDecodeOutcome::Failed(
                            "Cloud thumbnail decoder worker panicked".to_owned(),
                        )
                    });
                    if completion_sender
                        .send(CloudThumbnailDecodeCompletion { work, outcome })
                        .is_err()
                    {
                        return;
                    }
                }) {
                Ok(worker) => worker,
                Err(error) => {
                    drop(sender);
                    drop(receiver);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(format!("start Cloud thumbnail decoder worker: {error}"));
                }
            };
            workers.push(worker);
        }
        drop(completion_sender);
        Ok(Self {
            sender: Some(sender),
            completions,
            workers,
        })
    }

    fn try_submit(
        &self,
        work: CloudThumbnailDecodeWork,
    ) -> Result<(), Box<CloudThumbnailDecodeWork>> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(Box::new(work));
        };
        sender
            .try_send(CloudThumbnailDecodeJob { work })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job) => {
                    Box::new(job.work)
                }
            })
    }

    fn shutdown(&mut self) -> Result<(), String> {
        self.sender.take();
        let mut first_error = None;
        for worker in self.workers.drain(..) {
            if worker.join().is_err() && first_error.is_none() {
                first_error = Some("Cloud thumbnail decoder worker panicked".to_owned());
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

pub struct CloudThumbnailPump {
    pub images_changed: bool,
    pub results: Vec<CatalogResult>,
}

/// Shell-owned Cloud image runtime. It lives across window recreation, while
/// `reconcile(None, ..)` drops every window-scoped image and cancels jobs.
pub struct CloudThumbnailImageOwner {
    controller: CloudThumbnailImageController<slint::Image>,
    pool: CloudThumbnailDecodePool,
    manifest: Option<CloudThumbnailManifest>,
    ready: Vec<CloudThumbnailDescriptor>,
    viewport_start: usize,
}

impl CloudThumbnailImageOwner {
    pub fn start(port: Arc<dyn CloudThumbnailDecodePort>) -> Result<Self, String> {
        Ok(Self {
            controller: CloudThumbnailImageController::new(),
            pool: CloudThumbnailDecodePool::start(port)?,
            manifest: None,
            ready: Vec::new(),
            viewport_start: 0,
        })
    }

    pub fn reconcile(
        &mut self,
        manifest: Option<&CloudThumbnailManifest>,
        ready: &[CloudThumbnailDescriptor],
        viewport_start: usize,
    ) -> Result<bool, String> {
        self.manifest = manifest.cloned();
        self.ready = ready.to_vec();
        self.viewport_start = viewport_start;
        let update = self
            .controller
            .sync(manifest, ready, viewport_start)
            .map_err(|error| error.to_string())?;
        let changed = !update.released.is_empty();
        drop(update.released);
        drop(update.canceled);
        self.submit(update.queued)?;
        Ok(changed)
    }

    fn submit(&mut self, queued: Vec<CloudThumbnailDecodeWork>) -> Result<(), String> {
        let mut queued = queued.into_iter();
        while let Some(work) = queued.next() {
            if let Err(work) = self.pool.try_submit(work) {
                self.controller
                    .accept_abandoned(work.as_ref())
                    .map_err(|error| error.to_string())?;
                for unsent in queued {
                    self.controller
                        .accept_abandoned(&unsent)
                        .map_err(|error| error.to_string())?;
                }
                return Err("Cloud thumbnail decoder queue is unavailable".to_owned());
            }
        }
        Ok(())
    }

    pub fn pump_completions(
        &mut self,
        window: Option<&crate::CliplineSpike>,
    ) -> Result<CloudThumbnailPump, String> {
        let mut images_changed = false;
        let mut results = Vec::new();
        while let Ok(completion) = self.pool.completions.try_recv() {
            match completion.outcome {
                CloudThumbnailDecodeOutcome::Ready { pixels } => {
                    let accepted = self
                        .controller
                        .accept_ready_with(&completion.work, || slint::Image::from_rgb8(pixels))
                        .map_err(|error| error.to_string())?;
                    if let Some(accepted) = accepted {
                        images_changed = true;
                        if let Some(window) = window {
                            let model = window.get_library_items();
                            if let Some(mut row) = model.row_data(accepted.row) {
                                row.poster_image = accepted.image;
                                model.set_row_data(accepted.row, row);
                            }
                        }
                    }
                }
                CloudThumbnailDecodeOutcome::Missing => {
                    if self
                        .controller
                        .accept_failed(&completion.work)
                        .map_err(|error| error.to_string())?
                    {
                        results.push(CatalogResult::CloudThumbnail {
                            owner: completion.work.owner,
                            status: PosterStatus::Failed {
                                message: "Cloud thumbnail disappeared before native decode"
                                    .to_owned(),
                            },
                        });
                    }
                }
                CloudThumbnailDecodeOutcome::Failed(message) => {
                    if self
                        .controller
                        .accept_failed(&completion.work)
                        .map_err(|error| error.to_string())?
                    {
                        results.push(CatalogResult::CloudThumbnail {
                            owner: completion.work.owner,
                            status: PosterStatus::Failed {
                                message: bounded_message(message),
                            },
                        });
                    }
                }
                CloudThumbnailDecodeOutcome::Stale => {
                    self.controller
                        .accept_abandoned(&completion.work)
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        let refill = self
            .controller
            .sync(self.manifest.as_ref(), &self.ready, self.viewport_start)
            .map_err(|error| error.to_string())?;
        images_changed |= !refill.released.is_empty();
        drop(refill.released);
        drop(refill.canceled);
        self.submit(refill.queued)?;
        Ok(CloudThumbnailPump {
            images_changed,
            results,
        })
    }

    #[must_use]
    pub fn image_for(&self, identity: &CatalogItemIdentity) -> Option<slint::Image> {
        self.controller.retained_image(identity).cloned()
    }

    pub fn detach_window(&mut self) -> Result<bool, String> {
        self.reconcile(None, &[], 0)
    }

    #[must_use]
    pub fn retained_image_count(&self) -> usize {
        self.controller.retained_count()
    }

    #[must_use]
    pub fn ownership_count(&self) -> usize {
        self.controller.ownership_count()
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        let _ = self.detach_window();
        self.pool.shutdown()
    }
}

impl Drop for CloudThumbnailImageOwner {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn bounded_message(mut message: String) -> String {
    if message.len() <= MAX_CATALOG_STRING_BYTES {
        return message;
    }
    let mut end = MAX_CATALOG_STRING_BYTES;
    while end != 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}
