use std::io::{self, Seek, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};

use clipline_buffer::Segment;
use clipline_mp4::{AudioTrackConfig, FragSampleRef, HybridMp4Writer, TrackConfig, VideoTrackConfig};

use super::mux::{segment_audio_selections, segment_fragment_refs, set_segment_decode_times};


pub trait WriteSeek: Write + Seek + Send {}

impl<T: Write + Seek + Send> WriteSeek for T {}

#[derive(Debug)]
pub struct FullSessionSummary {
    pub start_s: f64,
    pub end_s: f64,
    pub duration_s: f64,
}

pub(crate) struct FullSessionSink {
    pub(crate) tx: SyncSender<FullSessionWriteMsg>,
    pub(crate) join: JoinHandle<()>,
    pub(crate) queued_bytes: Arc<AtomicUsize>,
    pub(crate) max_queue_bytes: usize,
    pub(crate) audio_cfgs: Vec<AudioTrackConfig>,
    pub(crate) video_cfg: Option<VideoTrackConfig>,
    pub(crate) start_s: Option<f64>,
    pub(crate) end_s: Option<f64>,
    pub(crate) send_error: Option<String>,
}

pub(crate) struct FullSessionSegment {
    pub(crate) video_cfg: VideoTrackConfig,
    pub(crate) audio_cfgs: Vec<AudioTrackConfig>,
    pub(crate) segment: Arc<Segment>,
    pub(crate) reserved_bytes: usize,
}

pub(crate) enum FullSessionWriteMsg {
    Segment(FullSessionSegment),
    Finish(Sender<io::Result<()>>),
}

pub(crate) fn spawn_full_session_writer(
    target: Box<dyn WriteSeek>,
    max_queue_segments: usize,
) -> io::Result<(
    SyncSender<FullSessionWriteMsg>,
    JoinHandle<()>,
    Arc<AtomicUsize>,
)> {
    let (tx, rx) = mpsc::sync_channel(max_queue_segments);
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let writer_queued_bytes = Arc::clone(&queued_bytes);
    let join = thread::Builder::new()
        .name("clipline-full-session-writer".into())
        .spawn(move || full_session_writer_loop(target, rx, writer_queued_bytes))?;
    Ok((tx, join, queued_bytes))
}

pub(crate) fn finish_full_session_writer(sink: FullSessionSink) -> io::Result<()> {
    let (reply_tx, reply_rx) = mpsc::channel();
    sink.tx
        .send(FullSessionWriteMsg::Finish(reply_tx))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "full session writer stopped"))?;
    let result = reply_rx.recv().unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "full session writer stopped before finalizing",
        ))
    });
    let join_result = sink.join.join();
    match (result, join_result) {
        (Err(e), _) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(_)) => Err(io::Error::other("full session writer thread panicked")),
    }
}

pub(crate) fn full_session_writer_loop(
    target: Box<dyn WriteSeek>,
    rx: Receiver<FullSessionWriteMsg>,
    queued_bytes: Arc<AtomicUsize>,
) {
    let mut target = Some(target);
    let mut writer: Option<HybridMp4Writer<Box<dyn WriteSeek>>> = None;
    let mut timeline_origin_s = None;
    let mut first_error: Option<io::Error> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            FullSessionWriteMsg::Segment(segment) => {
                let reserved_bytes = segment.reserved_bytes;
                if first_error.is_none() {
                    if let Err(e) = write_full_session_segment(
                        &mut target,
                        &mut writer,
                        &mut timeline_origin_s,
                        segment.video_cfg,
                        segment.audio_cfgs,
                        segment.segment,
                    ) {
                        first_error = Some(e);
                        writer = None;
                    }
                }
                queued_bytes.fetch_sub(reserved_bytes, Ordering::AcqRel);
            }
            FullSessionWriteMsg::Finish(reply) => {
                let result = if let Some(e) = first_error.take() {
                    Err(e)
                } else if let Some(writer) = writer.take() {
                    writer.finalize().map(|_| ())
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
                break;
            }
        }
    }
}

pub(crate) fn write_full_session_segment(
    target: &mut Option<Box<dyn WriteSeek>>,
    writer: &mut Option<HybridMp4Writer<Box<dyn WriteSeek>>>,
    timeline_origin_s: &mut Option<f64>,
    video_cfg: VideoTrackConfig,
    audio_cfgs: Vec<AudioTrackConfig>,
    seg: Arc<Segment>,
) -> io::Result<()> {
    let origin_s = *timeline_origin_s.get_or_insert(seg.pts_start_s);
    if writer.is_none() {
        let mut track_cfgs = vec![TrackConfig::Video(video_cfg.clone())];
        for cfg in &audio_cfgs {
            track_cfgs.push(TrackConfig::Audio(cfg.clone()));
        }
        let target = target.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "full session writer target missing",
            )
        })?;
        *writer = Some(HybridMp4Writer::new_multi(target, track_cfgs)?);
    }
    let writer = writer.as_mut().expect("writer initialized");
    let audio_selections = segment_audio_selections(&seg, Some(origin_s))?;
    let timelines = set_segment_decode_times(
        writer,
        seg.pts_start_s,
        &audio_selections,
        &video_cfg,
        &audio_cfgs,
        origin_s,
    )?;
    let per_track =
        segment_fragment_refs(&seg, &audio_selections, &video_cfg, &audio_cfgs, &timelines)?;
    let slices: Vec<&[FragSampleRef<'_>]> = per_track.iter().map(|v| v.as_slice()).collect();
    writer.write_fragment_multi_borrowed(&slices)
}

pub(crate) fn try_reserve_queue_bytes(queued: &AtomicUsize, bytes: usize, max_bytes: usize) -> bool {
    queued
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(bytes).filter(|next| *next <= max_bytes)
        })
        .is_ok()
}

pub(crate) fn release_message_reservation(queued: &AtomicUsize, msg: &FullSessionWriteMsg) {
    if let FullSessionWriteMsg::Segment(segment) = msg {
        queued.fetch_sub(segment.reserved_bytes, Ordering::AcqRel);
    }
}
