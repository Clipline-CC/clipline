//! Isolated, deterministic native Library measurement harness.
//!
//! This is deliberately an example rather than a shipping entry point. It
//! validates a sampler-owned hard-link fixture, drives the real bounded Rust
//! catalog controller, and publishes the real Slint catalog models. Cloud
//! pages and progress are synthetic: this process never opens settings,
//! credentials, or a network client.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    Arc, Barrier, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clipline_library::{
    ActiveFileRegistry, CatalogEffect, CatalogItemIdentity, CatalogResult, CatalogRevision,
    CatalogSource, CloudAccountGeneration, CloudAccountKey, CloudCatalogOwner, CloudLibraryItem,
    CloudListPageCompletion, DurableUploadToken, ForegroundGeneration, LocalClipFilter,
    LocalClipGrouping, LocalClipId, LocalDay, LocalDayResolver, MAX_CATALOG_PAGE_ROWS,
    MAX_DECODED_PAGE_IMAGES, MAX_POSTER_RESULT_ENTRIES, PosterCompletion, PosterController,
    PosterPageItem, PosterService, PosterWorkKind, RequestGeneration, UploadGeneration,
    UploadSummary, WindowAttachmentGeneration, WindowWorkToken,
};
use clipline_shell::{LaunchMode, ShellCommand};
use clipline_slint_spike::catalog::{
    CatalogEffectHandler, CatalogUiIntent, LocalCatalogEffectHandler, SlintCatalogController,
    publish_projection,
};
use clipline_slint_spike::desktop::{DesktopAttachment, SlintDesktopAdapter};
use clipline_slint_spike::poster::{decode_poster_file, publish_decoded_poster};
use clipline_slint_spike::shell::{AttachmentToken, LifecycleAction, ShellLifecycle};
use clipline_slint_spike::{CliplineSpike, create_window};
use serde::Serialize;
use sha2::{Digest, Sha256};
use slint::{ComponentHandle, Model, Rgb8Pixel, SharedPixelBuffer};

const TELEMETRY_LIMIT: usize = 1024 * 1024;
const CHURN_CYCLES: usize = 100;
const PAGE_SAMPLES: usize = 20;
const STOP_POLL: Duration = Duration::from_millis(10);
const SETUP_TICK: Duration = Duration::from_millis(16);
const ALLOWED_COUNTS: [usize; 3] = [50, 500, 2_000];
const MAX_HARD_LINKS_PER_SEED: usize = 500;

type HarnessResult<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> HarnessResult<()> {
    let started_at = current_process_started_at(SystemTime::now());
    let options = match Options::parse(std::env::args_os()) {
        Ok(ParseOutcome::Run(options)) => options,
        Ok(ParseOutcome::Help) => {
            println!("{}", Options::usage());
            return Ok(());
        }
        Err(error) => {
            eprintln!("{error}\n{}", Options::usage());
            return Err(error);
        }
    };
    let source_sha256 = sha256_file(&options.source_fixture)?;
    if !source_sha256.eq_ignore_ascii_case(&options.source_sha256) {
        return Err(format!(
            "source fixture hash mismatch: expected {}, got {source_sha256}",
            options.source_sha256
        )
        .into());
    }
    let provenance = Provenance::new(&options, source_sha256, started_at);
    let markers = MarkerSink::create(&options.marker_path, provenance.clone())?;
    let outcome = run_harness(&options, &provenance, &markers, started_at);
    match outcome {
        Ok(telemetry) => {
            write_atomic_json(&options.telemetry_path, &telemetry)?;
            markers.write("stop", "owned stop observed; telemetry published")?;
            Ok(())
        }
        Err(error) => {
            let _ = markers.write("error", &bounded_detail(&error.to_string()));
            Err(error)
        }
    }
}

fn run_harness(
    options: &Options,
    provenance: &Provenance,
    markers: &MarkerSink,
    process_started_at: SystemTime,
) -> HarnessResult<Telemetry> {
    if options.stop_path.exists() {
        return Err("stop path already exists before harness launch".into());
    }
    if options.exercise_path.exists() {
        return Err("exercise path already exists before harness launch".into());
    }
    let validation_started = Instant::now();
    let fixture_paths = validate_hard_link_fixture(options, &provenance.source_sha256)?;
    let validation_ms = elapsed_ms(validation_started);
    let initial_poster_files = count_poster_files(&options.fixture_root)?;
    if options.scenario == Scenario::LocalCold && initial_poster_files != 0 {
        return Err("local-cold fixture already contains poster cache files".into());
    }
    if options.scenario == Scenario::LocalWarm && initial_poster_files == 0 {
        return Err("local-warm fixture has no pre-generated poster cache files".into());
    }
    if options.scenario == Scenario::LocalWarm {
        validate_warm_cache(&fixture_paths)?;
    }

    std::env::set_var("SLINT_BACKEND", &options.renderer);
    let window = create_window()?;
    let days: Arc<dyn LocalDayResolver> = Arc::new(HarnessDays);
    let mut catalog = SlintCatalogController::new(days)?;
    // Keep the scanner's displayed paths in the same canonical namespace as
    // PosterService's identity-fenced output. That makes the controller's
    // exact `Ready(path)` ownership comparison meaningful on Windows too.
    let canonical_fixture_root = options.fixture_root.canonicalize()?;
    let handler =
        LocalCatalogEffectHandler::open(&canonical_fixture_root, ActiveFileRegistry::new())?;
    let mut lifecycle = Lifecycle::default();
    let mut metrics = Metrics::default();
    let mut retained_model_images = 0;

    let initial_effects = catalog.attach(
        WindowAttachmentGeneration::new(1),
        ForegroundGeneration::new(1),
    )?;
    lifecycle.attachments_created = checked_add(lifecycle.attachments_created, 1)?;
    accept_local_refresh(&mut catalog, &handler, initial_effects)?;
    publish_and_record(
        &window,
        &catalog,
        &BTreeMap::new(),
        &mut metrics,
        &mut lifecycle,
        &mut retained_model_images,
    )?;

    let poster_service = Arc::new(PosterService::standard());
    let state = Rc::new(RefCell::new(HarnessState {
        catalog,
        posters: PosterController::new(),
        metrics,
        lifecycle,
        churn: Churn::default(),
        reveal: Reveal::default(),
        retained_model_images,
    }));
    window.show()?;
    window.window().request_redraw();
    let setup = make_setup_work(
        window.as_weak(),
        Rc::clone(&state),
        Arc::clone(&poster_service),
        options.scenario,
        options.fixture_root.clone(),
    );
    let exercise = match options.scenario {
        Scenario::SelectionPageChurn => Some(make_selection_churn_work(
            window.as_weak(),
            Rc::clone(&state),
            fixture_paths.clone(),
        )),
        Scenario::RevealClose100 => Some(make_reveal_close_work(Rc::clone(&state))?),
        Scenario::LocalCold | Scenario::LocalWarm | Scenario::CloudPages => None,
    };
    let event_report = run_measured_event_loop(
        &options.stop_path,
        &options.exercise_path,
        markers,
        process_started_at,
        setup,
        exercise,
    )?;
    let mut state = Rc::try_unwrap(state)
        .map_err(|_| "catalog event-loop work retained harness state")?
        .into_inner();
    state.metrics.window_shown_model_published = true;
    state.metrics.first_usable_page_ms = event_report.first_usable_page_ms;
    // Drop every Slint model-held image clone while the component is still
    // alive, then release the controller-owned decoded handles below. This
    // makes the balanced lifecycle counters precede telemetry publication.
    publish_projection(&window, &state.catalog.projection(), |_| None)?;
    state
        .lifecycle
        .replace_model_images(&mut state.retained_model_images, 0)?;
    window.hide()?;

    let detached = state.posters.detach_window()?;
    apply_poster_teardown(&mut state.posters, detached, &mut state.lifecycle)?;
    state.catalog.detach()?;
    state.lifecycle.attachments_dropped = checked_add(state.lifecycle.attachments_dropped, 1)?;
    state.metrics.retained_decoded_images = state
        .metrics
        .retained_decoded_images
        .max(state.posters.retained_image_count());
    state.metrics.poster_lru_entries = state
        .metrics
        .poster_lru_entries
        .max(state.posters.cache_len());
    validate_internal_gates(
        &mut state.metrics,
        &state.lifecycle,
        &state.churn,
        &state.reveal,
        options.scenario,
    )?;

    Ok(Telemetry {
        schema_version: 1,
        status: "completed",
        publication: "create-new-atomic-rename",
        scenario: options.scenario.as_str(),
        clip_count: options.clip_count,
        source_fixture: SourceFixture {
            path: provenance.source_fixture.clone(),
            sha256: provenance.source_sha256.clone(),
        },
        provenance: provenance.clone(),
        metrics: state.metrics,
        lifecycle: state.lifecycle,
        churn: state.churn,
        reveal: state.reveal,
        safety: Safety {
            production_credentials_loaded: false,
            cloud_network_requests: 0,
        },
        validation_ms,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    LocalCold,
    LocalWarm,
    CloudPages,
    SelectionPageChurn,
    RevealClose100,
}

impl Scenario {
    fn parse(value: &str) -> HarnessResult<Self> {
        match value {
            "local-cold" => Ok(Self::LocalCold),
            "local-warm" => Ok(Self::LocalWarm),
            "cloud-pages" => Ok(Self::CloudPages),
            "selection-page-churn" => Ok(Self::SelectionPageChurn),
            "reveal-close-100" => Ok(Self::RevealClose100),
            _ => Err(format!("unsupported catalog scenario: {value}").into()),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::LocalCold => "local-cold",
            Self::LocalWarm => "local-warm",
            Self::CloudPages => "cloud-pages",
            Self::SelectionPageChurn => "selection-page-churn",
            Self::RevealClose100 => "reveal-close-100",
        }
    }
}

struct Options {
    fixture_root: PathBuf,
    fixture_seed_root: PathBuf,
    source_fixture: PathBuf,
    source_sha256: String,
    clip_count: usize,
    scenario: Scenario,
    marker_path: PathBuf,
    stop_path: PathBuf,
    exercise_path: PathBuf,
    telemetry_path: PathBuf,
    renderer: String,
    build_sha: String,
    adapter: String,
    scale: f64,
}

enum ParseOutcome {
    Run(Box<Options>),
    Help,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = OsString>) -> HarnessResult<ParseOutcome> {
        let mut args = args.into_iter();
        let _program = args.next();
        let mut values = BTreeMap::<String, String>::new();
        while let Some(argument) = args.next() {
            let argument = argument
                .into_string()
                .map_err(|_| "catalog harness arguments must be Unicode")?;
            if argument == "--help" || argument == "-h" {
                return Ok(ParseOutcome::Help);
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {argument}"))?
                .into_string()
                .map_err(|_| format!("value for {argument} must be Unicode"))?;
            if !matches!(
                argument.as_str(),
                "--fixture-root"
                    | "--fixture-seed-root"
                    | "--source-fixture"
                    | "--source-sha256"
                    | "--clip-count"
                    | "--scenario"
                    | "--marker-path"
                    | "--stop-path"
                    | "--exercise-path"
                    | "--telemetry-path"
                    | "--renderer"
                    | "--build-sha"
                    | "--adapter"
                    | "--scale"
            ) {
                return Err(format!("unknown catalog harness argument: {argument}").into());
            }
            if values.insert(argument.clone(), value).is_some() {
                return Err(format!("duplicate catalog harness argument: {argument}").into());
            }
        }
        let required = |name: &str| -> HarnessResult<String> {
            values
                .get(name)
                .cloned()
                .ok_or_else(|| format!("missing required argument {name}").into())
        };
        let clip_count = required("--clip-count")?.parse::<usize>()?;
        if !ALLOWED_COUNTS.contains(&clip_count) {
            return Err("--clip-count must be exactly 50, 500, or 2000".into());
        }
        let renderer = values
            .get("--renderer")
            .cloned()
            .unwrap_or_else(|| "winit-software".to_owned());
        if renderer != "winit-software" {
            return Err("catalog harness supports only --renderer winit-software".into());
        }
        let scale = values
            .get("--scale")
            .map_or(Ok(1.0), |value| value.parse::<f64>())?;
        if !scale.is_finite() || scale <= 0.0 {
            return Err("--scale must be finite and positive".into());
        }
        let source_sha256 = required("--source-sha256")?;
        if source_sha256.len() != 64 || !source_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("--source-sha256 must be 64 hexadecimal characters".into());
        }
        Ok(ParseOutcome::Run(Box::new(Self {
            fixture_root: PathBuf::from(required("--fixture-root")?),
            fixture_seed_root: PathBuf::from(required("--fixture-seed-root")?),
            source_fixture: PathBuf::from(required("--source-fixture")?),
            source_sha256,
            clip_count,
            scenario: Scenario::parse(&required("--scenario")?)?,
            marker_path: PathBuf::from(required("--marker-path")?),
            stop_path: PathBuf::from(required("--stop-path")?),
            exercise_path: PathBuf::from(required("--exercise-path")?),
            telemetry_path: PathBuf::from(required("--telemetry-path")?),
            renderer,
            build_sha: values
                .get("--build-sha")
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned()),
            adapter: values
                .get("--adapter")
                .cloned()
                .unwrap_or_else(|| "unknown".to_owned()),
            scale,
        })))
    }

    const fn usage() -> &'static str {
        "catalog_harness --fixture-root <isolated-dir> --fixture-seed-root <isolated-seeds> --source-fixture <mp4> --source-sha256 <hex> \
         --clip-count <50|500|2000> --scenario <local-cold|local-warm|cloud-pages|selection-page-churn|reveal-close-100> \
         --marker-path <create-new.jsonl> --stop-path <file> --exercise-path <file> --telemetry-path <create-new.json> \
         [--renderer winit-software] [--build-sha <sha>] [--adapter <name>] [--scale <factor>]"
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Provenance {
    build_sha: String,
    renderer: String,
    adapter: String,
    scale: f64,
    process_id: u32,
    process_name: &'static str,
    process_start_unix_ms: u128,
    machine: String,
    fixture_root: String,
    fixture_seed_root: String,
    source_fixture: String,
    source_sha256: String,
}

impl Provenance {
    fn new(options: &Options, source_sha256: String, started_at: SystemTime) -> Self {
        Self {
            build_sha: options.build_sha.clone(),
            renderer: options.renderer.clone(),
            adapter: options.adapter.clone(),
            scale: options.scale,
            process_id: std::process::id(),
            process_name: "catalog_harness",
            process_start_unix_ms: unix_millis(started_at),
            machine: std::env::var("COMPUTERNAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "unknown".to_owned()),
            fixture_root: options.fixture_root.display().to_string(),
            fixture_seed_root: options.fixture_seed_root.display().to_string(),
            source_fixture: options.source_fixture.display().to_string(),
            source_sha256,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Marker<'a> {
    schema_version: u8,
    kind: &'a str,
    timestamp_utc: String,
    detail: &'a str,
    provenance: &'a Provenance,
}

#[derive(Clone)]
struct MarkerSink {
    file: Arc<Mutex<File>>,
    provenance: Arc<Provenance>,
}

impl MarkerSink {
    fn create(path: &Path, provenance: Provenance) -> HarnessResult<Self> {
        let file = OpenOptions::new().create_new(true).write(true).open(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            provenance: Arc::new(provenance),
        })
    }

    fn write(&self, kind: &str, detail: &str) -> HarnessResult<()> {
        let marker = Marker {
            schema_version: 1,
            kind,
            timestamp_utc: utc_timestamp(SystemTime::now()),
            detail,
            provenance: self.provenance.as_ref(),
        };
        let mut file = self
            .file
            .lock()
            .map_err(|_| "catalog marker lock poisoned")?;
        serde_json::to_writer(&mut *file, &marker)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Telemetry {
    schema_version: u8,
    status: &'static str,
    publication: &'static str,
    scenario: &'static str,
    clip_count: usize,
    source_fixture: SourceFixture,
    provenance: Provenance,
    metrics: Metrics,
    lifecycle: Lifecycle,
    churn: Churn,
    reveal: Reveal,
    safety: Safety,
    validation_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceFixture {
    path: String,
    sha256: String,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metrics {
    first_usable_page_ms: f64,
    window_shown_model_published: bool,
    page_change_p95_ms: f64,
    filter_group_p95_ms: f64,
    poster_settle_ms: f64,
    retained_rows: usize,
    retained_decoded_images: usize,
    poster_lru_entries: usize,
    poster_cache_size: usize,
    ffmpeg_child_peak: usize,
    duplicate_same_key_extractions: usize,
    poster_extraction_starts: usize,
    single_flight_followers: usize,
    off_page_decoded_images_after_settle: usize,
    off_page_model_images_after_settle: usize,
    stale_publications: usize,
    active_leases_after_close: usize,
    pws_growth_bytes: Option<i64>,
    pws_growth_measured_externally: bool,
    #[serde(skip)]
    page_samples_ms: Vec<f64>,
    #[serde(skip)]
    filter_samples_ms: Vec<f64>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Lifecycle {
    attachments_created: usize,
    attachments_dropped: usize,
    images_accepted: usize,
    images_released: usize,
    poster_handles_accepted: usize,
    poster_handles_released: usize,
    model_images_published: usize,
    model_images_replaced: usize,
    leases_acquired: usize,
    leases_released: usize,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Churn {
    local_cloud_page_switches: usize,
    poster_cancellations: usize,
    upload_progress_bursts: usize,
    executed_during_measured_window: bool,
}

struct HarnessState {
    catalog: SlintCatalogController,
    posters: PosterController<slint::Image>,
    metrics: Metrics,
    lifecycle: Lifecycle,
    churn: Churn,
    reveal: Reveal,
    retained_model_images: usize,
}

impl Lifecycle {
    fn accept_poster_handle(&mut self) -> HarnessResult<()> {
        self.poster_handles_accepted = checked_add(self.poster_handles_accepted, 1)?;
        self.images_accepted = checked_add(self.images_accepted, 1)?;
        Ok(())
    }

    fn release_poster_handles(&mut self, count: usize) -> HarnessResult<()> {
        self.poster_handles_released = checked_add(self.poster_handles_released, count)?;
        self.images_released = checked_add(self.images_released, count)?;
        Ok(())
    }

    fn replace_model_images(&mut self, retained: &mut usize, next: usize) -> HarnessResult<()> {
        self.model_images_published = checked_add(self.model_images_published, next)?;
        self.images_accepted = checked_add(self.images_accepted, next)?;
        self.model_images_replaced = checked_add(self.model_images_replaced, *retained)?;
        self.images_released = checked_add(self.images_released, *retained)?;
        *retained = next;
        Ok(())
    }
}

fn checked_add(current: usize, amount: usize) -> HarnessResult<usize> {
    current
        .checked_add(amount)
        .ok_or_else(|| "catalog lifecycle counter exhausted".into())
}

type EventLoopWork = Box<dyn FnMut() -> HarnessResult<bool>>;

#[derive(Clone, Copy)]
enum SetupStage {
    LocalPages,
    LocalFilters,
    Posters,
    CloudPages,
    Complete,
}

struct SetupProgress {
    stage: SetupStage,
    index: usize,
    pending: Option<Instant>,
}

impl Default for SetupProgress {
    fn default() -> Self {
        Self {
            stage: SetupStage::LocalPages,
            index: 0,
            pending: None,
        }
    }
}

fn make_setup_work(
    window: slint::Weak<CliplineSpike>,
    state: Rc<RefCell<HarnessState>>,
    poster_service: Arc<PosterService>,
    scenario: Scenario,
    fixture_root: PathBuf,
) -> EventLoopWork {
    let mut progress = SetupProgress::default();
    Box::new(move || {
        let window = window
            .upgrade()
            .ok_or("Slint catalog window disappeared during setup")?;
        let mut state = state.borrow_mut();
        loop {
            match progress.stage {
                SetupStage::LocalPages => {
                    if let Some(started) = progress.pending.take() {
                        state.metrics.page_samples_ms.push(elapsed_ms(started));
                        let HarnessState {
                            catalog,
                            metrics,
                            lifecycle,
                            retained_model_images,
                            ..
                        } = &mut *state;
                        catalog.dispatch(catalog.revision(), CatalogUiIntent::PreviousPage)?;
                        publish_and_record(
                            &window,
                            catalog,
                            &BTreeMap::new(),
                            metrics,
                            lifecycle,
                            retained_model_images,
                        )?;
                        window.window().request_redraw();
                        progress.index += 1;
                        return Ok(false);
                    }
                    if progress.index < PAGE_SAMPLES {
                        let started = Instant::now();
                        let HarnessState {
                            catalog,
                            metrics,
                            lifecycle,
                            retained_model_images,
                            ..
                        } = &mut *state;
                        catalog.dispatch(catalog.revision(), CatalogUiIntent::NextPage)?;
                        publish_and_record(
                            &window,
                            catalog,
                            &BTreeMap::new(),
                            metrics,
                            lifecycle,
                            retained_model_images,
                        )?;
                        window.window().request_redraw();
                        progress.pending = Some(started);
                        return Ok(false);
                    }
                    progress.stage = SetupStage::LocalFilters;
                    progress.index = 0;
                }
                SetupStage::LocalFilters => {
                    if let Some(started) = progress.pending.take() {
                        state.metrics.filter_samples_ms.push(elapsed_ms(started));
                        progress.index += 1;
                        return Ok(false);
                    }
                    if progress.index < PAGE_SAMPLES {
                        let started = Instant::now();
                        let HarnessState {
                            catalog,
                            metrics,
                            lifecycle,
                            retained_model_images,
                            ..
                        } = &mut *state;
                        catalog.dispatch(
                            catalog.revision(),
                            CatalogUiIntent::SetLocalGrouping(if progress.index % 2 == 0 {
                                LocalClipGrouping::None
                            } else {
                                LocalClipGrouping::Smart
                            }),
                        )?;
                        catalog.dispatch(
                            catalog.revision(),
                            CatalogUiIntent::SetLocalFilter(LocalClipFilter::All),
                        )?;
                        publish_and_record(
                            &window,
                            catalog,
                            &BTreeMap::new(),
                            metrics,
                            lifecycle,
                            retained_model_images,
                        )?;
                        window.window().request_redraw();
                        progress.pending = Some(started);
                        return Ok(false);
                    }
                    state.metrics.filter_group_p95_ms =
                        percentile_95(&state.metrics.filter_samples_ms);
                    progress.stage = SetupStage::Posters;
                    progress.index = 0;
                }
                SetupStage::Posters => {
                    let poster_started = Instant::now();
                    {
                        let HarnessState {
                            catalog,
                            posters,
                            metrics,
                            lifecycle,
                            retained_model_images,
                            ..
                        } = &mut *state;
                        settle_local_posters(
                            &window,
                            catalog,
                            posters,
                            &poster_service,
                            lifecycle,
                            metrics,
                            retained_model_images,
                        )?;
                        metrics.poster_settle_ms = elapsed_ms(poster_started);
                        metrics.ffmpeg_child_peak = poster_service.peak_active_extractions();
                        metrics.poster_lru_entries = posters.cache_len();
                        metrics.poster_cache_size = count_poster_files(&fixture_root)?;
                    }
                    if matches!(
                        scenario,
                        Scenario::CloudPages | Scenario::SelectionPageChurn
                    ) {
                        let update = state.posters.hide()?;
                        let HarnessState {
                            posters, lifecycle, ..
                        } = &mut *state;
                        apply_poster_teardown(posters, update, lifecycle)?;
                    }
                    match scenario {
                        Scenario::LocalCold | Scenario::LocalWarm => {
                            progress.stage = SetupStage::Complete;
                        }
                        Scenario::CloudPages | Scenario::SelectionPageChurn => {
                            let HarnessState {
                                catalog,
                                metrics,
                                lifecycle,
                                retained_model_images,
                                ..
                            } = &mut *state;
                            install_synthetic_cloud(
                                &window,
                                catalog,
                                metrics,
                                lifecycle,
                                retained_model_images,
                            )?;
                            progress.stage = if scenario == Scenario::CloudPages {
                                SetupStage::CloudPages
                            } else {
                                SetupStage::Complete
                            };
                        }
                        Scenario::RevealClose100 => {
                            state.reveal.cloud_media_cycles_pending = true;
                            progress.stage = SetupStage::Complete;
                        }
                    }
                }
                SetupStage::CloudPages => {
                    if let Some(started) = progress.pending.take() {
                        state.metrics.page_samples_ms.push(elapsed_ms(started));
                        let HarnessState {
                            catalog,
                            metrics,
                            lifecycle,
                            retained_model_images,
                            ..
                        } = &mut *state;
                        let effects =
                            catalog.dispatch(catalog.revision(), CatalogUiIntent::PreviousPage)?;
                        accept_synthetic_cloud_refresh(catalog, effects)?;
                        let images = synthetic_images(
                            catalog
                                .projection()
                                .rows
                                .iter()
                                .map(|row| row.identity.clone()),
                        );
                        publish_and_record(
                            &window,
                            catalog,
                            &images,
                            metrics,
                            lifecycle,
                            retained_model_images,
                        )?;
                        window.window().request_redraw();
                        progress.index += 1;
                        return Ok(false);
                    }
                    if progress.index < PAGE_SAMPLES {
                        let started = Instant::now();
                        let HarnessState {
                            catalog,
                            metrics,
                            lifecycle,
                            retained_model_images,
                            ..
                        } = &mut *state;
                        let effects =
                            catalog.dispatch(catalog.revision(), CatalogUiIntent::NextPage)?;
                        accept_synthetic_cloud_refresh(catalog, effects)?;
                        let images = synthetic_images(
                            catalog
                                .projection()
                                .rows
                                .iter()
                                .map(|row| row.identity.clone()),
                        );
                        publish_and_record(
                            &window,
                            catalog,
                            &images,
                            metrics,
                            lifecycle,
                            retained_model_images,
                        )?;
                        window.window().request_redraw();
                        progress.pending = Some(started);
                        return Ok(false);
                    }
                    progress.stage = SetupStage::Complete;
                }
                SetupStage::Complete => {
                    state.metrics.page_change_p95_ms =
                        percentile_95(&state.metrics.page_samples_ms);
                    return Ok(true);
                }
            }
        }
    })
}

fn make_selection_churn_work(
    window: slint::Weak<CliplineSpike>,
    state: Rc<RefCell<HarnessState>>,
    fixture_paths: Vec<PathBuf>,
) -> EventLoopWork {
    let mut cycle = 0;
    let mut cancellation_controller = PosterController::<()>::new();
    Box::new(move || {
        let window = window
            .upgrade()
            .ok_or("Slint catalog window disappeared during measured churn")?;
        let mut state = state.borrow_mut();
        let HarnessState {
            catalog,
            metrics,
            lifecycle,
            churn,
            retained_model_images,
            ..
        } = &mut *state;
        exercise_selection_page_churn_cycle(
            &window,
            catalog,
            &fixture_paths,
            cycle,
            &mut cancellation_controller,
            churn,
            metrics,
            lifecycle,
            retained_model_images,
        )?;
        cycle += 1;
        let complete = cycle == CHURN_CYCLES;
        if complete {
            churn.executed_during_measured_window = true;
        }
        Ok(complete)
    })
}

struct RevealWindow {
    attachment: AttachmentToken,
    desktop_attachment: DesktopAttachment,
    window: CliplineSpike,
}

fn make_reveal_close_work(state: Rc<RefCell<HarnessState>>) -> HarnessResult<EventLoopWork> {
    let (mut shell, initial) = ShellLifecycle::for_launch(LaunchMode::Autostart)?;
    if initial != LifecycleAction::KeepTrayOnly {
        return Err("autostart reveal driver did not begin tray-only".into());
    }
    let desktop = SlintDesktopAdapter::start_detached().map_err(std::io::Error::other)?;
    let mut current: Option<RevealWindow> = None;
    let mut cycles = 0_usize;
    Ok(Box::new(move || {
        if let Some(revealed) = current.take() {
            if shell.close_requested(revealed.attachment)?
                != (LifecycleAction::DropToTray {
                    attachment: revealed.attachment,
                })
            {
                return Err("shipping lifecycle did not authorize the measured window drop".into());
            }
            revealed.window.hide()?;
            desktop.detach(revealed.desktop_attachment)?;
            drop(revealed.window);
            shell.window_dropped(revealed.attachment)?;
            cycles = checked_add(cycles, 1)?;
            let mut state = state.borrow_mut();
            state.lifecycle.attachments_dropped =
                checked_add(state.lifecycle.attachments_dropped, 1)?;
            if cycles == CHURN_CYCLES {
                let snapshot = shell.snapshot();
                if snapshot.window_active
                    || snapshot.counters.windows_created != CHURN_CYCLES as u64
                    || snapshot.counters.windows_dropped != CHURN_CYCLES as u64
                    || snapshot.counters.open_requests != CHURN_CYCLES as u64
                    || snapshot.counters.close_requests != CHURN_CYCLES as u64
                {
                    return Err(
                        "shipping lifecycle counters do not prove 100 exact window cycles".into(),
                    );
                }
                state.reveal.window_reveal_close_cycles = CHURN_CYCLES;
                state.reveal.window_reveal_close_cycles_pending = false;
                state.reveal.window_cycles_executed_during_measured_window = true;
                return Ok(true);
            }
            return Ok(false);
        }

        let LifecycleAction::CreateWindow { attachment } =
            shell.handle_command(ShellCommand::Open)?
        else {
            return Err("shipping lifecycle did not authorize the measured window create".into());
        };
        let window = create_window()?;
        let desktop_attachment = desktop.attach(window.as_weak())?;
        if let Err(error) = window.show() {
            let _ = desktop.detach(desktop_attachment);
            let _ = shell.window_create_failed(attachment);
            return Err(error.into());
        }
        window.window().request_redraw();
        shell.window_created(attachment)?;
        let mut state = state.borrow_mut();
        state.lifecycle.attachments_created = checked_add(state.lifecycle.attachments_created, 1)?;
        drop(state);
        current = Some(RevealWindow {
            attachment,
            desktop_attachment,
            window,
        });
        Ok(false)
    }))
}

fn apply_poster_teardown(
    controller: &mut PosterController<slint::Image>,
    update: clipline_library::PosterControllerUpdate<slint::Image>,
    lifecycle: &mut Lifecycle,
) -> HarnessResult<()> {
    lifecycle.release_poster_handles(update.released.len())?;
    let mut canceled = VecDeque::from(update.canceled);
    if !update.queued.is_empty() {
        return Err("poster teardown unexpectedly queued replacement work".into());
    }
    while let Some(request) = canceled.pop_front() {
        let acknowledged = controller.acknowledge_canceled(&request)?;
        lifecycle.release_poster_handles(acknowledged.released.len())?;
        if !acknowledged.queued.is_empty() {
            return Err("poster cancellation acknowledgement queued work during teardown".into());
        }
        canceled.extend(acknowledged.canceled);
    }
    Ok(())
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Reveal {
    window_reveal_close_cycles: usize,
    window_reveal_close_cycles_pending: bool,
    window_cycles_executed_during_measured_window: bool,
    cloud_media_cycles: usize,
    cloud_media_cycles_pending: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Safety {
    production_credentials_loaded: bool,
    cloud_network_requests: usize,
}

#[derive(Clone, Copy)]
struct HarnessDays;

impl LocalDayResolver for HarnessDays {
    fn today_start_unix(&self) -> u64 {
        0
    }

    fn resolve_day(&self, timestamp: u64) -> LocalDay {
        LocalDay {
            key: format!("day-{}", timestamp / 86_400),
            label: "Fixture day".to_owned(),
        }
    }
}

fn accept_local_refresh(
    catalog: &mut SlintCatalogController,
    handler: &LocalCatalogEffectHandler,
    effects: Vec<CatalogEffect>,
) -> HarnessResult<()> {
    let refresh = effects
        .into_iter()
        .find(|effect| matches!(effect, CatalogEffect::RefreshLocal { .. }))
        .ok_or("catalog attach did not request a Local refresh")?;
    let completion = handler
        .execute(refresh)?
        .ok_or("Local refresh did not return a completion")?;
    catalog.accept(completion.result)?;
    Ok(())
}

fn publish_and_record(
    window: &CliplineSpike,
    catalog: &SlintCatalogController,
    images: &BTreeMap<CatalogItemIdentity, slint::Image>,
    metrics: &mut Metrics,
    lifecycle: &mut Lifecycle,
    retained_model_images: &mut usize,
) -> HarnessResult<()> {
    let projection = catalog.projection();
    publish_projection(window, &projection, |identity| {
        images.get(identity).cloned()
    })?;
    let actual_rows = window.get_library_items().row_count();
    if actual_rows != projection.rows.len() {
        return Err("Slint row model did not atomically match the Rust projection".into());
    }
    metrics.retained_rows = metrics.retained_rows.max(actual_rows);
    let projected = projection
        .rows
        .iter()
        .map(|row| &row.identity)
        .collect::<BTreeSet<_>>();
    metrics.off_page_model_images_after_settle = metrics.off_page_model_images_after_settle.max(
        images
            .keys()
            .filter(|identity| !projected.contains(identity))
            .count(),
    );
    metrics.retained_decoded_images = metrics
        .retained_decoded_images
        .max(images.len().min(MAX_DECODED_PAGE_IMAGES));
    lifecycle.replace_model_images(retained_model_images, images.len())?;
    Ok(())
}

fn settle_local_posters(
    window: &CliplineSpike,
    catalog: &SlintCatalogController,
    controller: &mut PosterController<slint::Image>,
    service: &Arc<PosterService>,
    lifecycle: &mut Lifecycle,
    metrics: &mut Metrics,
    retained_model_images: &mut usize,
) -> HarnessResult<()> {
    let page = catalog.poster_page()?;
    let page_identities = page
        .items
        .iter()
        .map(|item| item.identity.clone())
        .collect::<BTreeSet<_>>();
    let token = current_window_token(catalog)?;
    lifecycle.release_poster_handles(controller.replace_page(token, page.items)?.released.len())?;
    let queued = controller
        .set_viewport(0, MAX_DECODED_PAGE_IMAGES, 0)?
        .queued;
    let mut queue = VecDeque::from(queued);
    let mut extraction_keys = BTreeSet::new();
    let mut images = BTreeMap::new();
    while let Some(request) = queue.pop_front() {
        match &request.kind {
            PosterWorkKind::Extract => {
                if !extraction_keys.insert(request.item.identity.clone()) {
                    metrics.duplicate_same_key_extractions += 1;
                }
                let poster = if service.extraction_starts() == 0 {
                    ensure_same_key_single_flight(service, &request.item)?
                } else {
                    service
                        .ensure_poster(&request.item.native_path, request.item.seek_seconds)
                        .map_err(|error| {
                            format!("extract {}: {error}", request.item.native_path.display())
                        })?
                };
                let expected = clipline_library::poster_path(&request.item.native_path);
                if poster != expected {
                    return Err(format!(
                        "poster service returned a non-owned path: expected {}, got {}",
                        expected.display(),
                        poster.display()
                    )
                    .into());
                }
                let update = controller.accept_extracted(&request, PosterCompletion::Ready(poster));
                queue.extend(update.queued);
            }
            PosterWorkKind::Decode { .. } => {
                let identity = request.item.identity.clone();
                let decoded = decode_poster_file(request)?;
                let update = publish_decoded_poster(controller, decoded)?;
                lifecycle.accept_poster_handle()?;
                lifecycle.release_poster_handles(update.released.len())?;
                if let Some(image) = controller.retained_image(&identity).cloned() {
                    images.insert(CatalogItemIdentity::Local { path: identity }, image);
                }
                queue.extend(update.queued);
            }
        }
    }
    if controller.queued_work_count() != 0 {
        return Err("poster ownership did not settle".into());
    }
    metrics.poster_extraction_starts = usize::try_from(service.extraction_starts())?;
    metrics.single_flight_followers = usize::try_from(service.single_flight_followers())?;
    metrics.duplicate_same_key_extractions = metrics
        .poster_extraction_starts
        .saturating_sub(extraction_keys.len());
    metrics.retained_decoded_images = metrics
        .retained_decoded_images
        .max(controller.retained_image_count());
    let retained_on_page = page_identities
        .iter()
        .filter(|identity| controller.retained_image(identity).is_some())
        .count();
    metrics.off_page_decoded_images_after_settle = controller
        .retained_image_count()
        .saturating_sub(retained_on_page);
    publish_and_record(
        window,
        catalog,
        &images,
        metrics,
        lifecycle,
        retained_model_images,
    )?;
    Ok(())
}

fn ensure_same_key_single_flight(
    service: &Arc<PosterService>,
    item: &PosterPageItem,
) -> HarnessResult<PathBuf> {
    let start = Arc::new(Barrier::new(3));
    let mut callers = Vec::with_capacity(2);
    for _ in 0..2 {
        let service = Arc::clone(service);
        let path = item.native_path.clone();
        let seek_seconds = item.seek_seconds;
        let start = Arc::clone(&start);
        callers.push(std::thread::spawn(move || {
            start.wait();
            service.ensure_poster(&path, seek_seconds)
        }));
    }
    start.wait();
    let first = callers
        .remove(0)
        .join()
        .map_err(|_| "same-key poster leader panicked")?
        .map_err(|error| format!("same-key poster leader failed: {error}"))?;
    let second = callers
        .remove(0)
        .join()
        .map_err(|_| "same-key poster follower panicked")?
        .map_err(|error| format!("same-key poster follower failed: {error}"))?;
    if first != second || service.extraction_starts() != 1 || service.single_flight_followers() < 1
    {
        return Err("same-key poster work did not coalesce into one extraction".into());
    }
    Ok(first)
}

fn current_window_token(catalog: &SlintCatalogController) -> HarnessResult<WindowWorkToken> {
    let revision = CatalogRevision::new(catalog.revision());
    Ok(WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(1),
        foreground: ForegroundGeneration::new(1),
        request: RequestGeneration::new(revision.get().max(1)),
    })
}

fn cloud_owner() -> CloudCatalogOwner {
    CloudCatalogOwner {
        account_key: CloudAccountKey::new("synthetic-account").expect("constant account key"),
        account_generation: CloudAccountGeneration::new(1),
    }
}

fn synthetic_cloud_items(page: u32) -> Vec<CloudLibraryItem> {
    (0..MAX_CATALOG_PAGE_ROWS)
        .map(|index| {
            let id = (page as usize - 1) * MAX_CATALOG_PAGE_ROWS + index;
            CloudLibraryItem {
                remote_clip_id: format!("synthetic-{id:05}"),
                local_clip_id: None,
                path: String::new(),
                title: format!("Synthetic Cloud {id:05}"),
                remote_url: format!("https://invalid.example/clips/{id:05}"),
                visibility: "private".to_owned(),
                upload_status: "ready".to_owned(),
                updated_at_unix: id as u64,
                uploaded_at_unix: Some(id as u64),
                duration_ms: Some(5_000),
                file_size_bytes: Some(1_024),
                source_type: Some("replay".to_owned()),
            }
        })
        .collect()
}

fn install_synthetic_cloud(
    window: &CliplineSpike,
    catalog: &mut SlintCatalogController,
    metrics: &mut Metrics,
    lifecycle: &mut Lifecycle,
    retained_model_images: &mut usize,
) -> HarnessResult<()> {
    catalog.set_cloud_context(Some(cloud_owner()), Default::default())?;
    let effects = catalog.dispatch(
        catalog.revision(),
        CatalogUiIntent::SetSource(CatalogSource::Cloud),
    )?;
    accept_synthetic_cloud_refresh(catalog, effects)?;
    let images = synthetic_images(
        catalog
            .projection()
            .rows
            .iter()
            .map(|row| row.identity.clone()),
    );
    publish_and_record(
        window,
        catalog,
        &images,
        metrics,
        lifecycle,
        retained_model_images,
    )
}

fn accept_synthetic_cloud_refresh(
    catalog: &mut SlintCatalogController,
    effects: Vec<CatalogEffect>,
) -> HarnessResult<()> {
    let (token, revision, page) = effects
        .into_iter()
        .find_map(|effect| match effect {
            CatalogEffect::RefreshCloud {
                token,
                revision,
                page,
                ..
            } => Some((token, revision, page)),
            _ => None,
        })
        .ok_or("Cloud action did not request a synthetic page")?;
    let result = CloudListPageCompletion::page(
        token,
        revision,
        page,
        synthetic_cloud_items(page.get()),
        Vec::new(),
    )?;
    catalog.accept(CatalogResult::CloudPage(result))?;
    Ok(())
}

fn synthetic_images(
    identities: impl Iterator<Item = CatalogItemIdentity>,
) -> BTreeMap<CatalogItemIdentity, slint::Image> {
    identities
        .take(MAX_DECODED_PAGE_IMAGES)
        .map(|identity| {
            let pixels = SharedPixelBuffer::<Rgb8Pixel>::new(2, 2);
            (identity, slint::Image::from_rgb8(pixels))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn exercise_selection_page_churn_cycle(
    window: &CliplineSpike,
    catalog: &mut SlintCatalogController,
    fixture_paths: &[PathBuf],
    cycle: usize,
    cancellation_controller: &mut PosterController<()>,
    churn: &mut Churn,
    metrics: &mut Metrics,
    lifecycle: &mut Lifecycle,
    retained_model_images: &mut usize,
) -> HarnessResult<()> {
    if cycle >= CHURN_CYCLES || fixture_paths.is_empty() {
        return Err("measured churn cycle is outside its bounded fixture".into());
    }
    let source = if cycle.is_multiple_of(2) {
        CatalogSource::Local
    } else {
        CatalogSource::Cloud
    };
    catalog.dispatch(catalog.revision(), CatalogUiIntent::SetSource(source))?;
    if source == CatalogSource::Local && cycle == 0 {
        catalog.dispatch(catalog.revision(), CatalogUiIntent::EnterSelection)?;
        catalog.dispatch(catalog.revision(), CatalogUiIntent::SelectVisiblePage)?;
    }
    let images = if source == CatalogSource::Cloud {
        synthetic_images(
            catalog
                .projection()
                .rows
                .iter()
                .map(|row| row.identity.clone()),
        )
    } else {
        BTreeMap::new()
    };
    publish_and_record(
        window,
        catalog,
        &images,
        metrics,
        lifecycle,
        retained_model_images,
    )?;
    window.window().request_redraw();
    churn.local_cloud_page_switches = checked_add(churn.local_cloud_page_switches, 1)?;

    let path = fixture_paths[cycle % fixture_paths.len()].clone();
    let token = WindowWorkToken {
        attachment: WindowAttachmentGeneration::new(10),
        foreground: ForegroundGeneration::new(10),
        request: RequestGeneration::new(cycle as u64 + 1),
    };
    cancellation_controller.replace_page(token, vec![PosterPageItem::new(path, 1.0)?])?;
    let queued = cancellation_controller.set_viewport(0, 1, 0)?.queued;
    let canceled = cancellation_controller
        .replace_page(token, Vec::new())?
        .canceled;
    if queued.len() != 1 || canceled.len() != 1 {
        return Err("poster cancellation churn did not produce one exact cancellation".into());
    }
    let acknowledged = cancellation_controller.acknowledge_canceled(&canceled[0])?;
    if !acknowledged.queued.is_empty()
        || !acknowledged.canceled.is_empty()
        || !acknowledged.released.is_empty()
        || cancellation_controller.ownership_count() != 0
    {
        return Err("poster cancellation acknowledgement did not release exact ownership".into());
    }
    churn.poster_cancellations = checked_add(churn.poster_cancellations, 1)?;

    let fixture = &fixture_paths[cycle % fixture_paths.len()];
    let path = fixture.display().to_string();
    let token = DurableUploadToken {
        account_key: cloud_owner().account_key,
        account_generation: CloudAccountGeneration::new(1),
        upload_generation: UploadGeneration::new(cycle as u64 + 1),
        local_clip_id: LocalClipId::new(format!("fixture-{cycle:05}"))?,
        source_path: clipline_library::ClipPathIdentity::from_text(&path)
            .ok_or("fixture path has no catalog identity")?,
    };
    catalog.accept(CatalogResult::UploadByteProgress {
        token,
        progress: UploadSummary {
            local_clip_id: format!("fixture-{cycle:05}"),
            path,
            upload_status: "uploading".to_owned(),
            received_size_bytes: cycle as u64 + 1,
            file_size_bytes: CHURN_CYCLES as u64,
            remote_clip_id: None,
            remote_url: None,
            error: None,
        },
    })?;
    churn.upload_progress_bursts = checked_add(churn.upload_progress_bursts, 1)?;
    Ok(())
}

fn validate_hard_link_fixture(
    options: &Options,
    source_sha256: &str,
) -> HarnessResult<Vec<PathBuf>> {
    let expected_seed_count = options.clip_count.div_ceil(MAX_HARD_LINKS_PER_SEED);
    let mut seed_identities = Vec::with_capacity(expected_seed_count);
    for index in 0..expected_seed_count {
        let seed = options
            .fixture_seed_root
            .join(format!("seed-{index:02}.mp4"));
        let file = clipline_shell::open_regular_file_nofollow(&seed)
            .map_err(|error| format!("open expected fixture seed {}: {error}", seed.display()))?;
        let identity = clipline_shell::opened_file_identity(&file)?;
        if sha256_file(&seed)? != source_sha256 {
            return Err(format!(
                "fixture seed hash differs from the source oracle: {}",
                seed.display()
            )
            .into());
        }
        seed_identities.push(identity);
    }
    let actual_seed_files = std::fs::read_dir(&options.fixture_seed_root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .count();
    if actual_seed_files != expected_seed_count {
        return Err(format!(
            "fixture seed root has {actual_seed_files} files; expected exactly {expected_seed_count}"
        )
        .into());
    }
    let mut seed_links = vec![0_usize; expected_seed_count];
    let mut paths = Vec::with_capacity(options.clip_count);
    for index in 0..options.clip_count {
        let path = options.fixture_root.join(format!("clip-{index:05}.mp4"));
        let file = clipline_shell::open_regular_file_nofollow(&path)
            .map_err(|error| format!("open expected hard-link {}: {error}", path.display()))?;
        let identity = clipline_shell::opened_file_identity(&file)?;
        let Some(seed_index) = seed_identities.iter().position(|seed| *seed == identity) else {
            return Err(format!(
                "fixture clip is not linked to a hash-verified seed: {}",
                path.display()
            )
            .into());
        };
        seed_links[seed_index] = checked_add(seed_links[seed_index], 1)?;
        if seed_links[seed_index] > MAX_HARD_LINKS_PER_SEED {
            return Err("fixture seed exceeds the 500-link evidence bound".into());
        }
        paths.push(path);
    }
    if seed_links.contains(&0) {
        return Err("fixture contains an unused hash-verified seed".into());
    }
    let mp4_count = std::fs::read_dir(&options.fixture_root)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
        })
        .count();
    if mp4_count != options.clip_count {
        return Err(format!(
            "fixture root has {mp4_count} MP4 files; expected exactly {}",
            options.clip_count
        )
        .into());
    }
    Ok(paths)
}

fn validate_warm_cache(paths: &[PathBuf]) -> HarnessResult<()> {
    let expected = paths.len().min(MAX_DECODED_PAGE_IMAGES);
    let warm = paths
        .iter()
        .take(expected)
        .filter(|path| clipline_library::cached_poster(path).is_some())
        .count();
    if warm != expected {
        return Err(format!(
            "local-warm requires {expected} fresh owned poster entries; found {warm}"
        )
        .into());
    }
    Ok(())
}

fn validate_internal_gates(
    metrics: &mut Metrics,
    lifecycle: &Lifecycle,
    churn: &Churn,
    reveal: &Reveal,
    scenario: Scenario,
) -> HarnessResult<()> {
    metrics.pws_growth_bytes = None;
    metrics.pws_growth_measured_externally = true;
    if lifecycle.poster_handles_accepted != MAX_DECODED_PAGE_IMAGES
        || metrics.retained_decoded_images != MAX_DECODED_PAGE_IMAGES
        || metrics.poster_lru_entries != MAX_DECODED_PAGE_IMAGES
    {
        return Err("the initial Local poster slice did not decode exactly 32 owned images".into());
    }
    if metrics.retained_rows > MAX_CATALOG_PAGE_ROWS
        || metrics.retained_decoded_images > MAX_DECODED_PAGE_IMAGES
        || metrics.poster_lru_entries > MAX_POSTER_RESULT_ENTRIES
        || metrics.ffmpeg_child_peak > 2
        || metrics.duplicate_same_key_extractions != 0
        || metrics.off_page_decoded_images_after_settle != 0
        || metrics.off_page_model_images_after_settle != 0
        || metrics.stale_publications != 0
        || metrics.active_leases_after_close != 0
    {
        return Err("one or more bounded catalog gates failed".into());
    }
    if scenario == Scenario::LocalWarm {
        if metrics.poster_extraction_starts != 0 || metrics.single_flight_followers != 0 {
            return Err("warm poster cache unexpectedly entered extraction single-flight".into());
        }
    } else if metrics.poster_extraction_starts != MAX_DECODED_PAGE_IMAGES
        || metrics.single_flight_followers < 1
    {
        return Err("cold poster gate did not prove one same-key single-flight follower".into());
    }
    if lifecycle.attachments_created != lifecycle.attachments_dropped
        || lifecycle.images_accepted != lifecycle.images_released
        || lifecycle.poster_handles_accepted != lifecycle.poster_handles_released
        || lifecycle.model_images_published != lifecycle.model_images_replaced
        || lifecycle.leases_acquired != lifecycle.leases_released
    {
        return Err("catalog lifecycle counters are not balanced".into());
    }
    if scenario == Scenario::SelectionPageChurn
        && (churn.local_cloud_page_switches != CHURN_CYCLES
            || churn.poster_cancellations != CHURN_CYCLES
            || churn.upload_progress_bursts != CHURN_CYCLES
            || !churn.executed_during_measured_window)
    {
        return Err("selection/page churn did not complete exactly 100 cycles".into());
    }
    if scenario == Scenario::RevealClose100
        && (lifecycle.attachments_created != CHURN_CYCLES + 1
            || lifecycle.attachments_dropped != CHURN_CYCLES + 1
            || reveal.window_reveal_close_cycles != CHURN_CYCLES
            || reveal.window_reveal_close_cycles_pending
            || !reveal.window_cycles_executed_during_measured_window)
    {
        return Err("reveal/close lifecycle did not complete inside the measured window".into());
    }
    Ok(())
}

struct EventLoopReport {
    first_usable_page_ms: f64,
}

fn run_measured_event_loop(
    stop_path: &Path,
    exercise_path: &Path,
    markers: &MarkerSink,
    process_started_at: SystemTime,
    mut setup: EventLoopWork,
    mut exercise: Option<EventLoopWork>,
) -> HarnessResult<EventLoopReport> {
    let stop_path = stop_path.to_path_buf();
    let exercise_path = exercise_path.to_path_buf();
    let done = Arc::new(AtomicBool::new(false));
    let stop_observed = Arc::new(AtomicBool::new(false));
    let initial_turn = Arc::new(AtomicBool::new(false));
    let setup_done = Arc::new(AtomicBool::new(false));
    let exercise_started = Arc::new(AtomicBool::new(false));
    let exercise_done = Arc::new(AtomicBool::new(false));
    let exercise_required = exercise.is_some();
    let first_page_ms = Arc::new(Mutex::new(None));
    let marker_error = Arc::new(Mutex::new(None::<String>));

    let timer_turn = Arc::clone(&initial_turn);
    let timer_first_page = Arc::clone(&first_page_ms);
    let timer_error = Arc::clone(&marker_error);
    let timer_markers = markers.clone();
    let initial_timer = slint::Timer::default();
    initial_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(50),
        move || {
            let first_ms = SystemTime::now()
                .duration_since(process_started_at)
                .unwrap_or_default()
                .as_secs_f64()
                * 1_000.0;
            let result = timer_markers
                .write(
                    "pageSettled",
                    "bounded catalog page shown after a Slint event-loop interval",
                )
                .and_then(|()| {
                    timer_markers.write("ready", "first native Slint catalog page usable")
                });
            if let Err(error) = result {
                if let Ok(mut slot) = timer_error.lock() {
                    *slot = Some(error.to_string());
                }
                let _ = slint::quit_event_loop();
                return;
            }
            if let Ok(mut slot) = timer_first_page.lock() {
                *slot = Some(first_ms);
            }
            timer_turn.store(true, Ordering::Release);
        },
    );

    let setup_ready = Arc::clone(&initial_turn);
    let setup_finished = Arc::clone(&setup_done);
    let setup_error = Arc::clone(&marker_error);
    let setup_markers = markers.clone();
    let setup_timer = slint::Timer::default();
    setup_timer.start(slint::TimerMode::Repeated, SETUP_TICK, move || {
        if !setup_ready.load(Ordering::Acquire) || setup_finished.load(Ordering::Acquire) {
            return;
        }
        match setup() {
            Ok(true) => {
                if let Err(error) = setup_markers.write(
                    "postersSettled",
                    "bounded poster and interaction setup settled",
                ) {
                    if let Ok(mut slot) = setup_error.lock() {
                        *slot = Some(error.to_string());
                    }
                    let _ = slint::quit_event_loop();
                    return;
                }
                setup_finished.store(true, Ordering::Release);
            }
            Ok(false) => {}
            Err(error) => {
                if let Ok(mut slot) = setup_error.lock() {
                    *slot = Some(error.to_string());
                }
                let _ = slint::quit_event_loop();
            }
        }
    });

    let exercise_setup_done = Arc::clone(&setup_done);
    let exercise_has_started = Arc::clone(&exercise_started);
    let exercise_finished = Arc::clone(&exercise_done);
    let exercise_error = Arc::clone(&marker_error);
    let exercise_markers = markers.clone();
    let exercise_timer = slint::Timer::default();
    if exercise_required {
        exercise_timer.start(slint::TimerMode::Repeated, SETUP_TICK, move || {
            if !exercise_setup_done.load(Ordering::Acquire)
                || exercise_finished.load(Ordering::Acquire)
                || !exercise_path.exists()
            {
                return;
            }
            if !exercise_has_started.swap(true, Ordering::AcqRel)
                && clipline_shell::open_regular_file_nofollow(&exercise_path).is_err()
            {
                if let Ok(mut slot) = exercise_error.lock() {
                    *slot = Some("exercise signal is not an owned regular file".to_owned());
                }
                let _ = slint::quit_event_loop();
                return;
            }
            let Some(work) = exercise.as_mut() else {
                return;
            };
            match work() {
                Ok(true) => {
                    if let Err(error) = exercise_markers.write(
                        "exerciseSettled",
                        "100-cycle measured catalog exercise settled",
                    ) {
                        if let Ok(mut slot) = exercise_error.lock() {
                            *slot = Some(error.to_string());
                        }
                        let _ = slint::quit_event_loop();
                        return;
                    }
                    exercise_finished.store(true, Ordering::Release);
                }
                Ok(false) => {}
                Err(error) => {
                    if let Ok(mut slot) = exercise_error.lock() {
                        *slot = Some(error.to_string());
                    }
                    let _ = slint::quit_event_loop();
                }
            }
        });
    }

    let watcher_done = Arc::clone(&done);
    let watcher_observed = Arc::clone(&stop_observed);
    let watcher = std::thread::Builder::new()
        .name("clipline-catalog-stop".to_owned())
        .spawn(move || {
            while !watcher_done.load(Ordering::Acquire) {
                if stop_path.exists() {
                    let valid = clipline_shell::open_regular_file_nofollow(&stop_path).is_ok();
                    if valid {
                        watcher_observed.store(true, Ordering::Release);
                    }
                    let _ = slint::quit_event_loop();
                    return valid;
                }
                std::thread::sleep(STOP_POLL);
            }
            false
        })?;
    let loop_result = slint::run_event_loop_until_quit();
    done.store(true, Ordering::Release);
    let valid_stop = watcher
        .join()
        .map_err(|_| "catalog stop watcher panicked")?;
    loop_result?;
    if let Some(error) = marker_error
        .lock()
        .map_err(|_| "catalog marker error lock poisoned")?
        .take()
    {
        return Err(error.into());
    }
    if !valid_stop || !stop_observed.load(Ordering::Acquire) {
        return Err("Slint event loop exited before an owned regular stop file appeared".into());
    }
    if !initial_turn.load(Ordering::Acquire) {
        return Err("owned stop arrived before the initial Slint page settled".into());
    }
    if !setup_done.load(Ordering::Acquire) {
        return Err("owned stop arrived before poster and interaction setup settled".into());
    }
    if exercise_required && !exercise_done.load(Ordering::Acquire) {
        return Err("owned stop arrived before measured 100-cycle exercise settled".into());
    }
    let first_usable_page_ms = first_page_ms
        .lock()
        .map_err(|_| "first-page metric lock poisoned")?
        .ok_or("initial Slint page did not publish a latency sample")?;
    Ok(EventLoopReport {
        first_usable_page_ms,
    })
}

fn write_atomic_json(path: &Path, value: &Telemetry) -> HarnessResult<()> {
    if path.exists() {
        return Err("telemetry target already exists".into());
    }
    let parent = path.parent().ok_or("telemetry path has no parent")?;
    let target_name = path.file_name().ok_or("telemetry path has no file name")?;
    let temp_name = OsString::from(format!(
        ".{}.tmp.{}",
        target_name.to_string_lossy(),
        std::process::id()
    ));
    let temp = parent.join(&temp_name);
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() > TELEMETRY_LIMIT {
        return Err("catalog telemetry exceeds the 1 MiB bound".into());
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    let identity = clipline_shell::opened_file_identity(&file)?;
    let mut cleanup = TempCleanup::new(temp.clone(), identity);
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    clipline_shell::rename_file_noreplace_if_identity(&temp, path, identity)
        .map_err(|error| format!("publish catalog telemetry: {error}"))?;
    cleanup.disarm();
    Ok(())
}

struct TempCleanup {
    owned: Option<(PathBuf, clipline_shell::FileIdentity)>,
}

impl TempCleanup {
    fn new(path: PathBuf, identity: clipline_shell::FileIdentity) -> Self {
        Self {
            owned: Some((path, identity)),
        }
    }

    fn disarm(&mut self) {
        self.owned = None;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some((path, identity)) = self.owned.take() {
            let _ = clipline_shell::remove_file_if_identity(&path, identity);
        }
    }
}

fn sha256_file(path: &Path) -> HarnessResult<String> {
    let mut file = clipline_shell::open_regular_file_nofollow(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn count_poster_files(root: &Path) -> HarnessResult<usize> {
    Ok(std::fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".poster.jpg"))
        .count())
}

fn percentile_95(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (sorted.len() * 95).div_ceil(100).saturating_sub(1);
    sorted[rank]
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn unix_millis(time: SystemTime) -> u128 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(any(windows, test))]
const WINDOWS_UNIX_EPOCH_OFFSET_100NS: u64 = 116_444_736_000_000_000;

#[cfg(any(windows, test))]
fn system_time_from_windows_filetime(filetime_100ns: u64) -> Option<SystemTime> {
    let unix_100ns = filetime_100ns.checked_sub(WINDOWS_UNIX_EPOCH_OFFSET_100NS)?;
    let nanos = unix_100ns.checked_mul(100)?;
    Some(UNIX_EPOCH + Duration::from_nanos(nanos))
}

#[cfg(windows)]
fn current_process_started_at(fallback: SystemTime) -> SystemTime {
    clipline_shell::windows::process::process_identity(std::process::id())
        .ok()
        .and_then(|identity| system_time_from_windows_filetime(identity.creation_time()))
        .unwrap_or(fallback)
}

#[cfg(not(windows))]
fn current_process_started_at(fallback: SystemTime) -> SystemTime {
    fallback
}

fn utc_timestamp(now: SystemTime) -> String {
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn bounded_detail(value: &str) -> String {
    let mut end = value.len().min(4_096);
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_does_not_require_a_fixture() {
        let parsed =
            Options::parse([OsString::from("catalog_harness"), OsString::from("--help")]).unwrap();
        assert!(matches!(parsed, ParseOutcome::Help));
    }

    #[test]
    fn scenarios_and_counts_are_closed_sets() {
        for scenario in [
            "local-cold",
            "local-warm",
            "cloud-pages",
            "selection-page-churn",
            "reveal-close-100",
        ] {
            assert_eq!(Scenario::parse(scenario).unwrap().as_str(), scenario);
        }
        assert!(Scenario::parse("network-cloud").is_err());
        assert_eq!(ALLOWED_COUNTS, [50, 500, 2_000]);
    }

    #[test]
    fn telemetry_publication_is_create_new_bounded_and_atomic() {
        let root = std::env::temp_dir().join(format!(
            "clipline-catalog-harness-test-{}-{}",
            std::process::id(),
            unix_millis(SystemTime::now())
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("telemetry.json");
        let telemetry = Telemetry {
            schema_version: 1,
            status: "completed",
            publication: "create-new-atomic-rename",
            scenario: "local-warm",
            clip_count: 50,
            source_fixture: SourceFixture {
                path: "fixture.mp4".to_owned(),
                sha256: "0".repeat(64),
            },
            provenance: Provenance {
                build_sha: "test".to_owned(),
                renderer: "winit-software".to_owned(),
                adapter: "test".to_owned(),
                scale: 1.0,
                process_id: std::process::id(),
                process_name: "catalog_harness",
                process_start_unix_ms: 1,
                machine: "test".to_owned(),
                fixture_root: root.display().to_string(),
                fixture_seed_root: root.display().to_string(),
                source_fixture: "fixture.mp4".to_owned(),
                source_sha256: "0".repeat(64),
            },
            metrics: Metrics::default(),
            lifecycle: Lifecycle::default(),
            churn: Churn::default(),
            reveal: Reveal::default(),
            safety: Safety {
                production_credentials_loaded: false,
                cloud_network_requests: 0,
            },
            validation_ms: 0.0,
        };
        write_atomic_json(&path, &telemetry).unwrap();
        assert!(path.is_file());
        assert!(std::fs::metadata(&path).unwrap().len() <= TELEMETRY_LIMIT as u64);
        assert!(write_atomic_json(&path, &telemetry).is_err());
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn temp_cleanup_preserves_a_foreign_replacement() {
        let root = std::env::temp_dir().join(format!(
            "clipline-catalog-cleanup-test-{}-{}",
            std::process::id(),
            unix_millis(SystemTime::now())
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("telemetry.tmp");
        let original = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .unwrap();
        let original_identity = clipline_shell::opened_file_identity(&original).unwrap();
        let retained_original = root.join("retained-original.tmp");
        std::fs::hard_link(&path, &retained_original).unwrap();
        let cleanup = TempCleanup::new(path.clone(), original_identity);
        drop(original);
        clipline_shell::remove_file_if_identity(&path, original_identity).unwrap();
        std::fs::write(&path, b"foreign").unwrap();

        drop(cleanup);

        assert_eq!(std::fs::read(&path).unwrap(), b"foreign");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(retained_original).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn p95_uses_nearest_rank_without_approximating_empty_data() {
        assert_eq!(percentile_95(&[]), 0.0);
        assert_eq!(percentile_95(&[3.0]), 3.0);
        let values = (1..=100).map(f64::from).collect::<Vec<_>>();
        assert_eq!(percentile_95(&values), 95.0);
    }

    #[test]
    fn marker_timestamp_is_rfc3339_utc() {
        assert_eq!(
            utc_timestamp(UNIX_EPOCH + Duration::from_millis(1_719_843_845_678)),
            "2024-07-01T14:24:05.678Z"
        );
    }

    #[test]
    fn windows_process_creation_time_converts_to_the_unix_epoch() {
        assert_eq!(
            system_time_from_windows_filetime(WINDOWS_UNIX_EPOCH_OFFSET_100NS),
            Some(UNIX_EPOCH)
        );
        assert_eq!(
            system_time_from_windows_filetime(WINDOWS_UNIX_EPOCH_OFFSET_100NS + 123_450_000),
            Some(UNIX_EPOCH + Duration::from_millis(12_345))
        );
    }
}
