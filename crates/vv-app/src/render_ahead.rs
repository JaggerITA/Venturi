//! Video buffer at timeline level: a thread walks forward from the playhead
//! (and a little backwards) crossing cuts, gaps and tracks without special
//! cases, and fills a single `SharedFrameCache` on a global budget (see
//! plans/REFACTOR_PIPELINE.md §2). Decode only: the compositing stays on the UI thread,
//! the audio is played by the mixer.
//!
//! A compound clip is not decoded and not cached: its
//! content is a timeline, composed on the fly by whoever composes the outer
//! frame (`frame_provider::GpuCompounds`). Here its nested timeline is walked
//! anyway, at any depth, to keep warm the
//! real media that composition will read.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use vv_core::{ClipSource, FrameIdx, MediaId, Project, Timeline, TimelineId};
use vv_media::{Decoder, FrameYuv420, SharedFrameCache, WantedRange};
use vv_media::proxy::ProxyQuality;


/// Seconds buffered ahead of the playhead, by default.
pub const DEFAULT_LOOKAHEAD_SECS: f64 = 3.0;

/// Seconds buffered behind the playhead, by default. Few on purpose:
/// they only serve a close back-and-forth scrub; the forward window
/// always has priority on the budget.
pub const DEFAULT_BEHIND_SECS: f64 = 2.0;

/// Minimum lookahead/behind margin even if configured to 0: without a
/// cushion the playback stutters from the normal timing variability.
const MIN_MARGIN_FRAMES: FrameIdx = 4;

/// Chunks the forward window is split into, per media: see
/// `chunk_forward_segments_near_to_far`.
const FORWARD_CHUNK_FRAMES: FrameIdx = 15;

/// Chunks the window behind the playhead is split into. Decoding it in
/// a single seek would produce last the frames near the playhead, and a
/// continuous backwards scrub would never reach them: from the nearest
/// chunk to the farthest the hole stays at most one chunk wide.
const BEHIND_CHUNK_FRAMES: FrameIdx = 15;

/// Cap on the transit (frames decoded before the wanted stretch) in frames,
/// not in budget bytes: a tight budget says nothing about the GOP
/// length. Generous: it only guards against pathological GOPs.
const TRANSIT_SAFETY_CAP_FRAMES: FrameIdx = 3000;

fn store_secs(atomic: &AtomicU64, secs: f64) {
    atomic.store(secs.max(0.0).to_bits(), Ordering::Relaxed);
}

fn load_secs(atomic: &AtomicU64) -> f64 {
    f64::from_bits(atomic.load(Ordering::Relaxed))
}

/// A cycle finding everything cached returns immediately: a short poll costs little.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Seek threshold until the GOP of the media has been observed. Low: one
/// seek too many costs little, it reuses the open decoder.
const DEFAULT_SEEK_THRESHOLD_FRAMES: FrameIdx = 30;

/// Cap on the GOP estimate, until a tighter observation arrives.
const MAX_SEEK_THRESHOLD_FRAMES: FrameIdx = 300;

/// Seek threshold on the proxies: they are all-intra, seeking costs almost nothing.
/// The adaptive estimate there would stay high during a fast scrub and would
/// make dozens of useless frames be decoded in sequence.
const PROXY_SEEK_THRESHOLD_FRAMES: FrameIdx = 1;

enum Command {
    UpdateProject(Box<Project>, TimelineId),
    /// Goes through the commands and not through an atomic: the worker must react to the change
    /// by emptying caches and decoders (the frames have the wrong resolution).
    SetProxy(Option<ProxyQuality>),
    /// Wakes the worker immediately instead of waiting for `POLL_INTERVAL`.
    Wake,
    Stop,
}

/// Diagnostic log on stderr with `VV_DEBUG_RENDER_AHEAD=1`.
fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("VV_DEBUG_RENDER_AHEAD").is_ok())
}

/// State shared between `RenderAhead` (UI thread) and the worker.
struct SharedState {
    caches: SharedFrameCache,
    target: AtomicI64,
    cache_budget_bytes: AtomicUsize,
    /// The worker's last cycle found the whole window cached: the
    /// UI stops asking for repaints for the "buffered" strip.
    caught_up: AtomicBool,
    /// Seconds (bits of an `f64`, see `store_secs`) to buffer ahead of
    /// and behind the playhead. Atomics and not `Command`s: the worker re-reads them on
    /// every cycle, there is no transition to react to.
    lookahead_secs: AtomicU64,
    behind_secs: AtomicU64,
}

/// See the module docs. One per `VenturiApp`.
pub struct RenderAhead {
    shared: Arc<SharedState>,
    tx: mpsc::Sender<Command>,
    handle: Option<JoinHandle<()>>,
}

impl RenderAhead {
    pub fn spawn(
        project: Project,
        timeline_id: TimelineId,
        cache_budget_bytes: usize,
        proxy: Option<ProxyQuality>,
        lookahead_secs: f64,
        behind_secs: f64,
    ) -> Self {
        let shared = Arc::new(SharedState {
            caches: SharedFrameCache::new(),
            target: AtomicI64::new(0),
            cache_budget_bytes: AtomicUsize::new(cache_budget_bytes),
            caught_up: AtomicBool::new(false),
            lookahead_secs: AtomicU64::new(lookahead_secs.max(0.0).to_bits()),
            behind_secs: AtomicU64::new(behind_secs.max(0.0).to_bits()),
        });
        let (tx, rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(rx, &thread_shared, project, timeline_id, proxy);
        });
        Self {
            shared,
            tx,
            handle: Some(handle),
        }
    }

    /// New playhead. If it changes it immediately marks "not buffered" (or the UI
    /// would stop asking for repaints on a stale state) and wakes the
    /// worker: with narrow windows 50 ms of waiting limit the playback.
    pub fn set_target(&self, frame: FrameIdx) {
        let previous = self.shared.target.swap(frame, Ordering::Relaxed);
        if previous != frame {
            self.shared.caught_up.store(false, Ordering::Relaxed);
            let _ = self.tx.send(Command::Wake);
        }
    }

    pub fn set_cache_budget_bytes(&self, bytes: usize) {
        self.shared.cache_budget_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Seconds buffered ahead (Settings > Playback), never below
    /// `MIN_MARGIN_FRAMES`.
    pub fn set_lookahead_secs(&self, secs: f64) {
        store_secs(&self.shared.lookahead_secs, secs);
        self.shared.caught_up.store(false, Ordering::Relaxed);
    }

    /// How many seconds of timeline to buffer *behind* the playhead too
    /// (Settings > Playback) — see the docs of `DEFAULT_BEHIND_SECS`.
    pub fn set_behind_secs(&self, secs: f64) {
        store_secs(&self.shared.behind_secs, secs);
        self.shared.caught_up.store(false, Ordering::Relaxed);
    }

    /// The worker works on a copy of the project: it must be updated on every
    /// change of the history.
    pub fn update_project(&self, project: &Project, timeline_id: TimelineId) {
        self.shared.caught_up.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::UpdateProject(
            Box::new(project.clone()),
            timeline_id,
        ));
    }

    /// "Use proxy" toggle (plans/REFACTOR_PIPELINE.md proxy): the worker
    /// empties the shared cache and reopens from scratch every decoder on the
    /// right path for the new state — see the docs of `Command::SetProxy`.
    pub fn set_proxy(&self, proxy: Option<ProxyQuality>) {
        self.shared.caught_up.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::SetProxy(proxy));
    }

    /// The decoded frame for `(media_id, source_frame)`, if already
    /// cached.
    pub fn get_frame(&self, media_id: MediaId, source_frame: FrameIdx) -> Option<Arc<FrameYuv420>> {
        self.shared.caches.get(media_id, source_frame)
    }

    /// `false` while work remains for the current window: the UI keeps
    /// asking for repaints to advance the "buffered" strip.
    pub fn is_caught_up(&self) -> bool {
        self.shared.caught_up.load(Ordering::Relaxed)
    }

    /// Cached intervals of a media, in source frames.
    pub fn cached_ranges_for(&self, media_id: MediaId) -> Vec<(FrameIdx, FrameIdx)> {
        self.shared.caches.cached_ranges(media_id)
    }
}

impl Drop for RenderAhead {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Non-blocking and never an error: a media that does not decode is
/// skipped by the worker.
impl crate::frame_provider::FrameProvider for RenderAhead {
    fn frame_for(
        &mut self,
        _project: &Project,
        clip: &vv_core::Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<FrameYuv420>>, String> {
        let Some((media_id, source_frame)) =
            crate::frame_provider::media_source_frame(clip, timeline_frame)
        else {
            return Ok(None);
        };
        Ok(self.get_frame(media_id, source_frame))
    }
}

/// Decoder open for a media, with the next frame it will produce: it says whether
/// it is better to decode in sequence or to seek. One per media, so a
/// cut between two media does not make them reopen.
struct OpenDecoder {
    decoder: Decoder,
    /// Open path: if the proxy becomes available or the toggle changes, the
    /// decoder must be reopened on the new one.
    resolved_path: std::path::PathBuf,
    /// `true` when `resolved_path` is a proxy (plans/REFACTOR_PIPELINE.md
    /// proxy) — see `PROXY_SEEK_THRESHOLD_FRAMES` on why it bypasses the
    /// adaptive GOP estimate instead of merely initializing it.
    is_all_intra: bool,
    next_frame: FrameIdx,
    /// After a seek or an open the first frame is a keyframe: it serves to
    /// learn the GOP of the media.
    just_repositioned: bool,
    /// Last seek landing and GOP estimated from the distance between landings.
    last_keyframe_landed: Option<FrameIdx>,
    estimated_gop: Option<FrameIdx>,
}

impl OpenDecoder {
    fn fresh(decoder: Decoder, resolved_path: std::path::PathBuf, is_all_intra: bool) -> Self {
        Self {
            decoder,
            resolved_path,
            is_all_intra,
            next_frame: 0,
            just_repositioned: true,
            last_keyframe_landed: None,
            estimated_gop: None,
        }
    }

    /// About one observed GOP, a fallback while there is none; fixed on the proxies.
    fn seek_threshold_frames(&self) -> FrameIdx {
        if self.is_all_intra {
            return PROXY_SEEK_THRESHOLD_FRAMES;
        }
        self.estimated_gop.unwrap_or(DEFAULT_SEEK_THRESHOLD_FRAMES)
    }

    /// Updates the GOP estimate with a new landing. It keeps the minimum:
    /// a jump of several GOPs would overestimate it.
    fn record_keyframe_landing(&mut self, idx: FrameIdx) {
        if let Some(prev) = self.last_keyframe_landed
            && idx > prev
        {
            let observed = (idx - prev).min(MAX_SEEK_THRESHOLD_FRAMES);
            self.estimated_gop = Some(match self.estimated_gop {
                Some(g) => g.min(observed),
                None => observed,
            });
        }
        if debug_enabled() {
            eprintln!(
                "[render_ahead] ATTERRAGGIO idx={idx} last_keyframe_landed_prima={:?} estimated_gop_dopo={:?}",
                self.last_keyframe_landed, self.estimated_gop
            );
        }
        self.last_keyframe_landed = Some(idx);
    }
}

fn worker_loop(
    rx: mpsc::Receiver<Command>,
    shared: &SharedState,
    mut project: Project,
    mut timeline_id: TimelineId,
    mut proxy: Option<ProxyQuality>,
) {
    let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
    // Separate decoders for the window behind the playhead: see `walk_and_fill`.
    let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
    // `from` of the previous cycle (only that, never a historical min/max):
    // it says whether the playhead went backwards.
    let mut last_from_frame: Option<FrameIdx> = None;
    // Previous cycle interrupted by a playhead jump: it restarts
    // immediately instead of waiting for `POLL_INTERVAL`.
    let mut retry_immediately = false;
    loop {
        let first = if retry_immediately {
            match rx.try_recv() {
                Ok(cmd) => Some(cmd),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        // The commands already queued too: only the final state counts.
        for cmd in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match cmd {
                Command::Stop => return,
                Command::UpdateProject(p, id) => {
                    project = *p;
                    timeline_id = id;
                    open.clear();
                    open_behind.clear();
                }
                Command::SetProxy(v) => {
                    proxy = v;
                    shared.caches.clear();
                    open.clear();
                    open_behind.clear();
                }
                Command::Wake => {}
            }
        }

        let from = shared.target.load(Ordering::Relaxed);
        let went_backward = last_from_frame.is_some_and(|last| from < last);
        last_from_frame = Some(from);
        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &shared.caches,
            &mut open,
            &mut open_behind,
            from,
            shared.cache_budget_bytes.load(Ordering::Relaxed),
            went_backward,
            proxy,
            load_secs(&shared.lookahead_secs),
            load_secs(&shared.behind_secs),
            &shared.target,
        );
        retry_immediately = outcome.interrupted;
        shared.caught_up.store(outcome.caught_up, Ordering::Relaxed);
    }
}

/// What `position_decoder` did to satisfy the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Positioned {
    /// Decoder already open and already well positioned: no seek, the front
    /// of the buffer advances in sequence at no extra cost.
    Reused,
    /// Decoder already open, repositioned with a seek on the existing
    /// decoder (`seek_to_time`, not a reopen).
    Seeked,
    /// No decoder open for this media: opened from scratch.
    Opened,
    /// Opening the media failed: the caller skips the segment.
    Failed,
}

/// Stretch of a Media clip in the window, in source frames, with its
/// position on the timeline (used to measure the distance from the playhead).
#[derive(Clone, Copy, Debug)]
struct MediaSegment {
    media_id: MediaId,
    source_start: FrameIdx,
    source_end: FrameIdx,
    timeline_start: FrameIdx,
    /// `Clip::rate` of the clip the segment comes from: needed by
    /// `chunk_behind_segments_near_to_far` to translate an offset in
    /// source frames into the corresponding timeline offset.
    rate: vv_core::Rational,
}

/// Segments of `[from_frame, end_frame)`, one per Media clip of every video
/// track (including those below: they show in the letterbox bars),
/// from the nearest to the playhead and, at equal position, from the highest track.
/// Real media only: a compound clip is not decoded, it is walked
/// into (at any nesting depth).
/// Limit on the nesting depth of compound clips this module
/// follows: since the project timeline shows up in the media pool too
/// (see `MediaItem::compound`), dragging it inside itself (or inside one of
/// its compound clips) would create a cycle — without a limit, a stack
/// overflow instead of a plain "not composed". Generous for real use.
const MAX_COMPOUND_DEPTH: u32 = 16;

fn collect_media_segments(
    project: &Project,
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut real = clipped_media_segments(project, timeline, from_frame, end_frame, 0);
    real.sort_by_key(|(track, s)| (s.timeline_start, std::cmp::Reverse(*track)));
    strip_track(real)
}

/// Like `collect_media_segments` for the window behind: from the nearest
/// to the playhead, i.e. from the farthest ahead on the timeline.
fn collect_media_segments_behind(
    project: &Project,
    timeline: &Timeline,
    from_frame: FrameIdx,
    start_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut real = clipped_media_segments(project, timeline, start_frame, from_frame, 0);
    real.sort_by_key(|(track, s)| (std::cmp::Reverse(s.timeline_start), std::cmp::Reverse(*track)));
    strip_track(real)
}

fn strip_track(segments: Vec<(usize, MediaSegment)>) -> Vec<MediaSegment> {
    segments.into_iter().map(|(_, s)| s).collect()
}

/// The part common to the two functions above: every Media clip of every video
/// track clipped to `[from_frame, end_frame)`, with the index of its
/// track so they can be sorted afterwards. Not sorted. Recursive: a clip
/// referencing a compound clip (`MediaItem::compound`) does not generate a
/// segment to decode, but — on the same range, translated into frames
/// of its nested timeline by `Clip::source_frame_at` — the segments
/// collected inside that timeline, at any depth (within
/// `MAX_COMPOUND_DEPTH`).
fn clipped_media_segments(
    project: &Project,
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
    depth: u32,
) -> Vec<(usize, MediaSegment)> {
    let mut real = Vec::new();
    if depth >= MAX_COMPOUND_DEPTH {
        return real;
    }
    for (track_index, track) in timeline.tracks_of_kind(vv_core::TrackKind::Video) {
        if track.muted {
            continue;
        }
        for clip in track.clips.iter().filter(|c| !c.disabled) {
            let ClipSource::Media(media_id) = &clip.source else {
                continue;
            };
            let segment_start = clip.timeline_start.max(from_frame);
            let segment_end = clip.timeline_end().min(end_frame);
            if segment_end <= segment_start {
                continue;
            }
            // clip→source-frame mapping shared with the export
            // (`vv_core::Clip::source_frame_at`, see the docs there for the
            // reason — plans/REFACTOR_PIPELINE.md B1).
            let source_start = clip.source_frame_at(segment_start);
            let source_end = clip.source_frame_at(segment_end - 1);
            let segment = MediaSegment {
                media_id: *media_id,
                source_start,
                source_end,
                timeline_start: segment_start,
                rate: clip.rate,
            };
            push_or_recurse(project, track_index, segment, depth, &mut real);
        }
    }
    real
}

/// Adds `segment` to the media to decode, or — if `segment.media_id` is
/// a compound clip, which is never decoded — recurses into its
/// nested timeline on the same range (in source frames, i.e. already in the
/// space of that timeline).
fn push_or_recurse(
    project: &Project,
    track_index: usize,
    segment: MediaSegment,
    depth: u32,
    real: &mut Vec<(usize, MediaSegment)>,
) {
    match project.media_pool.get(segment.media_id).and_then(|m| m.compound) {
        Some(nested_id) => {
            if let Some(nested) = project.timelines.get(nested_id) {
                real.extend(clipped_media_segments(project, nested, segment.source_start, segment.source_end + 1, depth + 1));
            }
        }
        None => real.push((track_index, segment)),
    }
}

/// Segments "lent" for the crossing transitions active in
/// `[from_frame, end_frame)`: `extrapolated_frame_for` (`frame_provider.rs`)
/// never freezes a clip on its last/first real frame, but keeps
/// playing the footage the trim had discarded until it reaches the real
/// end of the media — that is, past its own declared edge, it asks for a
/// growing range of source frames, not a single fixed frame. No
/// segment of `clipped_media_segments` ever covers it (it is clipped tightly
/// to the declared range of each clip), so without this
/// `SharedFrameCache::reconcile` evicts it (or never fetches it) as soon as
/// the playhead passes the cut, freezing the compositing for that half
/// of the crossing window. Like `clipped_media_segments`, real media
/// only: a compound clip is walked into.
fn crossing_borrowed_segments(
    project: &Project,
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut real = Vec::new();
    for (_, track) in timeline.tracks_of_kind(vv_core::TrackKind::Video) {
        if track.muted {
            continue;
        }
        for crossing in &track.crossings {
            let (Some(left), Some(right)) = (track.clip(crossing.left_clip), track.clip(crossing.right_clip)) else {
                continue;
            };
            let window = crossing.window(left, right);
            if window.end <= from_frame || window.start >= end_frame {
                continue;
            }
            // `left` lends the stretch after its own declared edge,
            // `right` the one before: the other half of each is already covered
            // by its own normal segment.
            if window.end > left.timeline_end() {
                push_borrowed_segment(project, left, left.timeline_end(), window.end - 1, &mut real);
            }
            if window.start < right.timeline_start {
                push_borrowed_segment(project, right, window.start, right.timeline_start - 1, &mut real);
            }
        }
    }
    real
}

/// A source segment for the stretch of `clip` lent to a crossing,
/// `[from_timeline, to_timeline]` (inclusive) in timeline frames,
/// clamped to the same edges of `media.meta.duration_frames` that
/// `held_timeline_frame` would use — otherwise it would ask to buffer a
/// source frame the decoder will never produce.
fn push_borrowed_segment(
    project: &Project,
    clip: &vv_core::Clip,
    from_timeline: FrameIdx,
    to_timeline: FrameIdx,
    real: &mut Vec<MediaSegment>,
) {
    let ClipSource::Media(media_id) = &clip.source else {
        return;
    };
    let Some(item) = project.media_pool.get(*media_id) else {
        return;
    };
    let last = (item.meta.duration_frames - 1).max(0);
    let a = clip.source_frame_at(from_timeline).clamp(0, last);
    let b = clip.source_frame_at(to_timeline).clamp(0, last);
    let segment = MediaSegment {
        media_id: *media_id,
        source_start: a.min(b),
        source_end: a.max(b),
        timeline_start: from_timeline,
        rate: clip.rate,
    };
    match item.compound {
        Some(nested_id) => {
            if let Some(nested) = project.timelines.get(nested_id) {
                real.extend(strip_track(clipped_media_segments(
                    project,
                    nested,
                    segment.source_start,
                    segment.source_end + 1,
                    0,
                )));
            }
        }
        None => real.push(segment),
    }
}

/// Splits the forward segments into chunks of `FORWARD_CHUNK_FRAMES` and orders
/// them by timeline position, so clips playing together (two video tracks) are
/// filled interleaved. Whole segment at a time, the playhead interrupts the
/// cycle before the lower track is ever reached and the compositing shows it
/// black.
fn chunk_forward_segments_near_to_far(segments: &[MediaSegment]) -> Vec<MediaSegment> {
    let mut chunks: Vec<MediaSegment> = Vec::new();
    for segment in segments {
        let mut chunk_start = segment.source_start;
        loop {
            let chunk_end = (chunk_start + FORWARD_CHUNK_FRAMES - 1).min(segment.source_end);
            let offset = segment.rate.scale_round(chunk_start)
                - segment.rate.scale_round(segment.source_start);
            chunks.push(MediaSegment {
                media_id: segment.media_id,
                source_start: chunk_start,
                source_end: chunk_end,
                timeline_start: segment.timeline_start + offset,
                rate: segment.rate,
            });
            if chunk_end == segment.source_end {
                break;
            }
            chunk_start = chunk_end + 1;
        }
    }
    // Stable: at equal position the order of the segments (topmost track first) holds.
    chunks.sort_by_key(|c| c.timeline_start);
    chunks
}

/// Splits the segments behind the playhead into chunks of `BEHIND_CHUNK_FRAMES`,
/// from the edge near the playhead (`source_end`) towards the far one.
fn chunk_behind_segments_near_to_far(segments: &[MediaSegment]) -> Vec<MediaSegment> {
    let mut chunks = Vec::new();
    for segment in segments {
        let mut chunk_end = segment.source_end;
        loop {
            let chunk_start = (chunk_end - BEHIND_CHUNK_FRAMES + 1).max(segment.source_start);
            let offset = segment.rate.scale_round(chunk_start)
                - segment.rate.scale_round(segment.source_start);
            chunks.push(MediaSegment {
                media_id: segment.media_id,
                source_start: chunk_start,
                source_end: chunk_end,
                timeline_start: segment.timeline_start + offset,
                rate: segment.rate,
            });
            if chunk_start == segment.source_start {
                break;
            }
            chunk_end = chunk_start - 1;
        }
    }
    chunks
}

/// Removes the chunks already cached. In the window behind, the decoder stays
/// parked on the farthest chunk of the previous cycle: without the filter every chunk
/// would look like it needs reseeking on every cycle, even with the playhead still.
fn without_already_cached_chunks(
    caches: &SharedFrameCache,
    chunks: Vec<MediaSegment>,
) -> Vec<MediaSegment> {
    chunks
        .into_iter()
        .filter(|chunk| {
            !caches.covers(chunk.media_id, chunk.source_start, chunk.source_end)
        })
        .collect()
}

/// Brings `open[media_id]` where it can cover `segment_start` by decoding
/// forward. A decoder already past the segment is the normal state and is reused.
/// It seeks (on the open decoder, never reopening: reparsing the container can
/// cost seconds) only if the segment is too far ahead, if the playhead went
/// back and the decoder passed it, or if the cache has a hole
/// where the decoder thinks it already went through. `went_backward` is decided
/// once per cycle: per media the order of the segments would dirty it.
fn position_decoder(
    caches: &SharedFrameCache,
    open: &mut HashMap<MediaId, OpenDecoder>,
    media_id: MediaId,
    path: &Path,
    segment_start: FrameIdx,
    went_backward: bool,
    is_all_intra: bool,
    is_image: bool,
) -> Positioned {
    if open.get(&media_id).is_some_and(|o| o.resolved_path != path) {
        open.remove(&media_id);
    }
    if let Some(o) = open.get_mut(&media_id) {
        let needs_seek = segment_start > o.next_frame + o.seek_threshold_frames()
            || (went_backward && segment_start < o.next_frame)
            // An eviction may have removed the tail the decoder thinks it already
            // produced: it is checked against the cache.
            || (o.next_frame > segment_start
                && !caches.covers(media_id, segment_start, o.next_frame - 1));
        if !needs_seek {
            return Positioned::Reused;
        }
        let secs = segment_start as f64 / o.decoder.fps().as_f64().max(1e-9);
        let debug_start = debug_enabled().then(std::time::Instant::now);
        let _ = o.decoder.seek_to_time(secs);
        if let Some(t) = debug_start {
            eprintln!(
                "[render_ahead] seek (decoder riusato) media={media_id:?} target={segment_start} elapsed={:?}",
                t.elapsed()
            );
        }
        // Placeholder: the next frame will say where the seek really landed.
        o.next_frame = 0;
        o.just_repositioned = true;
        return Positioned::Seeked;
    }
    // No decoder open for this media: here the real open is
    // unavoidable (first time, or a media different from the one open so far).
    let debug_start = debug_enabled().then(std::time::Instant::now);
    // An image must be opened with `open_image`, or it would hit EOF after the
    // first frame.
    let opened = if is_image { Decoder::open_image(path) } else { Decoder::open(path) };
    let Ok(mut decoder) = opened else {
        return Positioned::Failed;
    };
    let secs = segment_start as f64 / decoder.fps().as_f64().max(1e-9);
    let _ = decoder.seek_to_time(secs);
    if let Some(t) = debug_start {
        eprintln!(
            "[render_ahead] OPEN (nuovo decoder) media={media_id:?} path={} target={segment_start} elapsed={:?}",
            path.display(),
            t.elapsed()
        );
    }
    open.insert(
        media_id,
        OpenDecoder::fresh(decoder, path.to_path_buf(), is_all_intra),
    );
    Positioned::Opened
}

/// Outcome of one `walk_and_fill` cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalkOutcome {
    /// The playhead moved enough to make the remaining work obsolete:
    /// it restarts immediately with the new target.
    interrupted: bool,
    /// The whole window is cached: the UI can stop asking for repaints.
    caught_up: bool,
}

impl WalkOutcome {
    const SETTLED: Self = Self {
        interrupted: false,
        caught_up: true,
    };
    /// Stopped by the budget or by transit: there is still work, but not right away.
    const UNFINISHED: Self = Self {
        interrupted: false,
        caught_up: false,
    };
}

fn walk_and_fill(
    project: &Project,
    timeline_id: TimelineId,
    caches: &SharedFrameCache,
    open: &mut HashMap<MediaId, OpenDecoder>,
    open_behind: &mut HashMap<MediaId, OpenDecoder>,
    from_frame: FrameIdx,
    cache_budget_bytes: usize,
    went_backward: bool,
    proxy: Option<ProxyQuality>,
    lookahead_secs: f64,
    behind_secs: f64,
    target: &AtomicI64,
) -> WalkOutcome {
    let Some(timeline) = project.timelines.get(timeline_id) else {
        return WalkOutcome::SETTLED;
    };
    let fps = timeline.fps.as_f64().max(1e-9);
    let lookahead_frames = ((lookahead_secs * fps).round() as FrameIdx).max(MIN_MARGIN_FRAMES);
    let behind_frames = ((behind_secs * fps).round() as FrameIdx).max(MIN_MARGIN_FRAMES);
    let end_frame = from_frame + lookahead_frames;
    let start_frame = (from_frame - behind_frames).max(0);

    let mut forward_segments = collect_media_segments(project, timeline, from_frame, end_frame);
    let mut behind_segments = collect_media_segments_behind(project, timeline, from_frame, start_frame);
    forward_segments.extend(crossing_borrowed_segments(project, timeline, from_frame, end_frame));
    behind_segments.extend(crossing_borrowed_segments(project, timeline, start_frame, from_frame));
    if forward_segments.is_empty() && behind_segments.is_empty() {
        return WalkOutcome::SETTLED;
    }
    let forward_media: HashSet<MediaId> = forward_segments.iter().map(|s| s.media_id).collect();
    let behind_media: HashSet<MediaId> = behind_segments.iter().map(|s| s.media_id).collect();
    open.retain(|id, _| forward_media.contains(id));
    open_behind.retain(|id, _| behind_media.contains(id));

    let window: Vec<WantedRange> = forward_segments
        .iter()
        .chain(behind_segments.iter())
        .map(|s| WantedRange {
            media_id: s.media_id,
            source_start: s.source_start,
            source_end: s.source_end,
            timeline_start: s.timeline_start,
            rate: s.rate,
        })
        .collect();
    // Discards what is outside both windows and, past the budget, the
    // farthest from the playhead.
    caches.reconcile(from_frame, &window, cache_budget_bytes);
    if debug_enabled() {
        for id in forward_media
            .iter()
            .chain(behind_media.iter())
            .collect::<HashSet<_>>()
        {
            eprintln!(
                "[render_ahead] DOPO-RECONCILE media={id:?} cached_ranges={:?}",
                caches.cached_ranges(*id)
            );
        }
    }

    let ctx = FillContext {
        project,
        caches,
        went_backward,
        cache_budget_bytes,
        from_frame,
        proxy,
        target,
    };
    // The forward window first: behind gets only the budget left over.
    let forward_chunks = chunk_forward_segments_near_to_far(&forward_segments);
    if let ControlFlow::Break(outcome) = fill_segments(&forward_chunks, &ctx, open) {
        return outcome;
    }
    // Decoders separate from the forward ones: a single decoder would stay
    // past every segment behind and would skip them all. For the remaining chunks
    // a seek is always needed, so `went_backward` is forced.
    let all_behind_chunks = chunk_behind_segments_near_to_far(&behind_segments);
    let behind_chunks = without_already_cached_chunks(caches, all_behind_chunks.clone());
    if debug_enabled() {
        eprintln!(
            "[render_ahead] DIETRO from_frame={from_frame} behind_secs={behind_secs} behind_frames={behind_frames} blocchi_totali={} blocchi_da_processare={} bytes_used={} budget={cache_budget_bytes}",
            all_behind_chunks.len(),
            behind_chunks.len(),
            caches.bytes_used(),
        );
        for c in &all_behind_chunks {
            let processed = behind_chunks
                .iter()
                .any(|b| b.source_start == c.source_start && b.source_end == c.source_end);
            eprintln!(
                "[render_ahead]   blocco media={:?} [{},{}] {}",
                c.media_id,
                c.source_start,
                c.source_end,
                if processed {
                    "DA PROCESSARE"
                } else {
                    "già in cache, saltato"
                }
            );
        }
    }
    let behind_ctx = FillContext {
        went_backward: true,
        ..ctx
    };
    if let ControlFlow::Break(outcome) = fill_segments(&behind_chunks, &behind_ctx, open_behind) {
        return outcome;
    }
    // Immediately removes the leftover transit frames: the UI reads the state just
    // declared `caught_up` and must not see a strip wider than the truth.
    caches.reconcile(from_frame, &window, cache_budget_bytes);
    WalkOutcome::SETTLED
}

/// Parameters of `fill_segments` common to both windows.
struct FillContext<'a> {
    project: &'a Project,
    caches: &'a SharedFrameCache,
    went_backward: bool,
    cache_budget_bytes: usize,
    from_frame: FrameIdx,
    /// Proxy quality in use, `None` if off (plans/REFACTOR_PIPELINE.md proxy), as read
    /// from the last `Command::SetProxy` — see `fill_segments`
    /// where it decides whether to resolve the source path or the proxy one.
    proxy: Option<ProxyQuality>,
    target: &'a AtomicI64,
}

/// Decodes what is needed to cover `segments`, stopping if the budget is
/// saturated or if the live playhead moved too far. `Break` carries the final
/// outcome.
fn fill_segments(
    segments: &[MediaSegment],
    ctx: &FillContext,
    open: &mut HashMap<MediaId, OpenDecoder>,
) -> ControlFlow<WalkOutcome> {
    for segment in segments {
        let Some(item) = ctx.project.media_pool.get(segment.media_id) else {
            continue;
        };
        // Already all cached: the position the decoder believes it has is not
        // trusted.
        if ctx.caches.covers(segment.media_id, segment.source_start, segment.source_end) {
            continue;
        }
        // Without room for even one frame it does not start: it would pay the
        // transit for a frame then rejected, cycle after cycle. The transit never
        // counts against the budget of the wanted stretch.
        let estimated_frame_bytes = vv_media::yuv420_frame_bytes(item.meta.width, item.meta.height);
        if ctx.caches.bytes_used() + estimated_frame_bytes > ctx.cache_budget_bytes {
            if debug_enabled() {
                eprintln!(
                    "[render_ahead] BUDGET-GIA-SATURO media={:?} segment=[{},{}] bytes_used={} budget={}",
                    segment.media_id,
                    segment.source_start,
                    segment.source_end,
                    ctx.caches.bytes_used(),
                    ctx.cache_budget_bytes
                );
            }
            return ControlFlow::Break(WalkOutcome::UNFINISHED);
        }
        // Proxy only if enabled and already generated; otherwise the source, until the
        // proxy shows up on disk.
        let proxy_path = ctx
            .proxy
            .filter(|&q| vv_media::proxy::proxy_exists(item.content_hash, q))
            .map(|q| vv_media::proxy::proxy_path_for(item.content_hash, q));
        let is_proxy = proxy_path.is_some();
        let path = proxy_path.unwrap_or_else(|| item.path.clone());
        if position_decoder(
            ctx.caches,
            open,
            segment.media_id,
            &path,
            segment.source_start,
            ctx.went_backward,
            is_proxy,
            item.meta.is_image(),
        ) == Positioned::Failed
        {
            continue;
        }

        // `next_frame` is a placeholder until the first frame after the seek
        // tells the real position.
        let mut resumed = false;
        // Transit bytes of this round: they must not prevent the segment from
        // reaching itself with a tight budget.
        let mut transit_bytes: usize = 0;
        let mut transit_frames: FrameIdx = 0;
        loop {
            let od = open.get_mut(&segment.media_id).unwrap();
            if od.next_frame > segment.source_end {
                break;
            }
            // Rejoined with a part already cached all the way to the end of the segment: the
            // rest is already there. A single contiguous interval is needed, not two points
            // present on different islands.
            if resumed
                && ctx.caches.covers(segment.media_id, od.next_frame, segment.source_end)
            {
                if debug_enabled() {
                    eprintln!(
                        "[render_ahead] RIAGGANCIO media={:?} segment=[{},{}] next_frame={}",
                        segment.media_id, segment.source_start, segment.source_end, od.next_frame
                    );
                }
                break;
            }
            let mut threshold_frames = od.seek_threshold_frames();
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    if od.just_repositioned {
                        od.record_keyframe_landing(idx);
                        od.just_repositioned = false;
                        threshold_frames = od.seek_threshold_frames();
                    }
                    let frame_bytes = frame.byte_len();
                    if idx >= segment.source_start {
                        if ctx.caches.bytes_used().saturating_sub(transit_bytes)
                            >= ctx.cache_budget_bytes
                        {
                            if debug_enabled() {
                                eprintln!(
                                    "[render_ahead] BUDGET-SATURO media={:?} segment=[{},{}] next_frame={} bytes_used={} transit_bytes={transit_bytes} budget={}",
                                    segment.media_id,
                                    segment.source_start,
                                    segment.source_end,
                                    od.next_frame,
                                    ctx.caches.bytes_used(),
                                    ctx.cache_budget_bytes
                                );
                            }
                            return ControlFlow::Break(WalkOutcome::UNFINISHED);
                        }
                    } else {
                        // Transit before the segment: kept anyway, a nearby segment in
                        // this same round reuses it instead of crossing the GOP again.
                        if transit_frames >= TRANSIT_SAFETY_CAP_FRAMES {
                            if debug_enabled() {
                                eprintln!(
                                    "[render_ahead] TRANSITO-ECCESSIVO media={:?} segment=[{},{}] next_frame={} transit_frames={transit_frames}",
                                    segment.media_id,
                                    segment.source_start,
                                    segment.source_end,
                                    od.next_frame,
                                );
                            }
                            return ControlFlow::Break(WalkOutcome::UNFINISHED);
                        }
                        transit_bytes += frame_bytes;
                        transit_frames += 1;
                    }
                    ctx.caches.insert(segment.media_id, idx, frame);
                    od.next_frame = idx + 1;
                    resumed = true;
                }
                Ok(None) => {
                    if debug_enabled() {
                        eprintln!(
                            "[render_ahead] DECODE-FINE(EOF) media={:?} segment=[{},{}] next_frame={}",
                            segment.media_id,
                            segment.source_start,
                            segment.source_end,
                            od.next_frame
                        );
                    }
                    break;
                }
                Err(e) => {
                    if debug_enabled() {
                        eprintln!(
                            "[render_ahead] DECODE-FINE(ERR) media={:?} segment=[{},{}] next_frame={} errore={e}",
                            segment.media_id,
                            segment.source_start,
                            segment.source_end,
                            od.next_frame
                        );
                    }
                    break;
                }
            }
            // Live target: if the playhead moved past the threshold the prefetch is
            // obsolete and it restarts immediately.
            let live = ctx.target.load(Ordering::Relaxed);
            if (live - ctx.from_frame).abs() > threshold_frames {
                if debug_enabled() {
                    eprintln!(
                        "[render_ahead] INTERROTTO media={:?} segment=[{},{}] live={live} from_frame={} threshold={threshold_frames} estimated_gop={:?} last_keyframe_landed={:?}",
                        segment.media_id,
                        segment.source_start,
                        segment.source_end,
                        ctx.from_frame,
                        open.get(&segment.media_id).unwrap().estimated_gop,
                        open.get(&segment.media_id).unwrap().last_keyframe_landed,
                    );
                }
                return ControlFlow::Break(WalkOutcome {
                    interrupted: true,
                    caught_up: false,
                });
            }
        }

        if debug_enabled() {
            let final_next_frame = open.get(&segment.media_id).unwrap().next_frame;
            let from_frame = ctx.from_frame;
            eprintln!(
                "[render_ahead] media={:?} target_frame={from_frame} segment=[{},{}] next_frame_after={final_next_frame} bytes_used={} cached_ranges={:?}",
                segment.media_id,
                segment.source_start,
                segment.source_end,
                ctx.caches.bytes_used(),
                ctx.caches.cached_ranges(segment.media_id)
            );
        }
    }
    ControlFlow::Continue(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vv_core::{Clip, ClipId, MediaItem, MediaMeta, Rational, Track, TrackKind};

    fn make_test_clip(dir_name: &str, file_name: &str, duration_secs: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            &path,
        );
        path
    }

    fn make_test_image(dir_name: &str, file_name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=green:size=320x240:rate=1:duration=1",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &path,
        );
        path
    }

    /// End-to-end regression for image support
    /// (`Decoder::open_image`, `position_decoder`): a still image
    /// stretched over a 2.4s clip at 25fps asks for source frames up to
    /// ~60 — well past the single real frame an image has. Before the
    /// dedicated support, a plain `Decoder::open` would have hit EOF
    /// on any position past the first, leaving the cache uncovered
    /// for the rest of the clip.
    #[test]
    fn walk_and_fill_decodes_a_stretched_image_clip_past_its_only_real_frame() {
        let path = make_test_image("vv-app-render-ahead-image-test", "still.png");
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: vv_core::IMAGE_DURATION_FRAMES,
                fps: vv_media::IMAGE_FPS,
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        // 60 frames (2.4s at 25fps): within `DEFAULT_LOOKAHEAD_SECS`
        // (3s), otherwise the last frame would stay outside the window
        // for a reason independent of this test (the lookahead, not the
        // image support).
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 60)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 4 * 200;
        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            generous_budget,
            false,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );

        assert!(!outcome.interrupted);
        for frame in [0, 1, 30, 59] {
            assert!(
                caches.get(media_a, frame).is_some(),
                "frame sorgente {frame} dell'immagine non è stato decodificato"
            );
        }
    }

    /// Like `make_test_clip`, but with a short and explicit GOP: without
    /// this, the keyframe nearest to a target far from the start
    /// is still the initial one (default keyint 250, longer
    /// than the duration of the test clips), so a seek further into
    /// the file would still have to cross in sequence everything
    /// preceding it — masking a possible incorrect eviction of a
    /// portion already buffered, because it would be regenerated anyway
    /// on the way. With a short GOP the seek can jump directly
    /// near the target without touching the preceding portions already
    /// cached, making an undue eviction visible.
    fn make_test_clip_with_short_gop(
        dir_name: &str,
        file_name: &str,
        duration_secs: u32,
        gop: u32,
    ) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-g",
                &gop.to_string(),
                "-keyint_min",
                &gop.to_string(),
            ],
            &path,
        );
        path
    }

    /// Two distinct `MediaId`s (slotmap keys, not generatable by hand):
    /// enough for the pure tests of `collect_media_segments`, which do not
    /// need a real `MediaItem` behind them.
    fn dummy_media_item() -> MediaItem {
        MediaItem {
            path: "dummy.mp4".into(),
            meta: MediaMeta {
                duration_frames: 0,
                fps: Rational::new(25, 1),
                width: 0,
                height: 0,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        }
    }

    fn two_media_ids() -> (MediaId, MediaId) {
        let mut project = Project::default();
        let a = project.media_pool.insert(dummy_media_item());
        let b = project.media_pool.insert(dummy_media_item());
        (a, b)
    }

    /// Dummy 1x1 frame: enough to populate `SharedFrameCache` in the
    /// pure tests of `without_already_cached_chunks`, which only check
    /// which indices come out covered, never the actual content.
    fn dummy_frame() -> FrameYuv420 {
        FrameYuv420 {
            width: 1,
            height: 1,
            y: vec![0],
            u: vec![0],
            v: vec![0],
            u_width: 1,
            u_height: 1,
            matrix: vv_media::ColorMatrix::Bt601,
            full_range: false,
            alpha: None,
        }
    }

    fn media_clip(id: u64, media_id: MediaId, start: FrameIdx, len: FrameIdx) -> Clip {
        Clip::from_source_range(
            ClipId(id),
            ClipSource::Media(media_id),
            0,
            len,
            start,
            Rational::one(),
        )
    }

    /// Like `media_clip`, but with an explicit `source_in` — used to
    /// simulate two clips on the timeline that are *cuts* of the same
    /// long file (sequential source ranges, not both from 0), the
    /// common case that exposes the per-segment `evict_before` bug.
    fn media_clip_trimmed(
        id: u64,
        media_id: MediaId,
        timeline_start: FrameIdx,
        source_in: FrameIdx,
        len: FrameIdx,
    ) -> Clip {
        Clip::from_source_range(
            ClipId(id),
            ClipSource::Media(media_id),
            source_in,
            source_in + len,
            timeline_start,
            Rational::one(),
        )
    }

    fn solid_clip(id: u64, start: FrameIdx, len: FrameIdx) -> Clip {
        Clip::from_source_range(ClipId(id), ClipSource::SolidColor, 0, len, start, Rational::one())
    }

    fn timeline_with(tracks: Vec<Track>) -> Timeline {
        Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (320, 240),
            tracks,
        }
    }

    #[test]
    fn collect_media_segments_walks_across_a_straight_cut_between_two_media() {
        let (media_a, media_b) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 50),
                media_clip(2, media_b, 50, 50),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        let segments = collect_media_segments(&Project::default(), &tl, 40, 60);
        assert_eq!(
            segments.len(),
            2,
            "deve attraversare il taglio in un colpo solo"
        );
        assert_eq!(segments[0].source_start, 40);
        assert_eq!(segments[0].source_end, 49);
        assert_eq!(segments[1].source_start, 0);
        assert_eq!(segments[1].source_end, 9);
    }

    #[test]
    fn collect_media_segments_skips_gaps_and_solid_color_without_decoding() {
        let (media_a, _) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 10),
                // gap 10..20
                solid_clip(2, 20, 10),
                media_clip(3, media_a, 30, 10),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        let segments = collect_media_segments(&Project::default(), &tl, 0, 40);
        assert_eq!(
            segments.len(),
            2,
            "solo le due clip Media generano segmenti"
        );
        assert_eq!((segments[0].source_start, segments[0].source_end), (0, 9));
        assert_eq!((segments[1].source_start, segments[1].source_end), (0, 9));
    }

    /// With several video tracks the buffer must cover them all, not only the
    /// top one: under the bars of a clip with an aspect different from the
    /// timeline's, the layer below shows, so it must be decoded.
    #[test]
    fn collect_media_segments_covers_every_video_track_topmost_first() {
        let (media_a, media_b) = two_media_ids();
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![media_clip(1, media_a, 0, 50)],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
            Track {
                kind: TrackKind::Video,
                clips: vec![media_clip(2, media_b, 0, 50)],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
        ]);

        let segments = collect_media_segments(&Project::default(), &tl, 0, 50);
        assert_eq!(segments.len(), 2);
        assert_eq!(
            (segments[0].media_id, segments[1].media_id),
            (media_b, media_a),
            "a pari posizione la track in cima ha la priorità"
        );
    }

    #[test]
    fn collect_media_segments_is_empty_for_a_timeline_with_no_clips() {
        let tl = timeline_with(vec![Track::new(TrackKind::Video)]);
        assert!(collect_media_segments(&Project::default(), &tl, 0, 100).is_empty());
    }

    #[test]
    fn collect_media_segments_behind_walks_across_a_straight_cut_between_two_media() {
        let (media_a, media_b) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 50),
                media_clip(2, media_b, 50, 50),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        // Window behind [40,60): crosses the cut at 50 going
        // backwards, symmetric to the forward test above.
        let segments = collect_media_segments_behind(&Project::default(), &tl, 60, 40);
        assert_eq!(
            segments.len(),
            2,
            "deve attraversare il taglio all'indietro in un colpo solo"
        );
        // Discovery order: from the nearest to the playhead (60) to the
        // farthest — first the piece of media_b [50,60), then the one of
        // media_a [40,50).
        assert_eq!(segments[0].source_start, 0);
        assert_eq!(segments[0].source_end, 9);
        assert_eq!(segments[1].source_start, 40);
        assert_eq!(segments[1].source_end, 49);
    }

    #[test]
    fn collect_media_segments_behind_skips_gaps_and_solid_color_without_decoding() {
        let (media_a, _) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 10),
                // gap 10..20
                solid_clip(2, 20, 10),
                media_clip(3, media_a, 30, 10),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        let segments = collect_media_segments_behind(&Project::default(), &tl, 40, 0);
        assert_eq!(
            segments.len(),
            2,
            "solo le due clip Media generano segmenti, il vuoto e la SolidColor vengono saltati"
        );
        assert_eq!((segments[0].source_start, segments[0].source_end), (0, 9));
        assert_eq!((segments[1].source_start, segments[1].source_end), (0, 9));
    }

    #[test]
    fn collect_media_segments_behind_is_empty_for_a_timeline_with_no_clips() {
        let tl = timeline_with(vec![Track::new(TrackKind::Video)]);
        assert!(collect_media_segments_behind(&Project::default(), &tl, 100, 0).is_empty());
    }

    #[test]
    fn collect_media_segments_behind_stops_at_the_start_frame_bound() {
        // A single long clip [0,200): the window behind must stop
        // exactly at `start_frame`, not continue to the start
        // of the clip.
        let (media_a, _) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 200)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        let segments = collect_media_segments_behind(&Project::default(), &tl, 150, 100);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].timeline_start, 100);
        assert_eq!(segments[0].source_start, 100);
        assert_eq!(segments[0].source_end, 149);
    }

    /// A project with a compound clip: its real media (inside the
    /// nested timeline) must show up among the segments to decode,
    /// in the space of the nested timeline — not in that of the outer
    /// timeline — while the compound clip itself never shows up (it is not
    /// decoded: it is composed on the fly, see `frame_provider::GpuCompounds`).
    fn project_with_compound_clip() -> (Project, Timeline, MediaId, MediaId) {
        let mut project = Project::default();
        let real_media = project.media_pool.insert(dummy_media_item());
        let nested = project.timelines.insert(Timeline {
            name: "Nested".into(),
            fps: Rational::new(25, 1),
            resolution: (320, 240),
            tracks: vec![Track {
                kind: TrackKind::Video,
                clips: vec![media_clip(100, real_media, 0, 40)],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            }],
        });
        let compound_media = project.media_pool.insert(vv_core::MediaItem {
            path: "Compound Clip 1".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 40,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: Some(nested),
        });
        let root = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, compound_media, 0, 40)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);
        (project, root, real_media, compound_media)
    }

    #[test]
    fn collect_media_segments_recurses_into_a_compound_clips_nested_timeline() {
        let (project, root, real_media, _) = project_with_compound_clip();

        let real = collect_media_segments(&project, &root, 10, 30);

        assert_eq!(real.len(), 1, "solo il media vero dentro la compound clip, che è l'unico da decodificare");
        assert_eq!(real[0].media_id, real_media);
        assert_eq!(real[0].source_start, 10, "stesso range, in frame della timeline annidata");
        assert_eq!(real[0].source_end, 29);
    }

    /// Since the project timeline shows up in the media pool too
    /// (see `MediaItem::compound`), a user can drag it inside
    /// itself: a cycle, not just a deep nesting. Without
    /// `MAX_COMPOUND_DEPTH` this would stack overflow instead of
    /// stopping.
    #[test]
    fn collect_media_segments_stops_at_a_cyclic_compound_clip_instead_of_overflowing() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(timeline_with(vec![]));
        let media_id = project.media_pool.insert(vv_core::MediaItem {
            path: "Timeline 1".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: Some(timeline_id),
        });
        // The timeline references itself through its own entry in the pool.
        project.timelines[timeline_id].tracks.push(Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_id, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        });

        let real = collect_media_segments(&project, &project.timelines[timeline_id], 0, 100);

        assert!(real.is_empty(), "nessun media vero da decodificare in un ciclo puro");
    }

    /// Two clips playing together on two video tracks must be filled
    /// interleaved chunk by chunk, not one whole clip then the other.
    #[test]
    fn chunk_forward_segments_near_to_far_interleaves_two_overlapping_tracks() {
        let (media_a, media_b) = two_media_ids();
        let segments = vec![
            MediaSegment {
                media_id: media_a,
                source_start: 0,
                source_end: 44,
                timeline_start: 100,
                rate: Rational::new(1, 1),
            },
            MediaSegment {
                media_id: media_b,
                source_start: 0,
                source_end: 44,
                timeline_start: 100,
                rate: Rational::new(1, 1),
            },
        ];

        let chunks = chunk_forward_segments_near_to_far(&segments);

        let order: Vec<(MediaId, FrameIdx, FrameIdx)> = chunks
            .iter()
            .map(|c| (c.media_id, c.source_start, c.source_end))
            .collect();
        assert_eq!(
            order,
            vec![
                (media_a, 0, 14),
                (media_b, 0, 14),
                (media_a, 15, 29),
                (media_b, 15, 29),
                (media_a, 30, 44),
                (media_b, 30, 44),
            ]
        );
    }

    /// A segment behind the playhead longer than `BEHIND_CHUNK_FRAMES`
    /// must be split into chunks from the near edge (`source_end`) to the
    /// far edge (`source_start`), each at most `BEHIND_CHUNK_FRAMES` long,
    /// with no holes nor overlaps — see the docs of
    /// `chunk_behind_segments_near_to_far`.
    #[test]
    fn chunk_behind_segments_near_to_far_splits_from_the_near_edge_without_gaps() {
        let (media_a, _) = two_media_ids();
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 100,
            source_end: 132, // 33 frames: 2 chunks of 15 + 1 of 3
            timeline_start: 500,
            rate: Rational::one(),
        };

        let chunks = chunk_behind_segments_near_to_far(&[segment]);

        assert_eq!(
            chunks
                .iter()
                .map(|c| (c.source_start, c.source_end))
                .collect::<Vec<_>>(),
            vec![(118, 132), (103, 117), (100, 102)],
            "dal bordo vicino (132) al lontano (100), ognuno da al più BEHIND_CHUNK_FRAMES"
        );
        // `timeline_start` follows the same offset as `source_start`
        // relative to the original segment (affine mapping, see the docs).
        assert_eq!(chunks[0].timeline_start, 518);
        assert_eq!(chunks[1].timeline_start, 503);
        assert_eq!(chunks[2].timeline_start, 500);
    }

    /// A segment shorter than a chunk produces a single chunk
    /// identical to the original segment — no superfluous splitting.
    #[test]
    fn chunk_behind_segments_near_to_far_keeps_a_short_segment_whole() {
        let (media_a, _) = two_media_ids();
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 40,
            source_end: 44,
            timeline_start: 40,
            rate: Rational::one(),
        };

        let chunks = chunk_behind_segments_near_to_far(&[segment]);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].source_start, 40);
        assert_eq!(chunks[0].source_end, 44);
    }

    /// `without_already_cached_chunks` must discard only the chunks
    /// entirely covered by a *single* cached interval — an
    /// uncovered or only partially covered chunk stays in the list
    /// (rechecking in that case is cheap anyway, see the docs of the
    /// function).
    #[test]
    fn without_already_cached_chunks_drops_only_fully_covered_chunks() {
        let (media_a, media_b) = two_media_ids();
        let caches = SharedFrameCache::new();
        // media_a: [100,132] entirely cached (a single dummy frame
        // for each index, just to populate `cached_ranges`).
        for idx in 100..=132 {
            caches.insert(media_a, idx, Arc::new(dummy_frame()));
        }
        // media_b: only [100,110] cached, not the whole requested chunk.
        for idx in 100..=110 {
            caches.insert(media_b, idx, Arc::new(dummy_frame()));
        }

        let chunks = vec![
            // media_a: entirely covered, must be discarded.
            MediaSegment {
                media_id: media_a,
                source_start: 118,
                source_end: 132,
                timeline_start: 118,
                rate: Rational::one(),
            },
            // media_b: only partially covered, stays.
            MediaSegment {
                media_id: media_b,
                source_start: 95,
                source_end: 110,
                timeline_start: 95,
                rate: Rational::one(),
            },
            // media_a: outside the cached interval, stays.
            MediaSegment {
                media_id: media_a,
                source_start: 50,
                source_end: 64,
                timeline_start: 50,
                rate: Rational::one(),
            },
        ];

        let remaining = without_already_cached_chunks(&caches, chunks);

        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].media_id, media_b);
        assert_eq!(remaining[1].source_start, 50);
    }

    /// End-to-end test: the worker crosses a hard cut between two
    /// different media in a single lookahead window, buffering
    /// *both* without needing any special case — exactly the
    /// required behavior ("buffer at timeline level, not at single
    /// clip level").
    #[test]
    fn render_ahead_buffers_across_a_straight_cut_between_two_different_media() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "b.mp4", 2);

        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path: path_a,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let media_b = project.media_pool.insert(MediaItem {
            path: path_b,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 50),  // [0,50)
                media_clip(2, media_b, 50, 50), // [50,100), adjacent
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
        // Target near the end of the first clip: the lookahead
        // window (3s = 75 frames at 25fps) amply crosses the
        // cut at 50.
        render_ahead.set_target(45);

        let start = std::time::Instant::now();
        loop {
            let a_ready = !render_ahead.cached_ranges_for(media_a).is_empty();
            let b_ready = !render_ahead.cached_ranges_for(media_b).is_empty();
            if a_ready && b_ready {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout: a_ready={a_ready} b_ready={b_ready}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// End-to-end reproduction of the bug reported by the user: during a
    /// crossing transition, `extrapolated_frame_for` (`frame_provider.rs`)
    /// asks, for the "lent" side, for a stretch of source frames that
    /// belongs to the NORMAL range of one clip but falls outside the range
    /// declared by the OTHER — no normal `MediaSegment` covers it, and
    /// without `crossing_borrowed_segments` `SharedFrameCache::reconcile`
    /// evicts it (or never fetches it) as soon as the playhead passes the cut,
    /// freezing the compositing for half of the crossing window.
    /// `clip_a` is trimmed short (30 of the 50 real frames available) and
    /// `clip_b` starts at `source_in=5`: the crossing eats both the footage
    /// discarded by the trim of `clip_a` and the one before the declared
    /// start of `clip_b`.
    #[test]
    fn render_ahead_keeps_both_sides_of_a_crossing_readable_through_the_whole_window() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "crossing_a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "crossing_b.mp4", 2);

        let mut project = Project::default();
        let item = |path: std::path::PathBuf| MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        };
        let media_a = project.media_pool.insert(item(path_a));
        let media_b = project.media_pool.insert(item(path_b));

        let clip_a = media_clip(1, media_a, 0, 30); // timeline [0,30)
        let clip_b = media_clip_trimmed(2, media_b, 30, 5, 30); // timeline [30,60), source_in=5

        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![clip_a, clip_b],
            muted: false,
            solo: false,
            locked: false,
            crossings: vec![vv_core::CrossTransition {
                left_clip: ClipId(1),
                right_clip: ClipId(2),
                transition: vv_core::Transition {
                    kind: vv_core::TransitionKind::Push,
                    duration: 16,
                    direction: vv_core::PushDirection::Right,
                    ease: vv_core::Ease::None,
                    curve: 0.0,
                },
            }],
        }]));
        let timeline = project.timelines.get(timeline_id).unwrap().clone();

        let mut render_ahead = RenderAhead::spawn(
            project.clone(),
            timeline_id,
            100_000_000,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );

        // Crossing window (duration=16, split 8/8 around the
        // cut at 30): [22,38). It covers a good margin before and after.
        let mut missing = Vec::new();
        for frame in 15..45 {
            render_ahead.set_target(frame);
            let start = std::time::Instant::now();
            let (mut ok, mut expected);
            loop {
                let clips = timeline.active_video_clips_at(frame);
                ok = 0;
                expected = 0;
                for (track_index, clip) in &clips {
                    let involved = match timeline.tracks[*track_index].crossing_at(frame) {
                        Some((left, right, _)) if left.id == clip.id || right.id == clip.id => 2,
                        _ => 1,
                    };
                    expected += involved;
                    let layers = crate::frame_provider::track_layers_at(
                        &project,
                        &timeline,
                        *track_index,
                        clip,
                        frame,
                        (320, 240),
                        &mut render_ahead,
                    )
                    .unwrap();
                    ok += layers.len();
                }
                if ok >= expected || start.elapsed() > Duration::from_secs(5) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if ok < expected {
                missing.push((frame, ok, expected));
            }
        }
        assert!(missing.is_empty(), "frame con layer mancanti (frame, ok, expected): {missing:?}");
    }

    /// End-to-end reproduction (real worker thread, not a direct
    /// `walk_and_fill`) of the original scenario reported by the user: cut
    /// a clip, place the playhead *still* just before the cut
    /// point (lookahead window including a piece of both
    /// halves, two segments of the same media). With the playhead really
    /// still for several real poll cycles (not just two direct
    /// calls to `walk_and_fill` as in the equivalent unit test), the
    /// buffer must converge and stay stable — not recompute itself nor
    /// shrink repeatedly.
    #[test]
    fn render_ahead_does_not_loop_when_the_playhead_sits_still_just_before_a_cut() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "stationary_before_cut.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        // Cut at timeline_start=50 between two *contiguous* pieces of the
        // same file (a plain split, not a trim with a hole in between):
        // source [0,50) and then [50,150).
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip_trimmed(1, media_a, 0, 0, 50),
                media_clip_trimmed(2, media_a, 50, 50, 100),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
        render_ahead.set_target(40);

        // Waits for the buffer to reach at least the cut.
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if ranges.iter().any(|&(s, e)| s <= 40 && e >= 50) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout: il buffer non ha mai raggiunto il taglio: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Playhead still for a handful of real poll cycles (50ms
        // each): if there were a compute/invalidate loop, the buffered
        // interval around the playhead would disappear and reappear
        // repeatedly instead of simply staying stable (or
        // growing forward, never shrinking from behind the playhead).
        let mut samples = Vec::new();
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(50));
            samples.push(render_ahead.cached_ranges_for(media_a));
        }
        for (i, ranges) in samples.iter().enumerate() {
            assert!(
                ranges.iter().any(|&(s, e)| s <= 40 && e >= 50),
                "campione {i}: il buffer attorno alla testina è sparito con la testina ferma: {ranges:?}"
            );
        }
    }

    /// plans/REFACTOR_PIPELINE.md §3.3: a freshly opened `OpenDecoder` has no
    /// observations yet, so it uses the default fallback.
    #[test]
    fn open_decoder_seek_threshold_uses_the_default_fallback_before_any_observation() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_fresh.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let od = OpenDecoder::fresh(decoder, path.clone(), false);
        assert_eq!(od.seek_threshold_frames(), DEFAULT_SEEK_THRESHOLD_FRAMES);
    }

    /// Two consecutive seek landings update the GOP estimate
    /// to the distance observed between them.
    #[test]
    fn open_decoder_records_the_observed_gap_between_two_consecutive_landings() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_observed.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), false);

        od.record_keyframe_landing(25);
        od.record_keyframe_landing(50);

        assert_eq!(od.seek_threshold_frames(), 25);
    }

    /// The estimate is a *minimum*: an accidental jump of several GOPs at
    /// once (here a distance of 200 after one of 25) must not make
    /// the threshold rise — only a *tighter* observation
    /// narrows it further, never the opposite.
    #[test]
    fn open_decoder_gop_estimate_never_grows_from_a_wider_observation() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_min.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), false);

        od.record_keyframe_landing(0);
        od.record_keyframe_landing(25); // distance 25: estimate = 25
        assert_eq!(od.seek_threshold_frames(), 25);

        od.record_keyframe_landing(225); // distance 200: must not rise to 200
        assert_eq!(
            od.seek_threshold_frames(),
            25,
            "un salto più largo di uno già osservato non deve far crescere la stima"
        );

        od.record_keyframe_landing(235); // distance 10: must narrow
        assert_eq!(od.seek_threshold_frames(), 10);
    }

    /// Without the cap (`MAX_SEEK_THRESHOLD_FRAMES`), the *first*
    /// observation alone could blow up the threshold if two
    /// seeks happen to land many GOPs apart before a tighter
    /// one arrives.
    #[test]
    fn open_decoder_gop_estimate_is_capped_even_on_the_first_observation() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_cap.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), false);

        od.record_keyframe_landing(0);
        od.record_keyframe_landing(10_000);

        assert_eq!(od.seek_threshold_frames(), MAX_SEEK_THRESHOLD_FRAMES);
    }

    /// Regression: a fast and monotonic scrub (target always further
    /// ahead, never a close landing) on a proxy must not leave
    /// `seek_threshold_frames` stuck on an estimate as wide as
    /// for the real source (`PROXY_SEEK_THRESHOLD_FRAMES` bypasses the
    /// estimate entirely, see its docs) — this is exactly the scenario
    /// diagnosed with `VV_DEBUG_RENDER_AHEAD=1`: without the bypass, every
    /// worker cycle stayed stuck 15-60ms decoding in
    /// sequence instead of seeking (almost free on an all-intra proxy),
    /// more than the time between two ticks of a fast scrub.
    #[test]
    fn open_decoder_ignores_the_learned_gop_estimate_for_an_all_intra_proxy() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_proxy_bypass.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), true);
        assert_eq!(od.seek_threshold_frames(), PROXY_SEEK_THRESHOLD_FRAMES);

        // Wide landings, never close together, as during a fast and
        // monotonic scrub: for a "normal" decoder the estimate
        // would converge on a large value (the minimum observed so far,
        // here 90) instead of narrowing towards the real GOP.
        od.record_keyframe_landing(90);
        od.record_keyframe_landing(180);
        od.record_keyframe_landing(270);

        assert_eq!(
            od.seek_threshold_frames(),
            PROXY_SEEK_THRESHOLD_FRAMES,
            "un proxy all-intra non deve mai usare la soglia imparata, qualunque atterraggio osservi"
        );
    }

    /// Regression: a real seek for an already open media must reuse
    /// the existing decoder (`seek_to_time`), not throw it away to
    /// reopen the file from scratch — for a large file not optimized for
    /// streaming, reopening means reparsing the whole index every
    /// time (seconds, even), and if that cost exceeds the tolerance the
    /// target moves further during the opening itself, triggering
    /// another one on the next round: a loop that never recovers
    /// (observed: one frame every few seconds). Verified by the return
    /// value: `Seeked` (reuse) instead of `Opened` (reopen) on the
    /// second call on the same path.
    ///
    /// Note: here the path is the same on both calls on
    /// purpose — a *different* path for the same media_id now forces
    /// a reopen even at the same position (see
    /// `position_decoder_reopens_when_the_resolved_path_changes_even_without_a_seek`,
    /// the proxy becoming available mid-session needs
    /// exactly this).
    #[test]
    fn position_decoder_reuses_the_open_decoder_for_a_real_seek_instead_of_reopening_the_file() {
        let path = make_test_clip("vv-app-render-ahead-test", "reuse.mp4", 3);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Opened
        );

        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 1000, false, false, false),
            Positioned::Seeked,
            "un seek reale su un media già aperto deve riusare il decoder, non riaprirlo"
        );
    }

    /// plans/REFACTOR_PIPELINE.md proxy: a proxy becoming available in the
    /// background (or the "use proxy" toggle changing) makes a
    /// different path resolve for the same media_id — the decoder open on the
    /// old path makes no sense to reuse/seek (it points at a
    /// different file), it must be reopened from scratch even if the requested position
    /// would otherwise be "close enough" not to justify a
    /// seek.
    #[test]
    fn position_decoder_reopens_when_the_resolved_path_changes_even_without_a_seek() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "swap_a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "swap_b.mp4", 2);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path_a, 0, false, false, false),
            Positioned::Opened
        );

        // Same requested position (0): without the check on the resolved
        // path, `needs_seek` would be `false` (0 is not "too far ahead"
        // relative to a freshly opened decoder) and the call
        // would return `Reused` — reusing a decoder pointing at the
        // wrong file.
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path_b, 0, false, false, false),
            Positioned::Opened,
            "il path è cambiato: deve riaprire sul nuovo, non riusare il decoder del vecchio"
        );
    }

    /// Regression for a real bug, confirmed by the user: `next_frame`
    /// is only what the decoder *believes* it has already produced, not a
    /// guarantee that it is still cached — an eviction between one cycle
    /// and the next (`reconcile`, or a tight budget during an earlier
    /// fill) may have removed the tail the decoder thinks it
    /// already has behind it. Here exactly this is simulated: a
    /// decoder "advanced" with `next_frame` past the target, but with the
    /// content next_frame assumes it covered removed by hand
    /// from the cache (as a real `reconcile` would) — `position_decoder`
    /// must notice and force a real seek, not trust
    /// `next_frame` and return `Reused` over a hole.
    #[test]
    fn position_decoder_reseeks_when_next_frame_claims_coverage_the_cache_no_longer_has() {
        let path = make_test_clip("vv-app-render-ahead-test", "stale_next_frame.mp4", 3);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Opened
        );
        // Decodes and caches a few frames, as
        // walk_and_fill would — the decoder now "believes" it is ahead with
        // that whole stretch genuinely cached behind it.
        for _ in 0..20 {
            let od = open.get_mut(&media_a).unwrap();
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    caches.insert(media_a, idx, frame);
                    od.next_frame = idx + 1;
                }
                _ => break,
            }
        }
        let advanced_next_frame = open.get(&media_a).unwrap().next_frame;
        assert!(
            advanced_next_frame > 5,
            "il decoder deve aver avanzato di parecchio"
        );

        // Simulates a real eviction: `reconcile` with a window that
        // deliberately excludes a single frame in the middle of what
        // `next_frame` assumes covered — the cache loses that frame without
        // the decoder knowing anything about it (exactly what
        // a tight budget or a shrinking window would do).
        let gap_at = advanced_next_frame - 3;
        let window = [
            WantedRange {
                media_id: media_a,
                source_start: 0,
                source_end: gap_at - 1,
                timeline_start: 0,
                rate: vv_core::Rational::one(),
            },
            WantedRange {
                media_id: media_a,
                source_start: gap_at + 1,
                source_end: advanced_next_frame - 1,
                timeline_start: gap_at + 1,
                rate: vv_core::Rational::one(),
            },
        ];
        caches.reconcile(0, &window, usize::MAX);

        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Seeked,
            "un buco lasciato da uno sfratto dietro a next_frame deve forzare un seek reale, \
             non un Reused che lo lascia scoperto per sempre"
        );
    }

    /// Regression for the reported bug: during normal playback the
    /// decoder is almost always *ahead* of the target (it is the healthy
    /// state of a buffer working well). Before the fix,
    /// `position_decoder` read this as "too far behind" and
    /// reopened the file with a real seek on every poll cycle,
    /// invalidating the work just done — hence the indicator
    /// "going in circles" without ever advancing steadily.
    #[test]
    fn position_decoder_does_not_reseek_when_already_usefully_ahead_of_the_segment_start() {
        let path = make_test_clip("vv-app-render-ahead-test", "steady.mp4", 3);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Opened
        );

        // Decodes a few frames forward "by hand" *and* inserts them into
        // the cache, as walk_and_fill would, to simulate a decoder already
        // buffered past the current target — since the coverage recheck
        // in `position_decoder` (see its docs), an
        // advanced `next_frame` without cached content behind it
        // would no longer be enough to avoid a seek.
        for _ in 0..20 {
            let od = open.get_mut(&media_a).unwrap();
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    caches.insert(media_a, idx, frame);
                    od.next_frame = idx + 1;
                }
                _ => break,
            }
        }
        let advanced_next_frame = open.get(&media_a).unwrap().next_frame;
        assert!(advanced_next_frame > 0, "il decoder deve aver avanzato");

        // A later cycle with the target still behind the decoder's
        // position — the normal state during forward playback —
        // must not reopen/reset the decoder.
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Reused
        );
        assert_eq!(
            open.get(&media_a).unwrap().next_frame,
            advanced_next_frame,
            "non deve aver riaperto il decoder mentre è ancora utilmente avanti"
        );
    }

    /// `WalkOutcome::caught_up` is the basis of `RenderAhead::is_caught_up`,
    /// which the UI uses to decide whether it is worth requesting another
    /// repaint (see the docs there): with a budget ample enough for
    /// the whole lookahead window, one cycle must be enough to cover it
    /// all and signal so.
    #[test]
    fn walk_and_fill_reports_caught_up_when_the_whole_window_fits_the_budget() {
        let path = make_test_clip("vv-app-render-ahead-test", "caught_up.mp4", 3);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 75,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 75)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 4 * 200; // well past the 75 frames of the window
        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            generous_budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );

        assert!(
            outcome.caught_up,
            "budget e finestra coprono tutta la clip: non dovrebbe restare altro da fare"
        );
        assert!(!outcome.interrupted);
    }

    /// The retention window behind the playhead (`behind_secs`) is not
    /// just "do not discard what is already there": on a zone *never visited
    /// before* it must really be decoded, not only retained if
    /// already present — otherwise a scrub in a new zone shortly after
    /// the start of the clip would have nothing to retain behind it.
    #[test]
    fn walk_and_fill_decodes_the_behind_window_on_a_fresh_area() {
        let path = make_test_clip("vv-app-render-ahead-test", "fresh_behind.mp4", 4);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Generous budget: no capacity eviction to confuse the
        // result, here only "does it get decoded" or not matters.
        let generous_budget = 320 * 240 * 3 / 2 * 200;

        // First time this zone is seen: playhead at 60, never
        // anywhere else before (`went_backward` irrelevant on the first
        // cycle).
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            60,
            generous_budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(60),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 40 && e >= 59),
            "il tratto dietro la testina [40,59] (dentro behind_secs) deve essere stato decodificato, non solo trattenuto se già presente: {ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(s, e)| s <= 60 && e >= 99),
            "la finestra in avanti deve comunque essere coperta normalmente: {ranges:?}"
        );
    }

    /// Regression reported by the user: during a backwards scrub
    /// the frame useful *right away* is the one adjacent to the playhead (the near
    /// edge of the window behind), not the one on the far edge — but
    /// decoding a whole segment behind in a single seek (as for
    /// the forward window) produces the frames in the wrong order:
    /// ffmpeg decodes only forwards from `source_start` (far) towards
    /// `source_end` (near), so the most useful frame arrives
    /// last. With a budget covering the forward window (small,
    /// near the end of the clip) plus only the first chunk of the
    /// window behind (`BEHIND_CHUNK_FRAMES`), the frame adjacent to the
    /// playhead must still be cached, the one on the far edge
    /// not — direct proof that `chunk_behind_segments_near_to_far`
    /// really reorders the decoding priority, not just on paper.
    #[test]
    fn walk_and_fill_decodes_the_behind_window_nearest_frames_first_under_a_tight_budget() {
        // GOP=1 (every frame a keyframe, like a proxy): a seek lands
        // exactly where requested, so the budget needed for each
        // chunk is predictable in exact frames — with a long GOP (a
        // "normal" video) the seek would land on the keyframe nearest
        // *before* the target, making the computation below fragile
        // without adding anything to what is under test (the priority
        // order of the chunks, not how much it costs to get there).
        let path =
            make_test_clip_with_short_gop("vv-app-render-ahead-test", "behind_priority.mp4", 4, 1);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Playhead at 95: tiny forward window (only [95,99], the
        // clip ends at 100), normal window behind (2s = 50 frames,
        // [45,94]). Budget for the forward window (5 frames) plus *only*
        // the first chunk of the window behind (`BEHIND_CHUNK_FRAMES`
        // = 15 frames, [80,94]) — not enough to reach the far
        // edge at 45.
        let frame_bytes = 320 * 240 * 3 / 2;
        let tight_budget = frame_bytes * (5 + 15);
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            95,
            tight_budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(95),
        );

        assert!(
            caches.contains(media_a, 94),
            "il frame adiacente alla testina (bordo vicino della finestra dietro) deve essere \
             tra i primi decodificati, quindi in cache anche con un budget stretto"
        );
        assert!(
            !caches.contains(media_a, 45),
            "il frame sul bordo lontano della finestra dietro non deve essere raggiunto prima \
             di quelli vicini alla testina, con un budget che copre solo il primo blocco"
        );
    }

    /// Regression: with the playhead *still* (no change between two cycles), a
    /// second `walk_and_fill` on an already entirely filled window behind
    /// must not touch the decoder — see the docs of
    /// `without_already_cached_chunks`. Without the filter, the order from the
    /// nearest chunk to the farthest means the decoder, at the start of a
    /// cycle, is always positioned *behind* the first requested chunk
    /// (it had stopped where the farthest chunk of the previous cycle
    /// ended), so every chunk would be reseeked and at least one frame
    /// thrown away again — on every single cycle, forever, even without
    /// any scrub going on. A real seek involves the
    /// `ffmpeg` process/the container: even one costs orders of magnitude
    /// more than a round of in-memory `cached_ranges` checks, so a
    /// tight time cap on the second round reliably distinguishes
    /// "it did not touch the decoder" from "it redid work".
    #[test]
    fn walk_and_fill_does_not_reseek_an_already_complete_behind_window_when_idle() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "behind_idle_stability.mp4",
            4,
            1,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 3 / 2 * 200;

        // First round: fills entirely both ahead and behind (several
        // chunks, [45,94] at 25fps/2s).
        let first = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            95,
            generous_budget,
            false,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(95),
        );
        assert!(
            first.caught_up,
            "il primo giro deve completare la finestra: {first:?}"
        );

        // 20 later rounds, same playhead, nothing changes: they must
        // complete almost instantly (no real seek, only
        // in-memory `cached_ranges` checks). A single round is not
        // a stable enough measure (OS scheduling jitter
        // of a few ms can happen even without any real
        // work) — summing 20 independent rounds amplifies the
        // signal: if even one touches the decoder, the cost of a
        // real seek+decode (1-2ms, already measured elsewhere in this
        // file) dominates the total, while 20 rounds of in-memory checks
        // only stay in the hundreds of µs.
        let start = std::time::Instant::now();
        for _ in 0..20 {
            let outcome = walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                95,
                generous_budget,
                false,
                None,
                DEFAULT_LOOKAHEAD_SECS,
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(95),
            );
            assert!(outcome.caught_up);
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(20),
            "20 giri a testina ferma non devono toccare il decoder (nessun seek reale): \
             impiegati {elapsed:?} in totale, attesi <20ms"
        );
    }

    /// `lookahead_secs`/`behind_secs` configured to `0`: the window
    /// shrinks to the minimum margin (`MIN_MARGIN_FRAMES`), not to zero — even
    /// with a generous budget and a zone never seen before (which with a
    /// normal window would trigger both the forward window and
    /// the retention one, see the test above).
    #[test]
    fn walk_and_fill_buffers_only_a_minimal_margin_when_configured_to_zero_seconds() {
        let path = make_test_clip("vv-app-render-ahead-test", "no_read_ahead.mp4", 4);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 3 / 2 * 200;

        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            60,
            generous_budget,
            false,
            None, // proxy: irrelevant for this test
            0.0,   // lookahead_secs: the one under test
            0.0,   // behind_secs: the one under test
            &AtomicI64::new(60),
        );

        // [56,63], not a wider interval: with the default keyint
        // (250) and a clip of only 100 frames, the only keyframe is at 0 —
        // reaching frame 63 (= 60 + MIN_MARGIN_FRAMES) requires
        // decoding in sequence from there, and those *transit* frames
        // stay cached as a by-product DURING the round (see the docs
        // of `transit_bytes` in `fill_segments`: it serves to make the
        // chunk behind [56,59] find them already there instead of
        // crossing the same GOP from scratch again), but a final `reconcile`
        // discards them again before `walk_and_fill` declares
        // `caught_up` (see the comment there on why: otherwise the UI,
        // which stops requesting repaints on `caught_up`, can stay
        // stuck showing that transit as if it were "buffered" until
        // the next repaint for other reasons — bug reported
        // by the user, the strip "resizes itself" only by moving the
        // mouse). The reuse *between* the two segments of this same round has
        // already happened before this final `reconcile`, only
        // its survival past the end of the round is lost.
        assert_eq!(
            caches.cached_ranges(media_a),
            vec![(60 - MIN_MARGIN_FRAMES, 60 + MIN_MARGIN_FRAMES - 1)],
            "configurato a zero secondi la finestra non deve estendersi oltre il margine minimo"
        );
        assert!(
            outcome.caught_up,
            "una finestra minima, già coperta, deve risultare caught_up"
        );
    }

    /// Regression: if the budget is not enough to cover the whole lookahead
    /// window, the buffer must still start from the playhead (the
    /// nearest frames, the most useful to show right away) and not from an
    /// arbitrary tail of the window — otherwise the indicator shows
    /// an interval that "falls after" the playhead without ever covering it.
    #[test]
    fn walk_and_fill_prioritizes_frames_near_the_playhead_when_the_budget_is_too_small_for_the_full_window()
     {
        let path = make_test_clip("vv-app-render-ahead-test", "small_budget.mp4", 3);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 75,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 75)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Tiny budget: the lookahead window (3s = 75 frames at
        // 25fps) does not fit entirely in the cache.
        let tiny_budget = 320 * 240 * 4 * 5;
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            tiny_budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(!ranges.is_empty());
        assert_eq!(
            ranges[0].0, 0,
            "il buffer deve partire dalla testina, non da una coda arbitraria: {ranges:?}"
        );
        assert!(
            ranges[0].1 < 74,
            "con un budget così piccolo non deve riuscire a coprire tutta la finestra: {ranges:?}"
        );
    }

    /// plans/REFACTOR_PIPELINE.md §3.1 (responsiveness): if the *live* target has
    /// already moved past the fallback threshold (no GOP observation
    /// made for this media yet) relative to `from_frame`
    /// before even starting, the fill must notice at the first
    /// opportunity (after the first decoded frame) and stop
    /// returning `true`, instead of continuing to decode for the whole
    /// window a prefetch that is by now obsolete.
    #[test]
    fn walk_and_fill_stops_early_and_reports_true_when_the_live_target_has_already_drifted() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "reactivity_drift.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // The live target is already past the threshold relative to from_frame=0 before
        // the fill even starts: it simulates the playhead having jumped
        // elsewhere while this cycle was about to start.
        let drifted_target = AtomicI64::new(DEFAULT_SEEK_THRESHOLD_FRAMES + 200);

        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            100_000_000,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &drifted_target,
        );
        assert!(
            outcome.interrupted,
            "deve segnalare l'interruzione al chiamante (worker_loop) per farlo ripartire subito"
        );
        assert!(
            !outcome.caught_up,
            "interrotto: non ha potuto verificare se la finestra fosse coperta"
        );

        let ranges = caches.cached_ranges(media_a);
        let decoded_frames: FrameIdx = ranges.iter().map(|&(s, e)| e - s + 1).sum();
        assert!(
            decoded_frames < 10,
            "deve fermarsi dopo pochissimi frame, non decodificare l'intera finestra ormai obsoleta: ranges={ranges:?}"
        );
    }

    /// Regression for the bug reported by the user: two clips on the
    /// timeline sharing the same media (a single file cut
    /// into several pieces, very common) generate two `MediaSegment`s for the
    /// same `media_id` in the same window, with different `source_start`s.
    /// Calling `evict_before` with the `source_start` of the
    /// *single* segment being processed (as was done before)
    /// discarded, while processing the second segment, everything the first
    /// had just decoded — "the buffer recomputes itself from scratch
    /// invalidating the following frames" reported by the user,
    /// reproducible at every cut between two pieces of the same file.
    #[test]
    fn walk_and_fill_does_not_invalidate_one_segment_while_processing_another_segment_of_the_same_media()
     {
        let path =
            make_test_clip_with_short_gop("vv-app-render-ahead-test", "same_media_cut.mp4", 20, 25);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        // Cut at timeline_start=60 between two pieces of the same file:
        // the first uses source [0,60), the second restarts from a
        // point much further into the source [200,300) — exactly
        // like cutting away a middle part of the same file.
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip_trimmed(1, media_a, 0, 0, 60),
                media_clip_trimmed(2, media_a, 60, 200, 100),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 100_000_000;

        // The lookahead window (3s = 75 frames at 25fps) from 40
        // crosses the cut at 60, including a piece of both
        // clips in the same cycle.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            40,
            budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(40),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 40 && e >= 59),
            "il tratto della prima clip [40,59] non deve essere sfrattato dall'elaborazione della seconda: ranges={ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(s, e)| s <= 200 && e >= 200),
            "la seconda clip deve comunque essere bufferizzata, non solo attraversata: ranges={ranges:?}"
        );
    }

    /// Regression for the bug reported by the user ("the caching must not
    /// be per clip but per timeline"): the previous test uses a
    /// huge budget (100MB) that never puts the real capacity of the
    /// cache under pressure, so it does not catch it. With a tight
    /// budget, two segments of the same media in this window (a
    /// cut with a discarded part in between: sources far from
    /// each other) asked *together* for more frames than the shared
    /// `FrameCache` could hold — the second segment processed
    /// (`[200,254]`) evicted by capacity limit (ordinary LRU,
    /// not `evict_before`) everything the first (`[40,59]`) had
    /// just decoded in the very same cycle, even though
    /// `evict_before` alone would have protected it. As seen by the user:
    /// every clip seems to buffer "on its own", at the expense of the
    /// others — hence "the caching looks per-clip, not per-timeline".
    #[test]
    fn walk_and_fill_does_not_let_one_segment_of_a_media_evict_another_via_capacity_when_the_budget_is_tight()
     {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "same_media_cut_tight_budget.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        // Same cut as the previous test: [0,60) then [200,300).
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip_trimmed(1, media_a, 0, 0, 60),
                media_clip_trimmed(2, media_a, 60, 200, 100),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Capacity ~60 YUV420 frames (width*height*3/2 bytes/frame):
        // less than what the two segments together would ask for (~20 + ~55),
        // but more than what each asks for alone — it forces the
        // sharing of the same cache to really count.
        let budget = 60 * 320 * 240 * 3 / 2;

        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            40,
            budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(40),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 40 && e >= 59),
            "il tratto della prima clip [40,59] non deve sparire per colpa del secondo segmento nello stesso ciclo: ranges={ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(s, _)| s <= 200),
            "la seconda clip deve comunque ricevere una fetta della capacità condivisa: ranges={ranges:?}"
        );
        assert!(
            !outcome.caught_up,
            "budget saturo prima di finire la finestra: non è \"caught up\", c'è ancora lavoro per il prossimo ciclo"
        );
    }

    /// Regression for the bug reported by the user: with the playhead
    /// still just before a cut (it is enough to cut a clip and
    /// place the playhead just before the cut point: the
    /// lookahead window includes a piece of both
    /// halves anyway, two segments of the same media), every poll cycle
    /// reprocesses the same two segments in the same order: the second
    /// segment must not make the first look like it "went backwards" and
    /// trigger a real seek on every cycle while standing still (see the docs
    /// of `position_decoder` on `went_backward`). Verified by passing
    /// `false` (playhead still) to both segments in both cycles.
    ///
    /// Gap between the two segments (10 and 25) chosen on purpose below
    /// `DEFAULT_SEEK_THRESHOLD_FRAMES`: here `position_decoder` is
    /// called directly, without ever decoding a real frame, so
    /// no GOP observation ever happens and the threshold stays at the
    /// fallback for the whole test — a wider gap would legitimately trigger
    /// the "too far ahead" branch, masking the thing
    /// this test wants to isolate (the contamination between segments of the
    /// same media, not that threshold).
    #[test]
    fn position_decoder_does_not_reseek_across_cycles_when_the_same_media_appears_in_two_segments()
    {
        let path = make_test_clip("vv-app-render-ahead-test", "same_media_two_segments.mp4", 3);
        let (media_a, _) = two_media_ids();
        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();

        // Cycle 1: two segments of the same media in the same window
        // (as on the two sides of a cut), source_start 10 and then 25.
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 10, false, false, false),
            Positioned::Opened
        );
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 25, false, false, false),
            Positioned::Reused,
            "nello stesso ciclo il secondo segmento non deve mai richiedere un seek: il decoder è già lì"
        );

        // Cycle 2, playhead still (`went_backward=false` for both):
        // the same two segments. Reprocessing the *first* segment (10) must
        // not look like it "went backwards" just because the last call
        // seen in the previous cycle was for the following segment (25).
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 10, false, false, false),
            Positioned::Reused,
            "testina ferma: rielaborare il primo segmento non deve scatenare un seek reale"
        );
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 25, false, false, false),
            Positioned::Reused
        );
    }

    /// Regression (ex-A2, plans/REFACTOR_PIPELINE.md): with the old
    /// architecture (one `FrameCache` per media, capacity fixed at
    /// creation time) this test checked that the capacity
    /// of `media_b` widened when `media_a` left the window
    /// — a problem that with the globally budgeted `SharedFrameCache` (§2)
    /// can no longer arise *by construction*: there is no longer a
    /// per-media capacity to keep in sync, the budget is a single one
    /// and always the real one. So it checks the direct equivalent: with
    /// fewer media contending for the budget, `media_b` gets to
    /// buffer *more* frames (no longer capacity, but real coverage).
    #[test]
    fn walk_and_fill_buffers_more_of_a_media_once_fewer_distinct_media_share_the_budget() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "resize_a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "resize_b.mp4", 15);

        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path: path_a,
            meta: MediaMeta {
                duration_frames: 40,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let media_b = project.media_pool.insert(MediaItem {
            path: path_b,
            meta: MediaMeta {
                duration_frames: 375,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 40),   // [0,40)
                media_clip(2, media_b, 40, 400), // [40,440)
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // A budget that at 320x240 (307_200 B/frame) is enough for ~60 frames
        // in total: with two media contending for the window few fit
        // each, with only one many more.
        let total_budget = 60 * 320 * 240 * 4;

        let frames_cached = |ranges: &[(FrameIdx, FrameIdx)]| -> FrameIdx {
            ranges.iter().map(|&(s, e)| e - s + 1).sum()
        };

        // First cycle: the lookahead window (3s = 75 frames) crosses
        // the cut at 40, so media_a and media_b are both in the
        // window and share the same global budget.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            total_budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );
        let frames_b_shared = frames_cached(&caches.cached_ranges(media_b));

        // Second cycle: the target is well past the cut, only media_b is
        // in the window — reconcile discards media_a (Tier A), so
        // media_b has the whole global budget to itself, without needing
        // any separate capacity to "resize upwards".
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            200,
            total_budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(200),
        );
        let ranges = caches.cached_ranges(media_b);
        let frames_b_alone = frames_cached(&ranges);
        assert!(
            frames_b_alone > frames_b_shared,
            "con un solo media nella finestra deve arrivare a bufferizzarne di più, non restare fermo alla quota di quando la condivideva: {frames_b_shared} -> {frames_b_alone}"
        );

        // source_start for the second cycle: clip b has source_in=0,
        // timeline_start=40, hence 200-40=160.
        assert!(
            ranges.iter().any(|&(s, _)| s <= 160),
            "con più budget disponibile il buffer deve poter partire dalla nuova testina, non da una coda arbitraria più avanti: {ranges:?}"
        );
    }

    /// Regression: after a scrub far back relative to where the
    /// worker had already buffered forward, the buffer must
    /// reach the new position too — the decoder can only
    /// decode forwards, so without an explicit reopen
    /// it would stay stuck past the new target forever.
    #[test]
    fn render_ahead_catches_up_after_a_large_backward_seek() {
        let path = make_test_clip("vv-app-render-ahead-test", "backward_seek.mp4", 4);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
        render_ahead.set_target(80);

        let start = std::time::Instant::now();
        loop {
            if !render_ahead.cached_ranges_for(media_a).is_empty() {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout in avanti"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Scrub back past the seek threshold: the decoder that
        // buffered around 80 cannot continue forwards to
        // reach 0.
        render_ahead.set_target(0);
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if ranges.iter().any(|&(s, _)| s == 0) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout indietro: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Regression for the bug reported by the user: a backwards
    /// scrub must regenerate the buffer for the new position,
    /// not stay stuck on the nearest cached frame. The scrub here
    /// (70 frames) is chosen on purpose *past* the retention window
    /// behind the playhead (`DEFAULT_BEHIND_SECS`, 2s = 50 frames at 25fps): a
    /// real scrub past that window must still behave as
    /// before that window existed — regenerate from scratch — because here there
    /// is nothing to reuse. A scrub *inside* the window, on the other hand, must
    /// not regenerate anything by construction (see
    /// `walk_and_fill_does_not_redecode_the_already_buffered_tail_after_a_small_backward_seek`,
    /// which checks exactly that). Before the original fix of
    /// this regression, it stayed stuck on the nearest cached
    /// frame because `position_decoder` considered "far enough ahead"
    /// any target still behind `next_frame` by more than a threshold —
    /// but `walk_and_fill` discards on every cycle everything outside
    /// the current window, so even a small step back
    /// past the retention window falls into already discarded territory and
    /// is unreachable by decoding only forwards.
    #[test]
    fn render_ahead_catches_up_after_a_backward_seek_beyond_the_retention_window() {
        let path = make_test_clip("vv-app-render-ahead-test", "small_backward_seek.mp4", 6);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 150,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 150)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
        render_ahead.set_target(100);

        // Wait not only for the buffer to cover 100, but for the later poll
        // cycles to have also already discarded what is left
        // outside the window (forward+behind) of 100 — including 30, which
        // is 70 frames behind, past the 50 of the retention window.
        // Otherwise the test would pass by accident, because the first
        // fill (which decodes from the nearest keyframe, here
        // the start of the file) already includes 30 before it is even
        // discarded.
        let covers = |ranges: &[(FrameIdx, FrameIdx)], f: FrameIdx| {
            ranges.iter().any(|&(s, e)| s <= f && f <= e)
        };
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if covers(&ranges, 100) && !covers(&ranges, 30) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout in avanti: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Scrub back by 70 frames (past the retention window of
        // 50): it must still regenerate the buffer for the new
        // position, not stay stuck on the nearest frame already
        // cached. Coverage of 30 (not "a range starting exactly
        // there"): the seek lands on the keyframe nearest to 30, which may
        // be even before 30 itself.
        render_ahead.set_target(30);
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if covers(&ranges, 30) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout su scrub indietro: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Regression for the bug reported by the user: stuttering playback
    /// even at 1x with `lookahead_secs`/`behind_secs` at `0`, because
    /// `set_target` updated only an atomic read by the worker at most every
    /// `POLL_INTERVAL` (50ms) — a real cap at ~20 frames/sec
    /// regardless of how fast the decoding was (it happened
    /// with the proxies too).
    ///
    /// It checks the immediate wake-up alone (`Command::Wake`), isolated from the
    /// minimum margin: one isolated jump at a time, with a deadline
    /// of 40ms *after* waiting for the worker to settle on the
    /// previous target. Without the immediate wake-up the wait would be
    /// uniform between 0 and 50ms: each of the 11 jumps stays under 40ms by
    /// chance 80% of the time, all together ~9%. The deadline is no tighter
    /// because under the load of the other tests in parallel even
    /// decoding the frame can overrun.
    #[test]
    fn render_ahead_reacts_to_each_target_change_faster_than_the_old_poll_interval() {
        let path = make_test_clip("vv-app-render-ahead-test", "wake_on_change.mp4", 2);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 50)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(project, timeline_id, 100_000_000, None, 0.0, 0.0);

        let wait_for = |frame: FrameIdx| {
            let start = std::time::Instant::now();
            loop {
                if render_ahead.get_frame(media_a, frame).is_some() {
                    return start.elapsed();
                }
                assert!(
                    start.elapsed() < Duration::from_millis(40),
                    "frame {frame} non pronto entro una scadenza compatibile con la sveglia immediata"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };

        // First target: no tight deadline, the worker only has to
        // start up (opening the decoder included).
        render_ahead.set_target(0);
        loop {
            if render_ahead.get_frame(media_a, 0).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // From here on, every isolated jump has 40ms to be ready.
        for target in (8..=48).step_by(4) {
            render_ahead.set_target(target);
            wait_for(target);
        }
    }

    /// General regression: after some forward playback over several
    /// cycles, a backwards scrub towards a position more recent than the
    /// very first target ever seen must still make the buffer
    /// recompute for the new position. It checks that `went_backward`,
    /// computed once per cycle (simulated here as
    /// `worker_loop` would), holds over a realistic sequence of cycles, not just a
    /// single isolated backwards jump.
    #[test]
    fn walk_and_fill_catches_up_after_a_backward_seek_above_the_historical_minimum() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "backward_above_historical_min.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Tight budget: the whole window does not fit in the cache, so
        // as it advances `evict_before` really discards the frames behind
        // the playhead instead of just leaving them still present by
        // chance.
        let budget = 43_000_000;

        // Forward playback over several cycles: `went_backward` is always
        // `false` (every target is >= the previous one), exactly as
        // `worker_loop` would compute it comparing `from` with the previous
        // cycle.
        let mut prev = None;
        for from in [0, 50, 100, 150, 200] {
            let went_backward = prev.is_some_and(|p| from < p);
            prev = Some(from);
            walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                from,
                budget,
                went_backward,
                None,                   // proxy: irrelevant for this test
                DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(from),
            );
        }

        // At this point the frames around 0 are certainly evicted
        // (evict_before discarded everything behind the playhead
        // on every cycle, the last of which is 200).
        let ranges_before = caches.cached_ranges(media_a);
        assert!(
            !ranges_before.iter().any(|&(s, e)| s <= 80 && e >= 80),
            "80 non deve essere già in cache per coincidenza, altrimenti il test non prova nulla: {ranges_before:?}"
        );

        // Scrub back to 80: further back than the current playhead (200),
        // but further ahead than the oldest target ever seen (0).
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            80,
            budget,
            true,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(80),
        );

        let ranges_after = caches.cached_ranges(media_a);
        assert!(
            ranges_after.iter().any(|&(s, e)| s <= 80 && e >= 80),
            "lo scrub indietro a 80 deve far ricalcolare il buffer per la nuova posizione: {ranges_after:?}"
        );
    }

    /// Checks the requested optimization: after a small backwards
    /// scrub, the already buffered portion still falling within
    /// the *new* window (forward + behind, see `behind_secs`) must
    /// not be re-decoded — indeed, the decoder must not be touched at
    /// all (`SharedFrameCache::covers`, checked *before* consulting it:
    /// see its docs on a real bug, confirmed in production, caused
    /// by trusting the position the decoder *believes* it has instead
    /// of asking the cache). With the retention window behind the
    /// playhead, the 30-frame scrub below falls entirely
    /// *inside* that window (50 frames): the stretch [250,269] is not
    /// even discarded by `reconcile`, so the whole requested segment
    /// comes out already covered and is skipped outright — `OpenDecoder::
    /// next_frame` must stay exactly where it was before this
    /// call, direct proof that no seek/decode happened.
    ///
    /// Note (plans/REFACTOR_PIPELINE.md §2, Tier A): with the globally budgeted
    /// `SharedFrameCache`, `reconcile` also discards what is *past*
    /// the horizon of the new window (here: past 344, given that the
    /// new playhead is 270) — unlike the old `evict_before`,
    /// which discarded only what was behind and left intact everything
    /// that was ahead, whatever the horizon. This is intended: the budget
    /// of the window is always exactly that of the current
    /// window, not an indefinite accumulation of historical tails. So here
    /// it only checks that [250,344] (the intersection between the old tail and
    /// the new window widened by the retention) is reachable without
    /// re-decoding it — not that the whole old tail up to 374
    /// survives.
    #[test]
    fn walk_and_fill_does_not_redecode_the_already_buffered_tail_after_a_small_backward_seek() {
        let path = make_test_clip("vv-app-render-ahead-test", "reconnect.mp4", 20);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 43_000_000; // capacity ~139 frames

        // Buffers around 300: with keyint=250 (libx264 default) the
        // decoder restarts from keyframe 250 and fills up to the capacity
        // limit.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            300,
            budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(300),
        );
        let filled_up_to = open.get(&media_a).unwrap().next_frame - 1;
        assert!(
            filled_up_to > 350,
            "il primo riempimento deve aver bufferizzato ben oltre 300 (fino all'orizzonte di lookahead): {filled_up_to}"
        );

        // Scrub back by only 30 frames: below the old threshold of
        // 120, but still a real backwards move (it must
        // reopen/reseek, `went_backward=true`).
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            270,
            budget,
            true,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(270),
        );

        let next_frame_after = open.get(&media_a).unwrap().next_frame;
        assert_eq!(
            next_frame_after,
            filled_up_to + 1,
            "il segmento richiesto era già interamente in cache: il decoder non doveva essere \
             toccato affatto, next_frame deve restare dov'era: next_frame={next_frame_after}"
        );

        // The intersection between the old tail and the new window
        // widened by the retention ([250,344]) must be reachable
        // as a contiguous range, without holes due to a wasted
        // re-decode. The fact that `filled_up_to` (374) is further ahead
        // than the horizon of the new window is expected: that part was
        // discarded by Tier A of `reconcile` because it is no longer in the
        // current window (see the note above), not because the
        // reconnection failed.
        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 250 && e >= 344),
            "l'intersezione [250,344] tra vecchia coda e nuova finestra deve restare un range contiguo: ranges={ranges:?} filled_up_to={filled_up_to}"
        );
    }

    /// Regression for the infinite loop reported by the user and
    /// diagnosed with `VV_DEBUG_RENDER_AHEAD` on a real file
    /// (1080p60fps, long GOP, proxy off): a small backwards
    /// scrub reconnects the decoder early (as in the test
    /// above) leaving `next_frame` parked *before* `source_start` —
    /// far enough back to exceed the adaptive threshold. Before the
    /// fix, every later cycle with the playhead *still* at the same
    /// position still saw `segment_start > next_frame + threshold`
    /// (the decoder position is never updated by a cycle
    /// that skips it), so it reseeked, re-decoded the same stretch
    /// already cached until reconnecting at the same point as before — a
    /// stable and infinite loop, never self-limiting. `SharedFrameCache::covers`
    /// prevents it by asking the cache *before* looking at the
    /// (presumed) position of the decoder.
    #[test]
    fn walk_and_fill_does_not_loop_forever_after_reconnecting_early_from_a_backward_seek() {
        let path = make_test_clip("vv-app-render-ahead-test", "reconnect_loop.mp4", 20);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 43_000_000; // capacity ~139 frames, as above

        // Same setup as the test above: initial fill at 300, then
        // a small scrub back to 290 that reconnects early.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            300,
            budget,
            false,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(300),
        );
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            290,
            budget,
            true,
            None,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(290),
        );

        // 20 later cycles, playhead still at 290 (no scrub): in the
        // original bug each one reseeked and re-decoded from scratch,
        // dominating the total time (real seek+decode, not just
        // in-memory checks) — same threshold/logic as the
        // stability-at-rest test above.
        let start = std::time::Instant::now();
        for _ in 0..20 {
            walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                290,
                budget,
                false,
                None,
                DEFAULT_LOOKAHEAD_SECS,
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(290),
            );
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(20),
            "20 cicli a testina ferma dopo un riaggancio anticipato non devono riseekare/\
             ridecodificare a ripetizione: impiegati {elapsed:?} in totale, attesi <20ms"
        );
    }

    /// Regression for a real bug, confirmed by the user with a
    /// diagnostic log on a 1080p60fps file: the reconnection check
    /// inside `fill_segments` checked two points (`next_frame` and
    /// `segment.source_end`) with two *independent* `contains` — if
    /// both happen by chance to fall in two separate cache islands (a
    /// hole between them, left by an earlier fill saturated on budget),
    /// the check passed anyway, making it believe the segment
    /// was already covered when in reality there was a hole right in the
    /// middle, never reached before nor after — permanent, because the
    /// decoder stopped there convinced it had finished.
    #[test]
    fn fill_segments_bridges_the_gap_between_two_disconnected_cached_islands() {
        // Explicit GOP=10: keyframes at 0,10,20,... — it only needs to make
        // one predictable near the start of the requested segment, no
        // other requirement on the distance between the two islands below.
        let path =
            make_test_clip_with_short_gop("vv-app-render-ahead-test", "bridge_gap.mp4", 4, 10);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        // `fill_segments` does not need a timeline: it already works at
        // the level of a resolved segment.

        let caches = SharedFrameCache::new();
        // Two disconnected islands, a real hole in between ([16,39], never
        // touched by anything so far) — as an earlier fill saturated
        // on budget would leave.
        for idx in 5..=15 {
            caches.insert(media_a, idx, Arc::new(dummy_frame()));
        }
        for idx in 40..=50 {
            caches.insert(media_a, idx, Arc::new(dummy_frame()));
        }

        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 5,
            source_end: 50,
            timeline_start: 5,
            rate: Rational::one(),
        };
        let ctx = FillContext {
            project: &project,
            caches: &caches,
            went_backward: false,
            cache_budget_bytes: usize::MAX,
            from_frame: 5,
            proxy: None,
            target: &AtomicI64::new(5),
        };
        let _ = fill_segments(std::slice::from_ref(&segment), &ctx, &mut open);

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 5 && e >= 50),
            "il buco [16,39] tra le due isole deve essere colmato, non lasciato scoperto per \
             sempre da un riaggancio prematuro: ranges={ranges:?}"
        );
    }

    /// Regression for the same real bug confirmed by the user: even
    /// after fixing the premature reconnection above, a tight budget
    /// could still prevent filling a hole far from the
    /// nearest keyframe — because the pure transit frames (decoded
    /// only to cross a long GOP towards the requested segment,
    /// never part of any wanted window) were inserted into the cache and
    /// counted against the budget like everything else, and could saturate it
    /// before even reaching the stretch actually requested. Here a
    /// budget that is enough for the requested segment but not for all
    /// the transit preceding it too must still manage to fill it.
    #[test]
    fn fill_segments_does_not_let_transit_frames_exhaust_the_budget_before_the_wanted_range() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "transit_budget.mp4",
            4,
            250, // long GOP: no keyframe between 0 and the requested segment
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // A budget that is only enough for the requested segment (80,90], 11
        // frames, not for the 80 frames of pure transit to
        // decode to reach it from the keyframe at 0.
        let frame_bytes = 320 * 240 * 3 / 2;
        let tight_budget = frame_bytes * 11;
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 80,
            source_end: 90,
            timeline_start: 80,
            rate: Rational::one(),
        };
        let ctx = FillContext {
            project: &project,
            caches: &caches,
            went_backward: false,
            cache_budget_bytes: tight_budget,
            from_frame: 80,
            proxy: None,
            target: &AtomicI64::new(80),
        };
        let _ = fill_segments(std::slice::from_ref(&segment), &ctx, &mut open);

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 80 && e >= 90),
            "il segmento richiesto [80,90] deve essere raggiunto, non lasciato scoperto perché \
             il budget si è esaurito sul transito prima di arrivarci: ranges={ranges:?}"
        );
    }

    /// Regression for a real bug, confirmed by the user with a
    /// diagnostic log during normal playback on a real 1080p60fps file
    /// with GOP=250: a first attempt at this check
    /// estimated the necessary transit from the observed GOP and skipped the
    /// segment if that estimate alone exceeded the budget — violating
    /// the very rule the budget check on the *wanted* frame
    /// respects on purpose (the transit never counts against the budget of
    /// its own stretch). The real effect: the *forward* segment (not
    /// only those behind) stayed stuck for hundreds of consecutive
    /// frames every time the transit estimate looked
    /// large, even with plenty of free budget — playback stalling
    /// for seconds every time the playhead crossed a GOP
    /// boundary. Here it checks that an "experienced" decoder (which already knows
    /// the GOP and the last keyframe, hence would estimate a huge transit for
    /// a far segment) does NOT prevent filling a
    /// segment near the playhead when there is plenty of budget — only
    /// the availability of room for the wanted stretch counts, never an
    /// estimate of how much transit it takes to get there.
    #[test]
    fn fill_segments_does_not_block_a_reachable_segment_just_because_its_transit_would_be_large() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "transit_estimate_does_not_block.mp4",
            4,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path: path.clone(),
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // "Experienced" decoder: it already knows GOP=25 and a keyframe at 0. The
        // segment below (24,34) requires a *real* transit of 24
        // frames (almost a whole GOP) to be reached from the nearest
        // keyframe — genuine, not the result of a stale anchor: if
        // the check estimated it and compared it with the tight budget
        // below (which is enough for the wanted stretch anyway, 11 frames),
        // it would block the segment despite it being actually
        // reachable.
        let frame_bytes = 320 * 240 * 3 / 2;
        let mut od = OpenDecoder::fresh(Decoder::open(&path).unwrap(), path, false);
        od.record_keyframe_landing(0);
        od.record_keyframe_landing(25);
        open.insert(media_a, od);

        let segment = MediaSegment {
            media_id: media_a,
            source_start: 24,
            source_end: 34,
            timeline_start: 24,
            rate: Rational::one(),
        };
        // Enough for the wanted stretch (11 frames) with plenty of margin, but
        // less than the estimated transit (24 frames): with budget=frame_bytes*20
        // the old check (estimated transit >= free space, even
        // if that space is not needed by the transit at all) blocked it
        // anyway.
        let budget_enough_for_the_wanted_range_but_not_the_full_transit = frame_bytes * 20;
        let ctx = FillContext {
            project: &project,
            caches: &caches,
            went_backward: true,
            cache_budget_bytes: budget_enough_for_the_wanted_range_but_not_the_full_transit,
            from_frame: 24,
            proxy: None,
            target: &AtomicI64::new(24),
        };
        let outcome = fill_segments(std::slice::from_ref(&segment), &ctx, &mut open);

        assert_eq!(
            outcome,
            ControlFlow::Continue(()),
            "il tratto voluto ha budget a sufficienza: non deve essere saltato solo perché il \
             transito per arrivarci è stimato grande"
        );
        assert!(
            caches
                .cached_ranges(media_a)
                .iter()
                .any(|&(s, e)| s <= 24 && e >= 34),
            "il segmento [24,34] deve essere in cache"
        );
    }

    /// Regression for the bug reported by the user and confirmed by the real
    /// diagnostic log: when the playhead advances in small steps (never
    /// enough to exceed the seek threshold and force a real
    /// seek) the decoder stays comfortably ahead and continues from where it
    /// was — correct and intended (see `position_decoder`) — but the
    /// cache was evicted by the standard LRU alone, which removes the
    /// oldest only when new frames *arrive*, not when the *playhead
    /// moves*: the front of the buffer therefore stayed stuck far
    /// behind the playhead for an indefinite time, while the
    /// tail grew by a few frames on every cycle — exactly the
    /// fixed gap "the buffer always starts a few frames after the
    /// playhead" reported by the user (confirmed with a tight budget
    /// forcing the capacity to be exceeded on every cycle). With the
    /// retention window behind the playhead, the buffer also covers a
    /// stretch *before* each target: the right check now is that the
    /// playhead is covered (no longer in a gap), not that a range starts
    /// exactly there.
    #[test]
    fn walk_and_fill_keeps_the_buffer_front_at_the_playhead_even_without_a_real_reseek() {
        let path = make_test_clip("vv-app-render-ahead-test", "front_tracks_target.mp4", 20);

        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Tight budget: every advance of 10 frames adds more
        // frames than the cache can hold without evicting some,
        // forcing the eviction to act on every cycle.
        let budget = 43_000_000;

        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            10,
            budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(10),
        );

        let mut target = 300;
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            target,
            budget,
            false,
            None,                   // proxy: irrelevant for this test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(target),
        );
        for _ in 0..15 {
            target += 10;
            walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                target,
                budget,
                false,
                None,                   // proxy: irrelevant for this test
                DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrelevant for this test
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(target),
            );
            let ranges = caches.cached_ranges(media_a);
            assert!(
                ranges.iter().any(|&(s, e)| s <= target && target <= e),
                "il buffer deve coprire la testina (target={target}): {ranges:?}"
            );
        }
    }
}
