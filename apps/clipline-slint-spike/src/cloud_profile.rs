//! Bounded native Clipline Cloud profile rail and avatar ownership.

use std::io::Cursor;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Condvar, Mutex,
};

use clipline_library::{
    CloudAvatar, CloudUserProfile, CloudWorkToken, MAX_CATALOG_STRING_BYTES,
    MAX_FOREGROUND_MESSAGE_BYTES,
};
use image::{ImageFormat, ImageReader, Limits};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use thiserror::Error;

pub const MAX_CLOUD_AVATAR_DIMENSION: u32 = 8_192;
pub const MAX_CLOUD_AVATAR_PIXELS: u64 = 1_048_576;
pub const MAX_CLOUD_AVATAR_RGBA_BYTES: u64 = MAX_CLOUD_AVATAR_PIXELS * 4;
pub const MAX_CLOUD_AVATAR_DECODER_ALLOC_BYTES: u64 = 16 * 1024 * 1024;

const CLOUD_PROFILE_WORKERS: usize = 1;
const CLOUD_AVATAR_WORKERS: usize = 1;
const CLOUD_PROFILE_COMPLETION_CAPACITY: usize = 4;
const _: () = assert!(CLOUD_PROFILE_COMPLETION_CAPACITY >= 4);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloudProfileAdapterError {
    #[error("Cloud profile rail seed is invalid")]
    InvalidSeed,
    #[error("Cloud profile rail ticket space is exhausted")]
    TicketExhausted,
    #[error("Cloud avatar type is unsupported")]
    UnsupportedAvatar,
    #[error("Cloud avatar dimensions exceed the native bound")]
    AvatarDimensions,
    #[error("Cloud avatar decoded allocation exceeds the native bound")]
    AvatarAllocation,
    #[error("Cloud avatar could not be decoded")]
    AvatarDecode,
    #[error("Cloud profile worker is shut down")]
    WorkerClosed,
}

#[derive(Clone)]
pub struct CloudRailCancellation {
    inner: Arc<CloudRailCancellationInner>,
}

struct CloudRailCancellationInner {
    canceled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Default for CloudRailCancellation {
    fn default() -> Self {
        Self {
            inner: Arc::new(CloudRailCancellationInner {
                canceled: AtomicBool::new(false),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }
}

impl std::fmt::Debug for CloudRailCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudRailCancellation")
            .field("canceled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl CloudRailCancellation {
    pub fn cancel(&self) {
        if !self.inner.canceled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.canceled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

#[derive(Debug, Clone)]
pub struct CloudRailWork {
    pub token: CloudWorkToken,
    ticket: u64,
    cancellation: CloudRailCancellation,
}

impl CloudRailWork {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    #[must_use]
    pub fn cancellation(&self) -> &CloudRailCancellation {
        &self.cancellation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudRailSeed {
    pub token: CloudWorkToken,
    pub name: String,
}

impl CloudRailSeed {
    pub fn new(
        token: CloudWorkToken,
        name: impl Into<String>,
    ) -> Result<Self, CloudProfileAdapterError> {
        let name = name.into();
        if name.trim().is_empty() || name.len() > MAX_CATALOG_STRING_BYTES {
            return Err(CloudProfileAdapterError::InvalidSeed);
        }
        Ok(Self { token, name })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudRailProjection {
    pub visible: bool,
    pub name: String,
    pub initials: String,
    pub has_avatar: bool,
}

#[derive(Debug, Clone)]
pub struct CloudRailAttach {
    pub profile: CloudRailWork,
    pub avatar: CloudRailWork,
    pub clear_avatar: bool,
}

pub enum CloudProfileOutcome {
    Ready(CloudUserProfile),
    Stale,
    Failed(String),
}

pub enum CloudAvatarOutcome {
    Ready(SharedPixelBuffer<Rgba8Pixel>),
    Missing,
    Stale,
    Failed(String),
}

struct CurrentRail {
    profile: CloudRailWork,
    avatar: CloudRailWork,
    projection: CloudRailProjection,
}

pub struct CloudProfileController<Image> {
    current: Option<CurrentRail>,
    next_ticket: Option<u64>,
    marker: std::marker::PhantomData<fn() -> Image>,
}

impl<Image> Default for CloudProfileController<Image> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Image> CloudProfileController<Image> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: None,
            next_ticket: Some(1),
            marker: std::marker::PhantomData,
        }
    }

    pub fn attach(
        &mut self,
        seed: CloudRailSeed,
    ) -> Result<CloudRailAttach, CloudProfileAdapterError> {
        if seed.name.trim().is_empty() || seed.name.len() > MAX_CATALOG_STRING_BYTES {
            return Err(CloudProfileAdapterError::InvalidSeed);
        }
        let profile_ticket = self
            .next_ticket
            .ok_or(CloudProfileAdapterError::TicketExhausted)?;
        let avatar_ticket = profile_ticket
            .checked_add(1)
            .ok_or(CloudProfileAdapterError::TicketExhausted)?;
        let next_ticket = avatar_ticket.checked_add(1);
        self.cancel_current();
        let profile = CloudRailWork {
            token: seed.token.clone(),
            ticket: profile_ticket,
            cancellation: CloudRailCancellation::default(),
        };
        let avatar = CloudRailWork {
            token: seed.token.clone(),
            ticket: avatar_ticket,
            cancellation: CloudRailCancellation::default(),
        };
        let projection = CloudRailProjection {
            visible: true,
            name: seed.name.clone(),
            initials: rail_profile_initials(&seed.name),
            has_avatar: false,
        };
        self.next_ticket = next_ticket;
        self.current = Some(CurrentRail {
            profile: profile.clone(),
            avatar: avatar.clone(),
            projection,
        });
        Ok(CloudRailAttach {
            profile,
            avatar,
            clear_avatar: true,
        })
    }

    pub fn accept_profile(
        &mut self,
        work: &CloudRailWork,
        outcome: CloudProfileOutcome,
    ) -> Result<bool, CloudProfileAdapterError> {
        let Some(current) = self.current.as_mut() else {
            return Ok(false);
        };
        if !same_work(&current.profile, work) || work.is_cancelled() {
            return Ok(false);
        }
        let CloudProfileOutcome::Ready(profile) = outcome else {
            return Ok(false);
        };
        let name = profile_display_name(&profile);
        if name.len() > MAX_CATALOG_STRING_BYTES {
            return Err(CloudProfileAdapterError::InvalidSeed);
        }
        let initials = rail_profile_initials(&name);
        let changed = current.projection.name != name || current.projection.initials != initials;
        current.projection.name = name;
        current.projection.initials = initials;
        Ok(changed)
    }

    pub fn accept_avatar_with(
        &mut self,
        work: &CloudRailWork,
        outcome: CloudAvatarOutcome,
        construct: impl FnOnce(SharedPixelBuffer<Rgba8Pixel>) -> Image,
    ) -> Result<Option<Image>, CloudProfileAdapterError> {
        let Some(current) = self.current.as_mut() else {
            return Ok(None);
        };
        if !same_work(&current.avatar, work) || work.is_cancelled() {
            return Ok(None);
        }
        match outcome {
            CloudAvatarOutcome::Ready(pixels) => {
                current.projection.has_avatar = true;
                Ok(Some(construct(pixels)))
            }
            CloudAvatarOutcome::Missing | CloudAvatarOutcome::Failed(_) => {
                current.projection.has_avatar = false;
                Ok(None)
            }
            CloudAvatarOutcome::Stale => Ok(None),
        }
    }

    #[must_use]
    pub fn projection(&self) -> CloudRailProjection {
        self.current
            .as_ref()
            .map(|current| current.projection.clone())
            .unwrap_or_default()
    }

    pub fn detach(&mut self) -> bool {
        let had_avatar = self
            .current
            .as_ref()
            .is_some_and(|current| current.projection.has_avatar);
        self.cancel_current();
        had_avatar
    }

    fn cancel_current(&mut self) {
        if let Some(current) = self.current.take() {
            current.profile.cancellation.cancel();
            current.avatar.cancellation.cancel();
        }
    }
}

fn same_work(expected: &CloudRailWork, actual: &CloudRailWork) -> bool {
    expected.ticket == actual.ticket
        && expected.token == actual.token
        && expected.cancellation.same_request(&actual.cancellation)
}

fn profile_display_name(profile: &CloudUserProfile) -> String {
    profile
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let username = profile.username.trim();
            (!username.is_empty()).then_some(username)
        })
        .or_else(|| {
            let user_id = profile.user_id.trim();
            (!user_id.is_empty()).then_some(user_id)
        })
        .unwrap_or("Cloud")
        .to_owned()
}

#[must_use]
pub fn rail_profile_initials(name: &str) -> String {
    let parts: Vec<&str> = name
        .split(|character: char| character.is_whitespace() || matches!(character, '.' | '_' | '-'))
        .filter(|part| !part.is_empty())
        .collect();
    let value = if parts.len() >= 2 {
        let mut value = String::new();
        if let Some(character) = parts[0].chars().next() {
            value.push(character);
        }
        if let Some(character) = parts[1].chars().next() {
            value.push(character);
        }
        value
    } else {
        parts
            .first()
            .copied()
            .unwrap_or("C")
            .chars()
            .take(2)
            .collect()
    };
    let upper = value.to_uppercase();
    if upper.is_empty() {
        "C".to_owned()
    } else {
        upper
    }
}

pub fn decode_bounded_avatar(
    avatar: &CloudAvatar,
) -> Result<SharedPixelBuffer<Rgba8Pixel>, CloudProfileAdapterError> {
    let format = match avatar.content_type.as_str() {
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        _ => return Err(CloudProfileAdapterError::UnsupportedAvatar),
    };
    let dimensions = ImageReader::with_format(Cursor::new(avatar.bytes.as_slice()), format)
        .into_dimensions()
        .map_err(|_| CloudProfileAdapterError::AvatarDecode)?;
    let (width, height) = dimensions;
    if width == 0
        || height == 0
        || width > MAX_CLOUD_AVATAR_DIMENSION
        || height > MAX_CLOUD_AVATAR_DIMENSION
    {
        return Err(CloudProfileAdapterError::AvatarDimensions);
    }
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(CloudProfileAdapterError::AvatarAllocation)?;
    let rgba_bytes = pixel_count
        .checked_mul(4)
        .ok_or(CloudProfileAdapterError::AvatarAllocation)?;
    if pixel_count > MAX_CLOUD_AVATAR_PIXELS || rgba_bytes > MAX_CLOUD_AVATAR_RGBA_BYTES {
        return Err(CloudProfileAdapterError::AvatarAllocation);
    }

    let mut reader = ImageReader::with_format(Cursor::new(avatar.bytes.as_slice()), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_CLOUD_AVATAR_DIMENSION);
    limits.max_image_height = Some(MAX_CLOUD_AVATAR_DIMENSION);
    limits.max_alloc = Some(MAX_CLOUD_AVATAR_DECODER_ALLOC_BYTES);
    reader.limits(limits);
    let rgba = reader
        .decode()
        .map_err(|_| CloudProfileAdapterError::AvatarDecode)?
        .into_rgba8();
    if rgba.width() != width
        || rgba.height() != height
        || u64::try_from(rgba.as_raw().len()).ok() != Some(rgba_bytes)
    {
        return Err(CloudProfileAdapterError::AvatarDecode);
    }
    Ok(SharedPixelBuffer::clone_from_slice(
        rgba.as_raw(),
        width,
        height,
    ))
}

pub trait CloudProfileRequestPort: Send + Sync + 'static {
    fn profile(&self, work: &CloudRailWork) -> CloudProfileOutcome;
    fn avatar(&self, work: &CloudRailWork) -> CloudAvatarOutcome;
}

fn normalize_profile_outcome(outcome: CloudProfileOutcome) -> CloudProfileOutcome {
    match outcome {
        CloudProfileOutcome::Ready(profile)
            if profile.user_id.len() <= MAX_CATALOG_STRING_BYTES
                && profile.username.len() <= MAX_CATALOG_STRING_BYTES
                && profile
                    .display_name
                    .as_ref()
                    .is_none_or(|value| value.len() <= MAX_CATALOG_STRING_BYTES)
                && profile.profile_url.len() <= MAX_CATALOG_STRING_BYTES =>
        {
            CloudProfileOutcome::Ready(profile)
        }
        CloudProfileOutcome::Ready(_) => {
            CloudProfileOutcome::Failed("Cloud profile response exceeds its bound".to_owned())
        }
        CloudProfileOutcome::Failed(message) => {
            CloudProfileOutcome::Failed(bounded_cloud_profile_message(message))
        }
        CloudProfileOutcome::Stale => CloudProfileOutcome::Stale,
    }
}

fn normalize_avatar_outcome(outcome: CloudAvatarOutcome) -> CloudAvatarOutcome {
    match outcome {
        CloudAvatarOutcome::Ready(pixels)
            if pixels.width() > 0
                && pixels.height() > 0
                && pixels.width() <= MAX_CLOUD_AVATAR_DIMENSION
                && pixels.height() <= MAX_CLOUD_AVATAR_DIMENSION
                && u64::from(pixels.width()) * u64::from(pixels.height())
                    <= MAX_CLOUD_AVATAR_PIXELS =>
        {
            CloudAvatarOutcome::Ready(pixels)
        }
        CloudAvatarOutcome::Ready(_) => {
            CloudAvatarOutcome::Failed("Cloud avatar pixels exceed their bound".to_owned())
        }
        CloudAvatarOutcome::Failed(message) => {
            CloudAvatarOutcome::Failed(bounded_cloud_profile_message(message))
        }
        CloudAvatarOutcome::Missing => CloudAvatarOutcome::Missing,
        CloudAvatarOutcome::Stale => CloudAvatarOutcome::Stale,
    }
}

enum CloudRailCompletion {
    Profile {
        work: CloudRailWork,
        outcome: CloudProfileOutcome,
    },
    Avatar {
        work: CloudRailWork,
        outcome: CloudAvatarOutcome,
    },
}

struct MailboxState {
    pending: Option<CloudRailWork>,
    closed: bool,
}

struct LatestMailbox {
    state: Mutex<MailboxState>,
    ready: Condvar,
}

impl LatestMailbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState {
                pending: None,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn submit(&self, work: CloudRailWork) -> Result<(), CloudProfileAdapterError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CloudProfileAdapterError::WorkerClosed)?;
        if state.closed {
            return Err(CloudProfileAdapterError::WorkerClosed);
        }
        if let Some(previous) = state.pending.replace(work) {
            previous.cancellation.cancel();
        }
        self.ready.notify_one();
        Ok(())
    }

    fn receive(&self) -> Option<CloudRailWork> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(work) = state.pending.take() {
                return Some(work);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).ok()?;
        }
    }

    fn close(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.closed = true;
        if let Some(work) = state.pending.take() {
            work.cancellation.cancel();
        }
        self.ready.notify_all();
    }
}

struct CloudProfileWorkerPool {
    profile: Arc<LatestMailbox>,
    avatar: Arc<LatestMailbox>,
    completions: mpsc::Receiver<CloudRailCompletion>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl CloudProfileWorkerPool {
    fn start(port: Arc<dyn CloudProfileRequestPort>) -> Result<Self, String> {
        let profile = Arc::new(LatestMailbox::new());
        let avatar = Arc::new(LatestMailbox::new());
        let (completion_sender, completions) =
            mpsc::sync_channel(CLOUD_PROFILE_COMPLETION_CAPACITY);
        let mut pool = Self {
            profile: Arc::clone(&profile),
            avatar: Arc::clone(&avatar),
            completions,
            workers: Vec::new(),
        };
        pool.workers
            .try_reserve_exact(CLOUD_PROFILE_WORKERS + CLOUD_AVATAR_WORKERS)
            .map_err(|_| "reserve Cloud profile workers".to_owned())?;

        let profile_port = Arc::clone(&port);
        let profile_completions = completion_sender.clone();
        let profile_worker = std::thread::Builder::new()
            .name("clipline-cloud-profile".to_owned())
            .spawn(move || {
                while let Some(work) = profile.receive() {
                    let outcome = normalize_profile_outcome(profile_port.profile(&work));
                    if !work.is_cancelled() {
                        let _ = profile_completions
                            .try_send(CloudRailCompletion::Profile { work, outcome });
                    }
                }
            })
            .map_err(|error| format!("start Cloud profile worker: {error}"))?;
        pool.workers.push(profile_worker);

        let avatar_worker = match std::thread::Builder::new()
            .name("clipline-cloud-avatar".to_owned())
            .spawn(move || {
                while let Some(work) = avatar.receive() {
                    let outcome = normalize_avatar_outcome(port.avatar(&work));
                    if !work.is_cancelled() {
                        let _ = completion_sender
                            .try_send(CloudRailCompletion::Avatar { work, outcome });
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(error) => {
                pool.close();
                return Err(format!("start Cloud avatar worker: {error}"));
            }
        };
        pool.workers.push(avatar_worker);
        Ok(pool)
    }

    fn submit(&self, attached: &CloudRailAttach) -> Result<(), CloudProfileAdapterError> {
        self.profile.submit(attached.profile.clone())?;
        if let Err(error) = self.avatar.submit(attached.avatar.clone()) {
            attached.profile.cancellation.cancel();
            return Err(error);
        }
        Ok(())
    }

    fn close(&mut self) {
        self.profile.close();
        self.avatar.close();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

impl Drop for CloudProfileWorkerPool {
    fn drop(&mut self) {
        self.close();
    }
}

pub struct CloudProfilePump {
    pub changed: bool,
    pub avatar: Option<slint::Image>,
    pub clear_avatar: bool,
}

pub struct CloudProfileImageOwner {
    controller: CloudProfileController<slint::Image>,
    pool: CloudProfileWorkerPool,
}

impl CloudProfileImageOwner {
    pub fn start(port: Arc<dyn CloudProfileRequestPort>) -> Result<Self, String> {
        Ok(Self {
            controller: CloudProfileController::new(),
            pool: CloudProfileWorkerPool::start(port)?,
        })
    }

    pub fn attach(&mut self, seed: CloudRailSeed) -> Result<CloudRailProjection, String> {
        while self.pool.completions.try_recv().is_ok() {}
        let attached = self
            .controller
            .attach(seed)
            .map_err(|error| error.to_string())?;
        self.pool
            .submit(&attached)
            .map_err(|error| error.to_string())?;
        Ok(self.controller.projection())
    }

    pub fn pump_completions(&mut self) -> Result<CloudProfilePump, String> {
        let mut pump = CloudProfilePump {
            changed: false,
            avatar: None,
            clear_avatar: false,
        };
        for _ in 0..CLOUD_PROFILE_COMPLETION_CAPACITY {
            let completion = match self.pool.completions.try_recv() {
                Ok(completion) => completion,
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            };
            match completion {
                CloudRailCompletion::Profile { work, outcome } => {
                    pump.changed |= self
                        .controller
                        .accept_profile(&work, outcome)
                        .map_err(|error| error.to_string())?;
                }
                CloudRailCompletion::Avatar { work, outcome } => {
                    let terminal_without_image = matches!(
                        &outcome,
                        CloudAvatarOutcome::Missing | CloudAvatarOutcome::Failed(_)
                    );
                    let avatar = self
                        .controller
                        .accept_avatar_with(&work, outcome, slint::Image::from_rgba8)
                        .map_err(|error| error.to_string())?;
                    if avatar.is_some() {
                        pump.changed = true;
                        pump.avatar = avatar;
                    } else if terminal_without_image && !work.is_cancelled() {
                        pump.changed = true;
                        pump.clear_avatar = true;
                    }
                }
            }
        }
        Ok(pump)
    }

    #[must_use]
    pub fn projection(&self) -> CloudRailProjection {
        self.controller.projection()
    }

    pub fn detach_window(&mut self) -> bool {
        self.controller.detach()
    }

    pub fn shutdown(&mut self) {
        self.controller.detach();
        self.pool.close();
    }
}

impl Drop for CloudProfileImageOwner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[must_use]
pub const fn cloud_profile_worker_shape() -> (usize, usize, usize) {
    (
        CLOUD_PROFILE_WORKERS,
        CLOUD_AVATAR_WORKERS,
        CLOUD_PROFILE_COMPLETION_CAPACITY,
    )
}

#[must_use]
pub fn bounded_cloud_profile_message(mut value: String) -> String {
    if value.len() <= MAX_FOREGROUND_MESSAGE_BYTES {
        return value;
    }
    let mut end = MAX_FOREGROUND_MESSAGE_BYTES;
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}
