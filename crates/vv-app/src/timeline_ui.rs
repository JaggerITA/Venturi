//! Multi-track timeline widget: clips, selection (click, ctrl, shift,
//! rubber band, always extended to the linked groups), drag, trim, playhead.
//! Changes to the project go through `History::do_command`; only the
//! selection mutates `TimelineState`. Drawn with the painter: for a dense
//! grid of rectangles it costs less than nested widgets.

use std::collections::BTreeSet;

use vv_core::{
    Clip, ClipId, ClipSource, FadeEdge, FrameIdx, History, Keyframed, Project, TimelineId, Track,
    TrackFlag, TrackKind, TrimEdge,
};

const ROW_HEIGHT: f32 = 40.0;
const RULER_HEIGHT: f32 = 20.0;
const MIN_TIMELINE_SECS: f64 = 20.0;
const TRAILING_MARGIN_SECS: f64 = 5.0;
/// Height of the draggable separator between the Video group and the Audio group.
const GROUP_DIVIDER_HEIGHT: f32 = 8.0;
/// Fixed column on the left of the timeline (track label + remove),
/// not involved in the horizontal scroll — see `draw_track_headers`.
const TRACK_HEADER_WIDTH: f32 = 140.0;
const MIN_PANE_HEIGHT: f32 = 20.0;
/// Minimum "new track" zone past the last track when the box
/// scrolls: without it, with many tracks there would be nowhere to drag a new one.
const NEW_TRACK_ZONE_HEIGHT: f32 = 24.0;
const PANE_SCROLLBAR_WIDTH: f32 = 8.0;

/// (track, id): the id is a global counter, the track serves to find it.
type ClipKey = (usize, ClipId);

/// What the properties panel shows when a transition is selected
/// instead of a clip — an alternative to `TimelineState::selected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionSelection {
    /// Transition on a single edge (`EffectStack::transition_in`/`_out`).
    Edge(ClipKey, FadeEdge),
    /// Straddling transition, identified by its `left_clip` (a clip has
    /// at most one crossing on its own right edge) and by the track.
    Crossing(usize, ClipId),
}

pub struct TimelineState {
    /// Selected clips. Empty if no clip is selected (it must not
    /// be confused with "no timeline": here it is only the state of the
    /// selection inside an existing timeline).
    pub selected: BTreeSet<ClipKey>,
    /// Origin of the shift+click. A shift+click does not move it, as in
    /// file managers.
    selection_anchor: Option<ClipKey>,
    pub playhead: FrameIdx,
    pixels_per_sec: f32,
    /// `pixels_per_sec` of the last drawing: if it changed there was a zoom
    /// in this frame, and the scroll must be corrected to anchor it to the playhead.
    last_rendered_pps: f32,
    drag: Option<DragState>,
    /// Selection rectangle in progress, in content coordinates (it stays
    /// valid if the scroll changes).
    marquee: Option<MarqueeDrag>,
    /// Selected gap (track, start, end), an alternative to `selected`:
    /// it is closed with a ripple delete. The space at the end is not a gap.
    pub selected_gap: Option<(usize, FrameIdx, FrameIdx)>,
    /// Selected transition (single edge or crossing), an alternative to
    /// `selected`: a click on it replaces any clip selection,
    /// even a multiple one — the properties panel shows its
    /// controls instead of the clip's.
    pub selected_transition: Option<TransitionSelection>,
    /// Copied clips (Ctrl+C in `main.rs`), ready to be pasted
    /// (Ctrl+V) at the playhead position. Empty if nothing has ever
    /// been copied in this session yet.
    pub clipboard: Vec<ClipboardEntry>,
    trim: Option<TrimState>,
    fade_drag: Option<FadeDragState>,
    transition_drag: Option<TransitionDragState>,
    crossing_drag: Option<CrossingDragState>,
    /// Alt+drag on the body (not on the handle) of a transition marker,
    /// in progress: it duplicates instead of resizing. It mutates nothing by itself — the
    /// DnD payload (`vv_core::Transition`) leaves already set by
    /// `begin_transition_duplicate_drag` and from there `egui::DragAndDrop`
    /// keeps it alive for the duration of the drag; this field only serves to
    /// know when the gesture ends (see the `drag_stopped` branch), for the
    /// origin clip.
    transition_duplicate_drag: Option<ClipId>,
    volume_drag: Option<VolumeDragState>,
    /// Height of the Video box if the user dragged the separator
    /// (see `GROUP_DIVIDER_HEIGHT`); `None` = groups centered by default.
    video_pane_height: Option<f32>,
    /// Vertical scroll of the Video box, measured from the bottom: the video
    /// tracks rest against the separator, as in an NLE.
    video_scroll: f32,
    audio_scroll: f32,
    /// Residual speed (px/s) of the kinetic touchpad scroll: once the gesture
    /// is over the view keeps scrolling and brakes with friction until it
    /// reaches `KINETIC_STOP_SPEED`, instead of stopping abruptly.
    hscroll_vel: f32,
    video_scroll_vel: f32,
    audio_scroll_vel: f32,
    /// In/out of the timeline: the exported portion.
    pub export_marks: crate::transport::MarkRange,
}

/// A copied clip. Id and group are reassigned on paste; the position
/// is relative to the leftmost of the copied clips.
#[derive(Clone)]
pub struct ClipboardEntry {
    /// Origin track as V/A + number, not an absolute index: pasting
    /// from a compound clip with video tracks only into a
    /// video+audio timeline must land on V2, not on the track of index 2 (which there
    /// is audio).
    pub track_kind: TrackKind,
    pub track_number: usize,
    pub relative_start: FrameIdx,
    /// The clip as it was at copy time; id, position and group are reassigned
    /// on paste.
    pub clip: Clip,
    /// Fps of the origin timeline, in which `clip` and
    /// `relative_start` are expressed.
    pub timeline_fps: vv_core::Rational,
    /// Clips with the same tag were in the same group at copy time.
    pub link_tag: Option<u64>,
}

struct MarqueeDrag {
    start: egui::Pos2,
    current: egui::Pos2,
}

struct DragState {
    /// The pressed clip: its position drives the snapping, the others
    /// follow it with the initial offset.
    clip_id: ClipId,
    /// Starting track: the candidate track (see `track_drag_target`)
    /// may differ during the drag, the bounds are recomputed every frame.
    track_index: usize,
    original_start: FrameIdx,
    accum_px: f32,
    /// (clip_id, track_index, offset) of the clips following the primary one:
    /// the selection at the start of the drag, linked groups included.
    followers: Vec<(ClipId, usize, FrameIdx)>,
    /// Drag started with ALT: on release copies are inserted, the
    /// originals stay where they are.
    duplicate: bool,
}

/// Dragging the fade handle (fade-in or fade-out) of a
/// clip: no followers nor neighbors, it is always local to the single clip.
struct FadeDragState {
    clip_id: ClipId,
    track_index: usize,
    edge: FadeEdge,
    /// Original value (frames) of `fade_in`/`fade_out` before the drag.
    original_value: FrameIdx,
    accum_px: f32,
}

/// Dragging the end (duration) of a transition: same shape
/// as `FadeDragState`, same single clip involved.
struct TransitionDragState {
    clip_id: ClipId,
    track_index: usize,
    edge: FadeEdge,
    /// Duration (frames) before the drag.
    original_value: FrameIdx,
    accum_px: f32,
}

/// Dragging the end of a crossing transition: unlike
/// `TransitionDragState`, it always touches both sides equally (see
/// `CrossTransition::split`) — here a `FadeEdge` is not needed, only knowing whether
/// the end being grabbed is the one inside the left clip or the one
/// inside the right clip, for the sign of the displacement.
struct CrossingDragState {
    track_index: usize,
    left_clip: ClipId,
    grabbed_left_side: bool,
    /// Total duration (frames) before the drag.
    original_duration: FrameIdx,
    /// It cannot exceed the duration of the two clips involved: computed once
    /// at the start of the drag, the clips do not change length in the
    /// meantime.
    max_duration: FrameIdx,
    accum_px: f32,
}

/// Vertical dragging of the volume line on an audio clip: like
/// `FadeDragState`, always local to the single clip, never a multiple
/// selection. Unlike fade/trim, the gain is really applied (via
/// `PendingAction::SetGain`) on every drag frame instead of only on
/// release — the same chain of events as the properties panel slider,
/// so the waveform and the clip color follow live. `group` keeps
/// all those commits together in a single undo step (see
/// `History::begin_group`).
struct VolumeDragState {
    clip_id: ClipId,
    track_index: usize,
    /// Gain (dB) before the drag.
    original_db: f32,
    accum_px: f32,
    group: vv_core::GroupMark,
}

/// Trim of an edge, separate from an actual drag.
struct TrimState {
    clip_id: ClipId,
    track_index: usize,
    edge: TrimEdge,
    /// Original value (frames, timeline space) of the trimmed
    /// coordinate: `timeline_start` for `Start`, `timeline_end()` for `End`.
    original_value: FrameIdx,
    accum_px: f32,
    /// Valid range for the *new* value of `original_value`, already
    /// combined with that of all the `followers` (see
    /// `combined_trim_range`).
    min_value: FrameIdx,
    max_value: FrameIdx,
    /// (clip_id, track_index, offset, edge) of the other clips trimmed together;
    /// in a roll, the neighbor too, with the opposite edge.
    followers: Vec<(ClipId, usize, FrameIdx, TrimEdge)>,
    /// Roll edit between two adjacent clips (for the cursor only).
    roll: bool,
}

/// What a drag started near the edge of a clip does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EdgeZone {
    Trim(TrimEdge),
    /// On the contact point with the adjacent clip `neighbor`: `edge` is the
    /// edge of this clip, the neighbor moves with the opposite one.
    Roll { edge: TrimEdge, neighbor: ClipKey },
}

impl EdgeZone {
    fn edge(self) -> TrimEdge {
        match self {
            EdgeZone::Trim(edge) | EdgeZone::Roll { edge, .. } => edge,
        }
    }
}

/// Distance (in screen pixels) from the edge of a clip within which a drag
/// starts as a trim instead of as a move; reduced for very
/// narrow clips, otherwise the whole clip would be "only edges".
const TRIM_HANDLE_PX: f32 = 8.0;
/// Half-width of the roll zone around the contact point between two
/// adjacent clips; the trim zone starts right after, towards the inside.
const ROLL_HANDLE_PX: f32 = 4.0;
/// Radius of the dot drawn for the fade-in/fade-out handle.
const FADE_HANDLE_RADIUS: f32 = 4.0;
/// Half-width of the clickable zone around the handle: wider than the
/// drawn dot, so it can be grabbed without aiming at the pixel.
const FADE_HANDLE_HIT_RADIUS: f32 = 9.0;
/// Band at the top of the clip reserved for the fade handles: below stays the
/// trim/roll of the edge, as in the other NLEs.
const FADE_HANDLE_ZONE_HEIGHT: f32 = 14.0;
/// Below this width the clip shows no fade handles: there would be no
/// room to grab them without colliding with the trim.
const MIN_FADE_CLIP_WIDTH_PX: f32 = 20.0;
/// Band at the bottom of the clip reserved for the transition marker: mirroring
/// the fade band at the top, so the two do not contend for the same hover.
const TRANSITION_HANDLE_ZONE_HEIGHT: f32 = 14.0;
/// Half-width of the clickable zone around the draggable end (duration)
/// of a transition, like `FADE_HANDLE_HIT_RADIUS`.
const TRANSITION_HANDLE_HIT_RADIUS: f32 = 9.0;
/// Within how many pixels of a clip edge a transition drop is
/// accepted; past that, releasing in the middle of the clip does nothing.
const TRANSITION_DROP_ZONE_PX: f32 = 40.0;
/// Color of a transition marker: also the highlight of the edge
/// during the drag, so the color anticipates what will appear on release.
const TRANSITION_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 130, 235);
const TRANSITION_SELECTED_COLOR: egui::Color32 = egui::Color32::from_rgb(190, 197, 255);
/// Color of a crossing transition marker: a different hue from that
/// of a single edge, to signal at a glance that this one "eats"
/// the neighboring clip too instead of staying against transparency.
const CROSSING_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 150, 90);
const CROSSING_SELECTED_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 195, 150);
/// Vertical distance (px) within which the pointer grabs the volume
/// line of an audio clip.
const VOLUME_LINE_HIT_PX: f32 = 5.0;

/// At 0.1 px/s an hour fits in 360 px.
const MIN_PIXELS_PER_SEC: f32 = 0.1;
const MAX_PIXELS_PER_SEC: f32 = 800.0;

/// Below this speed the kinetic scroll stops instead of creeping
/// forever. Lower friction than egui's native one (1000 px/s²):
/// at that value the swipe was barely felt, here it glides longer.
const KINETIC_STOP_SPEED: f32 = 15.0; // px/s
const KINETIC_FRICTION: f32 = 500.0; // px/s^2
/// Amplifies the speed captured from the swipe: the native sensitivity
/// felt weak, a normal gesture barely set the inertia in motion.
const KINETIC_VELOCITY_GAIN: f32 = 1.6;

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            selected: BTreeSet::new(),
            selection_anchor: None,
            playhead: 0,
            pixels_per_sec: 60.0,
            // Same initial value as `pixels_per_sec`: on the first frame there
            // is no zoom to compensate yet.
            last_rendered_pps: 60.0,
            drag: None,
            marquee: None,
            selected_gap: None,
            selected_transition: None,
            clipboard: Vec::new(),
            trim: None,
            fade_drag: None,
            transition_drag: None,
            crossing_drag: None,
            transition_duplicate_drag: None,
            volume_drag: None,
            video_pane_height: None,
            video_scroll: 0.0,
            audio_scroll: 0.0,
            hscroll_vel: 0.0,
            video_scroll_vel: 0.0,
            audio_scroll_vel: 0.0,
            export_marks: crate::transport::MarkRange::default(),
        }
    }
}

impl TimelineState {
    /// Sets selection and anchor from outside (e.g. after a cut).
    pub fn set_selection(&mut self, selected: BTreeSet<ClipKey>, anchor: Option<ClipKey>) {
        self.selected = selected;
        self.selection_anchor = anchor;
        self.selected_gap = None;
        self.selected_transition = None;
    }

    /// A single clip (or none), for "selection follows playhead".
    pub fn set_single_selection(&mut self, clip: Option<ClipKey>) {
        self.set_selection(clip.into_iter().collect(), clip);
    }

    /// Removes from the selection whatever is on locked tracks.
    pub fn drop_locked(&mut self, timeline: &vv_core::Timeline) {
        self.selected.retain(|&(track_index, _)| !timeline.is_locked(track_index));
        if self
            .selection_anchor
            .is_some_and(|(track_index, _)| timeline.is_locked(track_index))
        {
            self.selection_anchor = None;
        }
        if self
            .selected_gap
            .is_some_and(|(track_index, _, _)| timeline.is_locked(track_index))
        {
            self.selected_gap = None;
        }
        if self.selected_transition.is_some_and(|sel| {
            let track_index = match sel {
                TransitionSelection::Edge((track_index, _), _) => track_index,
                TransitionSelection::Crossing(track_index, _) => track_index,
            };
            timeline.is_locked(track_index)
        }) {
            self.selected_transition = None;
        }
    }

    /// Empties the selection (clips, gap and transition).
    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.selection_anchor = None;
        self.selected_gap = None;
        self.selected_transition = None;
    }

    /// Horizontal zoom anchored to the playhead.
    pub fn zoom_in(&mut self) {
        self.set_pixels_per_sec(self.pixels_per_sec * ZOOM_STEP);
    }

    pub fn zoom_out(&mut self) {
        self.set_pixels_per_sec(self.pixels_per_sec / ZOOM_STEP);
    }

    fn set_pixels_per_sec(&mut self, value: f32) {
        self.pixels_per_sec = value.clamp(MIN_PIXELS_PER_SEC, MAX_PIXELS_PER_SEC);
    }
}

/// Zoom factor per step of `zoom_in`/`zoom_out`.
const ZOOM_STEP: f32 = 1.25;

struct ClipVisual<'a> {
    track_index: usize,
    /// Borrowed from the project; `Owned` only in the tests.
    clip: std::borrow::Cow<'a, Clip>,
    label: String,
    color: egui::Color32,
    /// The track is locked: the clip is untouchable.
    locked: bool,
    /// Excluded from the output: it or its video track is disabled.
    muted: bool,
}

/// Command collected during the drawing (which borrows `project`) and
/// applied afterwards.
enum PendingAction {
    /// Moves a dragged group. The new tracks must be created before
    /// resolving the `EffectiveTrack::New` of `moves`.
    Move {
        new_video_tracks: usize,
        new_audio_tracks: usize,
        moves: Vec<(ClipId, usize, EffectiveTrack, FrameIdx)>,
        duplicate: bool,
    },
    /// (clip_id, track, edge, new position) for every clip, plus the stretches
    /// they take by lengthening: whatever was there gets overwritten.
    Trim {
        trims: Vec<(ClipId, usize, TrimEdge, FrameIdx)>,
        overwritten: Vec<(usize, FrameIdx, FrameIdx)>,
    },
    /// New duration (in frames) of the fade in or out.
    SetFade {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        new_value: FrameIdx,
    },
    /// New constant gain (dB), from dragging the volume line on the timeline.
    SetGain {
        track_index: usize,
        clip_id: ClipId,
        new_value: f32,
    },
    /// A filter of the Effects panel was dropped on this clip:
    /// appended to its list (or re-enabled if already present),
    /// active by default.
    ApplyFilter {
        track_index: usize,
        clip_id: ClipId,
        filter: vv_core::FilterKind,
    },
    /// A transition of the Effects panel was dropped near an
    /// edge of this clip: it replaces the one already present on that edge
    /// (dropping again shortens/restores the default duration), never appended to
    /// a list like the filters — an edge has at most one.
    ApplyTransition {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        kind: vv_core::TransitionKind,
    },
    /// New duration (in frames) of the transition of an edge, from dragging
    /// its end on the timeline.
    SetTransitionDuration {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        new_value: FrameIdx,
    },
    /// An existing transition was duplicated (Alt+drag from its
    /// body) and dropped near an edge: unlike
    /// `ApplyTransition`, it does not start from the defaults — it keeps the same
    /// parameters as the original.
    DuplicateTransition {
        track_index: usize,
        clip_id: ClipId,
        edge: FadeEdge,
        transition: vv_core::Transition,
    },
    /// New duration (in frames) of a crossing transition, from the symmetric
    /// drag of its end — see `CrossingDragState`.
    SetCrossingDuration {
        track_index: usize,
        left_clip: ClipId,
        new_value: FrameIdx,
    },
    Unlink(usize, ClipId),
    /// Links all the listed clips (track_index, clip_id) into a single
    /// new group — at least 2.
    Link(Vec<ClipKey>),
    /// Removes the track at this index (and its clips).
    RemoveTrack(usize),
    SetTrackFlag(usize, TrackFlag, bool),
    /// Replaces the listed clips with a compound clip (see
    /// `make_compound_clip`).
    MakeCompound(Vec<ClipKey>),
}

#[derive(Clone, Copy)]
struct TrackFlags {
    muted: bool,
    solo: bool,
    locked: bool,
}

/// Drawing order: Video from the highest index (the new one is at the top),
/// then Audio in order. Independent of the order in `tracks`.
fn track_row_order(track_kinds: &[TrackKind]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..track_kinds.len())
        .filter(|&i| track_kinds[i] == TrackKind::Video)
        .collect();
    order.reverse();
    order.extend((0..track_kinds.len()).filter(|&i| track_kinds[i] == TrackKind::Audio));
    order
}

/// Vertical geometry (`y` local to the content) of the two boxes, Video
/// above and Audio below the separator, each with its own scroll.
#[derive(Clone, Copy, Debug)]
struct PaneLayout {
    video_pane: egui::Rangef,
    audio_pane: egui::Rangef,
    video_rows_top: f32,
    audio_rows_top: f32,
    video_count: usize,
    audio_count: usize,
    divider_height: f32,
    video_max_scroll: f32,
    audio_max_scroll: f32,
    /// Limits of the Video box height when dragging the separator.
    video_height_range: egui::Rangef,
}

impl PaneLayout {
    /// It also clamps the scrolls in `state` to the current limits.
    fn new(
        avail_below_ruler: f32,
        video_count: usize,
        audio_count: usize,
        state: &mut TimelineState,
    ) -> Self {
        let divider_height = if video_count > 0 && audio_count > 0 {
            GROUP_DIVIDER_HEIGHT
        } else {
            0.0
        };
        let rows_avail = (avail_below_ruler - divider_height).max(0.0);
        let video_rows = video_count as f32 * ROW_HEIGHT;
        let audio_rows = audio_count as f32 * ROW_HEIGHT;
        let default_video_height = if video_rows + audio_rows <= rows_avail {
            (rows_avail - video_rows - audio_rows) / 2.0 + video_rows
        } else {
            rows_avail * video_count as f32 / (video_count + audio_count) as f32
        };
        let min_height = if divider_height > 0.0 {
            MIN_PANE_HEIGHT.min(rows_avail / 2.0)
        } else {
            0.0
        };
        let video_height_range = egui::Rangef::new(min_height, rows_avail - min_height);
        let video_height = if divider_height > 0.0 {
            state
                .video_pane_height
                .unwrap_or(default_video_height)
                .clamp(video_height_range.min, video_height_range.max)
        } else {
            default_video_height
        };
        let audio_height = rows_avail - video_height;

        let max_scroll = |rows: f32, pane: f32| {
            if rows > pane {
                rows + NEW_TRACK_ZONE_HEIGHT - pane
            } else {
                0.0
            }
        };
        let video_max_scroll = max_scroll(video_rows, video_height);
        let audio_max_scroll = max_scroll(audio_rows, audio_height);
        state.video_scroll = state.video_scroll.clamp(0.0, video_max_scroll);
        state.audio_scroll = state.audio_scroll.clamp(0.0, audio_max_scroll);

        let video_bottom = RULER_HEIGHT + video_height;
        Self {
            video_pane: egui::Rangef::new(RULER_HEIGHT, video_bottom),
            audio_pane: egui::Rangef::new(
                video_bottom + divider_height,
                RULER_HEIGHT + avail_below_ruler.max(divider_height),
            ),
            video_rows_top: video_bottom - video_rows + state.video_scroll,
            audio_rows_top: video_bottom + divider_height - state.audio_scroll,
            video_count,
            audio_count,
            divider_height,
            video_max_scroll,
            audio_max_scroll,
            video_height_range,
        }
    }

    fn video_height(&self) -> f32 {
        self.video_pane.span()
    }

    fn video_rows_bottom(&self) -> f32 {
        self.video_rows_top + self.video_count as f32 * ROW_HEIGHT
    }

    fn audio_rows_bottom(&self) -> f32 {
        self.audio_rows_top + self.audio_count as f32 * ROW_HEIGHT
    }

    fn pane(&self, kind: TrackKind) -> egui::Rangef {
        match kind {
            TrackKind::Video => self.video_pane,
            TrackKind::Audio => self.audio_pane,
        }
    }

    /// `y` of a row of `track_row_order`.
    fn row_y(&self, row: usize) -> f32 {
        if row < self.video_count {
            self.video_rows_top + row as f32 * ROW_HEIGHT
        } else {
            self.audio_rows_top + (row - self.video_count) as f32 * ROW_HEIGHT
        }
    }

    /// Row (of `track_row_order`) nearest to `y`, inside the box
    /// containing `y`.
    fn row_at_y(&self, y: f32) -> usize {
        let in_video = self.video_count > 0 && (self.audio_count == 0 || y < self.audio_pane.min);
        if in_video {
            let row = ((y - self.video_rows_top) / ROW_HEIGHT).floor().max(0.0) as usize;
            row.min(self.video_count - 1)
        } else {
            let row = ((y - self.audio_rows_top) / ROW_HEIGHT).floor().max(0.0) as usize;
            self.video_count + row.min(self.audio_count.saturating_sub(1))
        }
    }

    /// `y` over a visible track (not in an empty zone nor hidden
    /// by the scroll).
    fn is_over_rows(&self, y: f32) -> bool {
        (self.video_pane.contains(y) && y >= self.video_rows_top && y < self.video_rows_bottom())
            || (self.audio_pane.contains(y)
                && y >= self.audio_rows_top
                && y < self.audio_rows_bottom())
    }
}

enum TrackDragTarget {
    Track(usize),
    NewTrack,
}

/// Candidate track of a drag: an existing one, or `New(depth)` to create on
/// release (`depth` 1-based: a follower may need several new
/// tracks to keep the spacing of the group).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveTrack {
    Existing(usize),
    New(usize),
}

/// Where a clip of kind `kind` dragged to `local_y` would land: a track or
/// a new one (zone above the Video ones / below the Audio ones). `None` in the box
/// of the other kind or on the separator.
fn track_drag_target(
    local_y: f32,
    kind: TrackKind,
    row_order: &[usize],
    layout: &PaneLayout,
) -> Option<TrackDragTarget> {
    match kind {
        TrackKind::Video => {
            if local_y >= layout.video_pane.max {
                return None;
            }
            let y = local_y.max(layout.video_pane.min);
            if y < layout.video_rows_top {
                return Some(TrackDragTarget::NewTrack);
            }
            if y < layout.video_rows_bottom() {
                return row_order.get(layout.row_at_y(y)).copied().map(TrackDragTarget::Track);
            }
            None
        }
        TrackKind::Audio => {
            if local_y < layout.audio_pane.min {
                return None;
            }
            let y = local_y.min(layout.audio_pane.max - 1.0);
            if y >= layout.audio_rows_bottom() {
                return Some(TrackDragTarget::NewTrack);
            }
            if y >= layout.audio_rows_top {
                return row_order.get(layout.row_at_y(y)).copied().map(TrackDragTarget::Track);
            }
            None
        }
    }
}

/// Drop on an existing track: if video is dragged onto a video
/// track it lands there, otherwise on the usual track. `None` if it is
/// locked.
fn media_pool_drop_target(
    track: usize,
    has_video: bool,
    track_kind: TrackKind,
    locked: bool,
) -> Option<MediaDropTarget> {
    if locked {
        None
    } else if has_video && track_kind == TrackKind::Video {
        Some(MediaDropTarget::Track(track))
    } else {
        Some(MediaDropTarget::Default)
    }
}

/// Target track of each clip of a dragged group: the followers
/// move by the same number of rows as the primary one (in the opposite direction if
/// of a different kind: video and audio grow in opposite directions). Past
/// the last track of their own kind they become `New(depth)`, in the opposite
/// direction they stop at the nearest one.
fn drag_group_row_targets(
    primary_id: ClipId,
    primary_track: usize,
    primary_target: EffectiveTrack,
    followers: &[(ClipId, usize, FrameIdx)],
    track_kinds: &[TrackKind],
    row_of_track: &[usize],
    row_order: &[usize],
    video_count: usize,
    track_count: usize,
) -> Vec<(ClipId, EffectiveTrack)> {
    let primary_kind = track_kinds[primary_track];
    let primary_original_row = row_of_track[primary_track] as isize;
    let primary_target_row = match primary_target {
        EffectiveTrack::Existing(track) => row_of_track[track] as isize,
        EffectiveTrack::New(_) => match primary_kind {
            TrackKind::Video => -1,
            TrackKind::Audio => track_count as isize,
        },
    };
    let delta_row = primary_target_row - primary_original_row;

    let mut targets = vec![(primary_id, primary_target)];
    for &(follower_id, follower_track, _) in followers {
        let follower_kind = track_kinds[follower_track];
        let signed_delta = if follower_kind == primary_kind {
            delta_row
        } else {
            -delta_row
        };
        let candidate_row = row_of_track[follower_track] as isize + signed_delta;
        let target = match follower_kind {
            TrackKind::Video if candidate_row < 0 => EffectiveTrack::New((-candidate_row) as usize),
            TrackKind::Audio if candidate_row > track_count as isize - 1 => {
                EffectiveTrack::New((candidate_row - (track_count as isize - 1)) as usize)
            }
            TrackKind::Video => {
                EffectiveTrack::Existing(row_order[candidate_row.min(video_count as isize - 1) as usize])
            }
            TrackKind::Audio => EffectiveTrack::Existing(
                row_order[candidate_row.max(video_count as isize) as usize],
            ),
        };
        targets.push((follower_id, target));
    }
    targets
}

/// Fixed column of the headers. Drawn by hand: an `add_space`
/// tied to the height would make the panel grow indefinitely.
fn draw_track_headers(
    ui: &mut egui::Ui,
    track_kinds: &[TrackKind],
    track_labels: &[String],
    track_flags: &[TrackFlags],
    row_order: &[usize],
    row_y: &[f32],
    layout: &PaneLayout,
    video_pane_height: &mut Option<f32>,
    pending: &mut Option<PendingAction>,
    playhead: FrameIdx,
    fps: f64,
) {
    let (rect, _resp) =
        ui.allocate_exact_size(egui::vec2(TRACK_HEADER_WIDTH, RULER_HEIGHT), egui::Sense::hover());
    let origin = rect.min;
    let full_clip = ui.clip_rect();
    let text_color = ui.visuals().text_color();

    // Timestamp of the playhead position in HH:MM:SS:FF format, in the
    // ruler row (as in DaVinci Resolve).
    let playhead_secs = playhead as f64 / fps;
    ui.painter().text(
        egui::pos2(origin.x + TRACK_HEADER_WIDTH / 2.0, origin.y + RULER_HEIGHT / 2.0),
        egui::Align2::CENTER_CENTER,
        format_timecode(playhead_secs, fps),
        egui::FontId::monospace(14.0),
        egui::Color32::WHITE,
    );

    for &track_index in row_order {
        let kind = track_kinds[track_index];
        let pane = layout.pane(kind);
        ui.set_clip_rect(full_clip.intersect(egui::Rect::from_x_y_ranges(
            rect.x_range(),
            (origin.y + pane.min)..=(origin.y + pane.max),
        )));
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(origin.x, origin.y + row_y[track_index]),
            egui::vec2(TRACK_HEADER_WIDTH, ROW_HEIGHT),
        );
        ui.painter().text(
            row_rect.left_center() + egui::vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &track_labels[track_index],
            egui::FontId::proportional(14.0),
            text_color,
        );

        let flags = track_flags[track_index];
        let toggle = |x: f32, id: &str, hover: &str, paint: &dyn Fn(&egui::Painter, egui::Rect)| {
            let rect = egui::Rect::from_center_size(
                egui::pos2(row_rect.left() + x, row_rect.center().y),
                egui::vec2(20.0, 20.0),
            );
            let resp = ui
                .interact(rect, ui.id().with(id).with(track_index), egui::Sense::click())
                .on_hover_text(hover);
            if resp.hovered() {
                ui.painter().rect_filled(rect, 3.0, egui::Color32::from_gray(60));
            }
            paint(ui.painter(), rect);
            resp.clicked()
        };
        if toggle(42.0, "lock_track", &t!("timeline.lock_track"), &|p, r| {
            paint_lock_icon(p, r, flags.locked)
        }) {
            *pending = Some(PendingAction::SetTrackFlag(
                track_index,
                TrackFlag::Locked,
                !flags.locked,
            ));
        }
        match kind {
            TrackKind::Video => {
                if toggle(66.0, "mute_track", &t!("timeline.disable_video_track"), &|p, r| {
                    paint_film_icon(p, r, !flags.muted)
                }) {
                    *pending = Some(PendingAction::SetTrackFlag(
                        track_index,
                        TrackFlag::Muted,
                        !flags.muted,
                    ));
                }
            }
            TrackKind::Audio => {
                let solo_color = egui::Color32::from_rgb(215, 170, 40);
                if toggle(66.0, "solo_track", &t!("timeline.solo"), &|p, r| {
                    paint_letter_button(p, r, "S", flags.solo.then_some(solo_color))
                }) {
                    *pending =
                        Some(PendingAction::SetTrackFlag(track_index, TrackFlag::Solo, !flags.solo));
                }
                let mute_color = egui::Color32::from_rgb(200, 60, 60);
                if toggle(90.0, "mute_track", &t!("timeline.mute"), &|p, r| {
                    paint_letter_button(p, r, "M", flags.muted.then_some(mute_color))
                }) {
                    *pending = Some(PendingAction::SetTrackFlag(
                        track_index,
                        TrackFlag::Muted,
                        !flags.muted,
                    ));
                }
            }
        }

        let is_last_of_kind = track_kinds.iter().filter(|k| **k == kind).count() <= 1;
        const REMOVE_BTN_SIZE: f32 = 18.0;
        let remove_rect = egui::Rect::from_center_size(
            egui::pos2(row_rect.right() - 14.0, row_rect.center().y),
            egui::vec2(REMOVE_BTN_SIZE, REMOVE_BTN_SIZE),
        );
        let sense = if is_last_of_kind {
            egui::Sense::hover()
        } else {
            egui::Sense::click()
        };
        let remove_resp = ui
            .interact(remove_rect, ui.id().with("remove_track").with(track_index), sense)
            .on_hover_text(if is_last_of_kind {
                t!("timeline.cannot_remove_last_track")
            } else {
                t!("timeline.remove_track")
            });
        if !is_last_of_kind && remove_resp.hovered() {
            ui.painter()
                .rect_filled(remove_rect, 3.0, egui::Color32::from_gray(70));
        }
        ui.painter().text(
            remove_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(14.0),
            if is_last_of_kind {
                egui::Color32::from_gray(90)
            } else {
                text_color
            },
        );
        if remove_resp.clicked() {
            *pending = Some(PendingAction::RemoveTrack(track_index));
        }
    }

    ui.set_clip_rect(full_clip);

    if layout.divider_height > 0.0 {
        let divider_rect = egui::Rect::from_min_size(
            egui::pos2(origin.x, origin.y + layout.video_pane.max),
            egui::vec2(TRACK_HEADER_WIDTH, layout.divider_height),
        );
        interact_divider(
            ui,
            ui.painter(),
            divider_rect,
            ui.id().with("timeline_header_track_split"),
            layout,
            video_pane_height,
        );
    }
}

/// Draggable Video/Audio separator: present both in the header column
/// and in the scrollable area, with shared state.
fn interact_divider(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    id: egui::Id,
    layout: &PaneLayout,
    video_pane_height: &mut Option<f32>,
) {
    let resp = ui.interact(rect, id, egui::Sense::drag());
    let active = resp.hovered() || resp.dragged();
    if active {
        ui.ctx()
            .output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeVertical);
    }
    if resp.dragged() {
        let range = layout.video_height_range;
        *video_pane_height =
            Some((layout.video_height() + resp.drag_delta().y).clamp(range.min, range.max));
    }
    painter.hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, egui::Color32::from_gray(if active { 160 } else { 80 })),
    );
}

/// Vertical scrollbar of a box; `offset` measured from the top.
/// Returns the new offset if the user drags it.
fn pane_scrollbar(
    ui: &egui::Ui,
    painter: &egui::Painter,
    track_rect: egui::Rect,
    id: egui::Id,
    offset: f32,
    max_offset: f32,
) -> Option<f32> {
    if max_offset <= 0.0 || track_rect.height() <= 0.0 {
        return None;
    }
    let content_height = track_rect.height() + max_offset;
    let thumb_height = (track_rect.height() * track_rect.height() / content_height)
        .max(16.0)
        .min(track_rect.height());
    let travel = track_rect.height() - thumb_height;
    let thumb_top = track_rect.top() + travel * offset / max_offset;
    let thumb_rect = egui::Rect::from_min_size(
        egui::pos2(track_rect.left(), thumb_top),
        egui::vec2(track_rect.width(), thumb_height),
    );
    let resp = ui.interact(track_rect, id, egui::Sense::click_and_drag());
    let active = resp.hovered() || resp.dragged();
    painter.rect_filled(track_rect, 4.0, egui::Color32::from_black_alpha(90));
    painter.rect_filled(
        thumb_rect.shrink2(egui::vec2(1.0, 1.0)),
        4.0,
        egui::Color32::from_gray(if active { 170 } else { 120 }),
    );
    if resp.dragged() && travel > 0.0 {
        return Some((offset + resp.drag_delta().y * max_offset / travel).clamp(0.0, max_offset));
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && travel > 0.0
    {
        let target = (pos.y - track_rect.top() - thumb_height / 2.0) / travel * max_offset;
        return Some(target.clamp(0.0, max_offset));
    }
    None
}

fn paint_lock_icon(painter: &egui::Painter, rect: egui::Rect, locked: bool) {
    let color = if locked {
        egui::Color32::from_gray(235)
    } else {
        egui::Color32::from_gray(110)
    };
    let c = rect.center();
    let body = egui::Rect::from_min_max(c + egui::vec2(-5.0, -1.0), c + egui::vec2(5.0, 6.0));
    painter.rect_filled(body, 1.5, color);
    // When open, the right leg of the arc does not reach the body.
    let right_leg_end = if locked { -1.0 } else { -4.0 };
    let mut points = vec![c + egui::vec2(-3.5, -1.0), c + egui::vec2(-3.5, -3.5)];
    points.extend((0..=8).map(|i| {
        let a = std::f32::consts::PI * (1.0 + i as f32 / 8.0);
        c + egui::vec2(3.5 * a.cos(), -3.5 + 3.5 * a.sin())
    }));
    points.push(c + egui::vec2(3.5, right_leg_end));
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.6, color)));
}

/// Film strip; crossed out in red if the track is disabled.
fn paint_film_icon(painter: &egui::Painter, rect: egui::Rect, enabled: bool) {
    let color = egui::Color32::from_gray(if enabled { 200 } else { 100 });
    let film = egui::Rect::from_center_size(rect.center(), egui::vec2(14.0, 11.0));
    painter.rect_stroke(film, 1.0, egui::Stroke::new(1.3, color), egui::StrokeKind::Inside);
    for i in 0..4 {
        let x = film.left() + 2.5 + i as f32 * 3.0;
        for y in [film.top() + 2.0, film.bottom() - 2.0] {
            painter.rect_filled(
                egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(1.4, 1.4)),
                0.0,
                color,
            );
        }
    }
    if !enabled {
        painter.line_segment(
            [film.left_bottom() + egui::vec2(-1.0, 1.0), film.right_top() + egui::vec2(1.0, -1.0)],
            egui::Stroke::new(1.6, egui::Color32::from_rgb(220, 70, 70)),
        );
    }
}

/// Hand-drawn gear (ring + radial teeth): no Unicode glyph, which
/// on some platforms (Asahi) is missing from egui's fonts (see the comment
/// on the link icon in `paint_clip_overlay`).
pub(crate) fn paint_gear_icon(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.6, color);
    painter.circle_stroke(center, radius * 0.55, stroke);
    painter.circle_filled(center, radius * 0.16, color);
    const TEETH: usize = 8;
    for i in 0..TEETH {
        let angle = std::f32::consts::TAU * i as f32 / TEETH as f32;
        let dir = egui::vec2(angle.cos(), angle.sin());
        painter.line_segment([center + dir * radius * 0.55, center + dir * radius], stroke);
    }
}

/// "S"/"M" button: filled with `active` when it is on.
fn paint_letter_button(
    painter: &egui::Painter,
    rect: egui::Rect,
    letter: &str,
    active: Option<egui::Color32>,
) {
    let button = rect.shrink(2.0);
    let text_color = match active {
        Some(fill) => {
            painter.rect_filled(button, 3.0, fill);
            egui::Color32::BLACK
        }
        None => {
            painter.rect_stroke(
                button,
                3.0,
                egui::Stroke::new(1.0, egui::Color32::from_gray(90)),
                egui::StrokeKind::Inside,
            );
            egui::Color32::from_gray(150)
        }
    };
    painter.text(
        button.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::proportional(11.0),
        text_color,
    );
}

/// Interval between major ticks from the 1-2-5 sequence, the first one that
/// keeps them at least `MIN_MAJOR_TICK_PX` apart.
fn nice_tick_interval_secs(pixels_per_sec: f32) -> f64 {
    const MIN_MAJOR_TICK_PX: f32 = 70.0;
    const CANDIDATES: &[f64] = &[
        1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0,
        14400.0,
    ];
    CANDIDATES
        .iter()
        .copied()
        .find(|&c| c as f32 * pixels_per_sec >= MIN_MAJOR_TICK_PX)
        .unwrap_or(*CANDIDATES.last().unwrap())
}

/// `true` if the pointer is on the rectangle *and* nothing is above it: a
/// floating window (the keyframe editor) keeps its own scroll,
/// the timeline underneath must not react.
pub(crate) fn pointer_over(ctx: &egui::Context, rect: egui::Rect) -> bool {
    ctx.input(|i| i.pointer.hover_pos()).is_some_and(|p| {
        rect.contains(p)
            && ctx.layer_id_at(p).is_none_or(|l| l.order == egui::Order::Background)
    })
}

/// Non-drop-frame HH:MM:SS:FF timecode: with non-integer fps (29.97) the
/// seconds are counted on the rounded nominal fps, as in NLEs.
pub(crate) fn format_timecode(total_secs: f64, fps: f64) -> String {
    let nominal = (fps.round() as i64).max(1);
    let frame = (total_secs.max(0.0) * fps).round() as i64;
    let (h, m) = (frame / (nominal * 3600), frame / (nominal * 60) % 60);
    let (s, f) = (frame / nominal % 60, frame % nominal);
    format!("{h:02}:{m:02}:{s:02}:{f:02}")
}

/// Ticks on three levels: major ones with timecode, medium ones every N frames,
/// one per frame; a level denser than `MIN_TICK_SPACING_PX` is not
/// drawn.
fn draw_ruler_ticks(
    painter: &egui::Painter,
    origin: egui::Pos2,
    visible_x: egui::Rect,
    pixels_per_sec: f32,
    fps: f64,
) {
    if !visible_x.is_positive() {
        return; // ruler completely outside the scrolled viewport
    }

    const MIN_TICK_SPACING_PX: f32 = 8.0;
    let tick_color = egui::Color32::from_gray(110);
    let label_color = egui::Color32::from_gray(200);
    let minor_color = egui::Color32::from_gray(70);
    let medium_color = egui::Color32::from_gray(90);

    // Heights of the three tick levels (from bottom to top).
    const FRAME_TICK_HEIGHT: f32 = 5.0;
    const MEDIUM_TICK_HEIGHT: f32 = 10.0;
    const MAJOR_TICK_HEIGHT: f32 = RULER_HEIGHT;

    // Range in seconds actually visible, not the whole duration of the
    // timeline (thousands of off-screen ticks otherwise).
    let visible_start_secs = ((visible_x.min.x - origin.x) / pixels_per_sec.max(1e-6)) as f64;
    let visible_end_secs = ((visible_x.max.x - origin.x) / pixels_per_sec.max(1e-6)) as f64;

    // Level 2: major ticks with an HH:MM:SS:FF label, adaptive
    // "clean" interval (1-2-5 sequence) — always visible.
    let major_secs = nice_tick_interval_secs(pixels_per_sec);
    let first_major = (visible_start_secs / major_secs).floor() as i64;
    let last_major = (visible_end_secs / major_secs).ceil() as i64;
    for i in first_major..=last_major {
        let secs = i as f64 * major_secs;
        if secs < 0.0 {
            continue;
        }
        let x = origin.x + (secs * pixels_per_sec as f64) as f32;
        painter.line_segment(
            [egui::pos2(x, origin.y), egui::pos2(x, origin.y + MAJOR_TICK_HEIGHT)],
            egui::Stroke::new(1.0, tick_color),
        );
        painter.text(
            egui::pos2(x + 3.0, origin.y + 2.0),
            egui::Align2::LEFT_TOP,
            format_timecode(secs, fps),
            egui::FontId::proportional(10.0),
            label_color,
        );
    }

    // Medium ticks, only if denser than the major ones.
    let px_per_frame = pixels_per_sec / fps.max(1e-9) as f32;

    // Computes the interval in frames for the medium ticks: the smallest
    // "clean" multiple (1, 2, 5, 10, 25, 50...) that keeps the ticks at
    // least MIN_TICK_SPACING_PX apart.
    let medium_interval_frames = {
        const MEDIUM_CANDIDATES: &[i64] = &[1, 2, 5, 10, 25, 50, 100, 250, 500];
        MEDIUM_CANDIDATES
            .iter()
            .copied()
            .find(|&c| c as f32 * px_per_frame >= MIN_TICK_SPACING_PX)
            .unwrap_or(*MEDIUM_CANDIDATES.last().unwrap())
    };

    // The medium ticks are useful only if they are closer than the major ones and
    // do not overlap them exactly (otherwise they would be redundant).
    let major_interval_frames = (major_secs * fps) as i64;
    if medium_interval_frames < major_interval_frames {
        let first_frame = (visible_start_secs * fps).floor().max(0.0) as i64;
        let last_frame = (visible_end_secs * fps).ceil().max(0.0) as i64;
        for frame in (first_frame..=last_frame).step_by(medium_interval_frames as usize) {
            // Skips the positions where there is already a major tick (redundant).
            let secs = frame as f64 / fps;
            let major_at_this_pos = ((secs / major_secs).round() * major_secs - secs).abs() < 1e-9;
            if major_at_this_pos {
                continue;
            }
            let x = origin.x + frame as f32 * px_per_frame;
            painter.line_segment(
                [
                    egui::pos2(x, origin.y + RULER_HEIGHT - MEDIUM_TICK_HEIGHT),
                    egui::pos2(x, origin.y + RULER_HEIGHT),
                ],
                egui::Stroke::new(1.0, medium_color),
            );
        }
    }

    // Level 0: ticks for every single frame — the shortest ones, visible only
    // when the zoom is high enough not to make them touch.
    if px_per_frame >= MIN_TICK_SPACING_PX {
        let first_frame = (visible_start_secs * fps).floor().max(0.0) as i64;
        let last_frame = (visible_end_secs * fps).ceil().max(0.0) as i64;
        for frame in first_frame..=last_frame {
            // Skips the positions where there is already a medium or major tick.
            let is_medium_pos = frame % medium_interval_frames == 0;
            let secs = frame as f64 / fps;
            let is_major_pos = ((secs / major_secs).round() * major_secs - secs).abs() < 1e-9;
            if is_medium_pos || is_major_pos {
                continue;
            }
            let x = origin.x + frame as f32 * px_per_frame;
            painter.line_segment(
                [
                    egui::pos2(x, origin.y + RULER_HEIGHT - FRAME_TICK_HEIGHT),
                    egui::pos2(x, origin.y + RULER_HEIGHT),
                ],
                egui::Stroke::new(1.0, minor_color),
            );
        }
    }
}

/// Payload of a media drag&drop towards the timeline: from the media pool
/// (the whole media) or from the viewer (the portion between the in/out markers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaDrag {
    pub media_id: vv_core::MediaId,
    pub source_in: FrameIdx,
    pub source_out: FrameIdx,
    pub streams: DragStreams,
}

/// Which streams of the media end up on the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DragStreams {
    #[default]
    All,
    VideoOnly,
    AudioOnly,
}

impl MediaDrag {
    /// Initial duration of an image from the pool (its `duration_frames` is a
    /// sentinel), like the generators.
    const DEFAULT_IMAGE_SECS: f64 = 5.0;

    pub fn whole(media_id: vv_core::MediaId, meta: &vv_core::MediaMeta) -> Self {
        let source_out = if meta.is_image() {
            (meta.fps.as_f64() * Self::DEFAULT_IMAGE_SECS).round() as FrameIdx
        } else {
            meta.duration_frames
        };
        Self {
            media_id,
            source_in: 0,
            source_out,
            streams: DragStreams::All,
        }
    }

    pub fn takes_video(&self, meta: &vv_core::MediaMeta) -> bool {
        meta.has_video && self.streams != DragStreams::AudioOnly
    }

    pub fn takes_audio(&self, meta: &vv_core::MediaMeta) -> bool {
        meta.has_audio && self.streams != DragStreams::VideoOnly
    }

    /// Duration in *source* frames (media fps): the in/out markers
    /// of the preview live in that space.
    pub fn source_len(&self) -> FrameIdx {
        self.source_out - self.source_in
    }

    /// How much it will occupy on the timeline, conformed to `rate` (see
    /// `Clip::rate`): what matters for the drop ghost and for the
    /// snapping, which work in timeline frames.
    pub fn timeline_len(&self, rate: vv_core::Rational) -> FrameIdx {
        rate.scale_round(self.source_out) - rate.scale_round(self.source_in)
    }
}

/// Actual payload of the drag&drop: several media selected together in the
/// media pool are appended onto the timeline in the order they appear
/// there, so the payload is an ordered list, not a single media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDragSet {
    pub items: Vec<MediaDrag>,
}

impl MediaDragSet {
    pub fn one(drag: MediaDrag) -> Self {
        Self { items: vec![drag] }
    }
}

/// Effects of the Effects panel: they generate a clip without a source media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generator {
    SolidColor,
    Text,
}

impl Generator {
    pub const ALL: [Generator; 2] = [Generator::SolidColor, Generator::Text];

    const DEFAULT_SECS: f64 = 5.0;

    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Generator::SolidColor => t!("generator.solid_color"),
            Generator::Text => t!("generator.text"),
        }
    }

    pub fn default_len(self, timeline_fps: vv_core::Rational) -> FrameIdx {
        (timeline_fps.as_f64() * Self::DEFAULT_SECS).round() as FrameIdx
    }
}

/// What is being dragged towards the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineDrag {
    Media(MediaDragSet),
    Generator(Generator),
}

impl TimelineDrag {
    pub fn hovered(resp: &egui::Response) -> Option<Self> {
        resp.dnd_hover_payload::<MediaDragSet>()
            .map(|set| Self::Media((*set).clone()))
            .or_else(|| resp.dnd_hover_payload::<Generator>().map(|g| Self::Generator(*g)))
    }

    pub fn released(resp: &egui::Response) -> Option<Self> {
        // `take_payload` discards the payload even if the type does not match:
        // the right type must be chosen before taking it. A `FilterKind`, a
        // `TransitionKind` or a whole `Transition` (duplication via
        // Alt+drag) is never a `TimelineDrag` (they are dropped only on
        // a clip, handled in the clip loop): if one of those is in
        // progress, exit immediately, otherwise the `MediaDragSet` branch below would
        // take and destroy it without being able to interpret it, and the
        // drop on the clip would no longer see anything (see the same oversight
        // already made and repaired for `FilterKind`).
        if egui::DragAndDrop::has_payload_of_type::<vv_core::FilterKind>(&resp.ctx)
            || egui::DragAndDrop::has_payload_of_type::<vv_core::TransitionKind>(&resp.ctx)
            || egui::DragAndDrop::has_payload_of_type::<vv_core::Transition>(&resp.ctx)
        {
            return None;
        }
        if egui::DragAndDrop::has_payload_of_type::<Generator>(&resp.ctx) {
            resp.dnd_release_payload::<Generator>().map(|g| Self::Generator(*g))
        } else {
            resp.dnd_release_payload::<MediaDragSet>()
                .map(|set| Self::Media((*set).clone()))
        }
    }

    fn is_media(&self) -> bool {
        matches!(self, Self::Media(_))
    }
}

/// Filters of the Effects panel, in the order they appear there: unlike the
/// `Generator`s, they apply to an existing video clip instead of
/// generating a new one, and for this reason they stay outside `TimelineDrag`
/// (no ghost on the empty zones, no new tracks). The type shared
/// with `EffectStack::filters` (`vv_core::FilterKind`) remains the single source
/// of truth on "which filters exist": here only their label.
pub const ALL_FILTER_KINDS: [vv_core::FilterKind; 1] = [vv_core::FilterKind::Grayscale];

pub fn filter_label(kind: vv_core::FilterKind) -> std::borrow::Cow<'static, str> {
    match kind {
        vv_core::FilterKind::Grayscale => t!("filter.grayscale"),
    }
}

/// How much the dragged media occupies on the timeline: its duration in
/// source frames conformed to the timeline fps (see `Clip::rate`).
/// `1/1` if the media is not (any longer) in the pool.
fn drag_timeline_len(
    project: &Project,
    timeline_fps: vv_core::Rational,
    drag: &MediaDrag,
) -> FrameIdx {
    let rate = project
        .media_pool
        .get(drag.media_id)
        .map(|item| vv_core::Rational::conform_rate(timeline_fps, item.meta.fps))
        .unwrap_or_else(vv_core::Rational::one);
    drag.timeline_len(rate)
}

/// Total length of the drop: the media appended one after the other.
fn drag_set_timeline_len(
    project: &Project,
    timeline_fps: vv_core::Rational,
    drag: &TimelineDrag,
) -> FrameIdx {
    match drag {
        TimelineDrag::Media(set) => set
            .items
            .iter()
            .map(|d| drag_timeline_len(project, timeline_fps, d))
            .sum(),
        TimelineDrag::Generator(g) => g.default_len(timeline_fps),
    }
}

/// A segment of the drop ghost: the media are appended, so the ghost
/// shows them separated.
struct DragSegment {
    offset: FrameIdx,
    len: FrameIdx,
    has_video: bool,
    has_audio: bool,
}

fn drag_set_segments(
    project: &Project,
    timeline_fps: vv_core::Rational,
    drag: &TimelineDrag,
) -> Vec<DragSegment> {
    let set = match drag {
        TimelineDrag::Media(set) => set,
        TimelineDrag::Generator(g) => {
            return vec![DragSegment {
                offset: 0,
                len: g.default_len(timeline_fps),
                has_video: true,
                has_audio: false,
            }];
        }
    };
    let mut offset = 0;
    set.items
        .iter()
        .filter(|d| project.media_pool.contains_key(d.media_id))
        .map(|d| {
            let len = drag_timeline_len(project, timeline_fps, d);
            let seg = DragSegment {
                offset,
                len,
                has_video: d.takes_video(&project.media_pool[d.media_id].meta),
                has_audio: d.takes_audio(&project.media_pool[d.media_id].meta),
            };
            offset += len;
            seg
        })
        .collect()
}

/// Where a drop goes: the usual track, a new one (band above the Video ones or
/// below the Audio ones) or a precise video track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaDropTarget {
    Default,
    NewVideoTrack,
    NewAudioTrack,
    /// Existing video track under the pointer (drop of an effect or of
    /// a media with video, when the pointer is on a video track already
    /// present).
    Track(usize),
}

/// `Some((drag, frame, target))` if in this frame something dragged from the
/// media pool or from the Effects panel was released; the caller
/// takes care of it.
pub fn show_timeline(
    ui: &mut egui::Ui,
    project: &mut Project,
    history: &mut History,
    timeline_id: TimelineId,
    media_labels: &dyn Fn(vv_core::MediaId) -> String,
    state: &mut TimelineState,
    snapping_enabled: bool,
    kinetic_scroll_enabled: bool,
    // Timeline intervals already cached: "buffered" strip in the ruler.
    buffered_ranges: &[(FrameIdx, FrameIdx)],
    // Timeline intervals of the clips served by the proxy: strip on the clip.
    proxy_ranges: &[(FrameIdx, FrameIdx)],
    // Loaded waveforms, per `(content_hash, stream_index)`.
    waveform_cache: &std::collections::HashMap<(u64, usize), vv_media::Waveform>,
    // The timeline is playing: during playback the playhead must
    // always stay visible, so the view "turns the page" to
    // follow it when it leaves the visible area (see below).
    playback_active: bool,
) -> (Option<(TimelineDrag, FrameIdx, MediaDropTarget)>, Option<TimelineId>) {
    let mut media_drop = None;
    // Double click on a compound clip: the caller (main.rs) opens it
    // as a timeline of its own (see `VenturiApp::enter_compound_timeline`).
    let mut enter_compound = None;

    // Alt+scroll/pinch zooms only with the pointer over the timeline.
    let panel_rect = ui.available_rect_before_wrap();
    let pointer_over_panel = pointer_over(ui.ctx(), panel_rect);
    if pointer_over_panel {
        let zoom = ui.input(|i| i.zoom_delta());
        if zoom != 1.0 {
            state.set_pixels_per_sec(state.pixels_per_sec * zoom);
        }
    }

    // A new touch immediately interrupts any residual inertia, as on
    // a real touchpad: a click/tap (1 finger), the start of a scroll
    // gesture (2 fingers, `TouchPhase::Start`) before it even produces a
    // delta, or even just the pointer moving — resting the fingers
    // on the touchpad without lifting them generates no dedicated event (the
    // system reports only changes, not "fingers at rest"), but a real touch
    // is never perfectly still: a micro-movement of the pointer is
    // the signal left to us to notice it.
    let touch_started = ui.input(|i| {
        i.pointer.delta() != egui::Vec2::ZERO
            || (pointer_over_panel
                && (i.pointer.any_pressed()
                    || i.events.iter().any(|e| {
                        matches!(e, egui::Event::MouseWheel { phase: egui::TouchPhase::Start, .. })
                    })))
    });
    if touch_started {
        state.video_scroll_vel = 0.0;
        state.audio_scroll_vel = 0.0;
        state.hscroll_vel = 0.0;
    }

    let timeline_fps = project.timelines[timeline_id].fps;
    let fps = timeline_fps.as_f64();
    let px_per_frame = state.pixels_per_sec / fps.max(1.0) as f32;

    // --- pass 1: collect the data to draw (immutable borrow) ---
    let (track_count, track_kinds, visuals, max_end_frames) = {
        let tl = &project.timelines[timeline_id];
        let mut visuals = Vec::new();
        let mut max_end: FrameIdx = 0;
        for (track_index, track) in tl.tracks.iter().enumerate() {
            for clip in &track.clips {
                max_end = max_end.max(clip.timeline_end());
                let offline = matches!(
                    &clip.source,
                    ClipSource::Media(id) if !project.media_pool.contains_key(*id)
                );
                let (label, color) = clip_label_and_color(clip, track, offline, media_labels);
                visuals.push(ClipVisual {
                    track_index,
                    clip: std::borrow::Cow::Borrowed(clip),
                    label,
                    color,
                    locked: track.locked,
                    muted: clip.disabled || (track.kind == TrackKind::Video && track.muted),
                });
            }
        }
        let track_kinds: Vec<TrackKind> = tl.tracks.iter().map(|t| t.kind).collect();
        (tl.tracks.len(), track_kinds, visuals, max_end)
    };
    let track_labels: Vec<String> = (0..track_count)
        .map(|i| project.timelines[timeline_id].track_label(i))
        .collect();
    let track_flags: Vec<TrackFlags> = project.timelines[timeline_id]
        .tracks
        .iter()
        .map(|t| TrackFlags {
            muted: t.muted,
            solo: t.solo,
            locked: t.locked,
        })
        .collect();
    let track_locked = |track_index: usize| track_flags.get(track_index).is_some_and(|f| f.locked);
    state.drop_locked(&project.timelines[timeline_id]);

    let total_secs = (max_end_frames as f64 / fps + TRAILING_MARGIN_SECS).max(MIN_TIMELINE_SECS);
    // At low zoom the natural content is narrower than the panel: we force
    // at least `viewport_width` so the ruler always reaches the edge.
    let viewport_width = (panel_rect.width() - TRACK_HEADER_WIDTH).max(1.0);
    let content_width =
        ((total_secs * state.pixels_per_sec as f64) as f32).max(viewport_width);

    let row_order = track_row_order(&track_kinds);
    let mut row_of_track = vec![0usize; track_count];
    for (row, &track_index) in row_order.iter().enumerate() {
        row_of_track[track_index] = row;
    }
    let video_count = track_kinds.iter().filter(|k| **k == TrackKind::Video).count();
    let audio_count = track_count - video_count;
    let avail_below_ruler = (panel_rect.height() - RULER_HEIGHT).max(0.0);
    let mut layout = PaneLayout::new(avail_below_ruler, video_count, audio_count, state);
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    if !kinetic_scroll_enabled {
        state.video_scroll_vel = 0.0;
        state.audio_scroll_vel = 0.0;
    }

    // Residual inertia from a touchpad swipe just finished: it keeps
    // scrolling and braking, even if the pointer moved in the meantime.
    let video_coasted =
        apply_kinetic_scroll(&mut state.video_scroll, &mut state.video_scroll_vel, layout.video_max_scroll, dt);
    let audio_coasted =
        apply_kinetic_scroll(&mut state.audio_scroll, &mut state.audio_scroll_vel, layout.audio_max_scroll, dt);
    if video_coasted || audio_coasted {
        layout = PaneLayout::new(avail_below_ruler, video_count, audio_count, state);
        ui.ctx().request_repaint();
    }

    // Wheel: vertical scroll of the box under the pointer (the
    // horizontal one stays on Shift+wheel, as the ScrollArea already did).
    if let Some(pos) = ui.input(|i| i.pointer.hover_pos())
        && pointer_over(ui.ctx(), panel_rect)
    {
        let local_y = pos.y - panel_rect.top();
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if wheel != 0.0 {
            let scrolled = if layout.video_pane.contains(local_y) && layout.video_max_scroll > 0.0 {
                state.video_scroll += wheel;
                state.video_scroll_vel =
                    if kinetic_scroll_enabled && dt > 0.0 { KINETIC_VELOCITY_GAIN * wheel / dt } else { 0.0 };
                true
            } else if layout.audio_pane.contains(local_y) && layout.audio_max_scroll > 0.0 {
                state.audio_scroll -= wheel;
                state.audio_scroll_vel =
                    if kinetic_scroll_enabled && dt > 0.0 { -KINETIC_VELOCITY_GAIN * wheel / dt } else { 0.0 };
                true
            } else {
                false
            };
            if scrolled {
                ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
                layout = PaneLayout::new(avail_below_ruler, video_count, audio_count, state);
            }
        }
    }
    let divider_height = layout.divider_height;
    let visual_height = layout.audio_pane.max;
    let pane_of = |track_index: usize| layout.pane(track_kinds[track_index]);

    // Local `y` of every track (indexed by `track_index`), consistent with
    // `clip_local_rect`.
    let row_y: Vec<f32> = (0..track_count)
        .map(|track_index| layout.row_y(row_of_track[track_index]))
        .collect();
    let track_at_y = |local_y: f32| -> usize { row_order[layout.row_at_y(local_y)] };

    let mut pending: Option<PendingAction> = None;
    // Taken from `state.volume_drag` before the reset further below clears it, to
    // close the undo group after `apply_pending_action` has applied
    // the last `SetGain` of the drag (see `VolumeDragState::group`).
    let mut volume_drag_group: Option<vv_core::GroupMark> = None;

    ui.horizontal_top(|ui| {
        // `Id::with(IdSalt)` and `Id::with(&str)` give different ids: the form
        // the ScrollArea uses in `begin` is needed.
        let scroll_id = ui.make_persistent_id(egui::IdSalt::new("timeline_scroll"));
        let scroll_viewport_width =
            (ui.available_rect_before_wrap().width() - TRACK_HEADER_WIDTH).max(1.0);
        sync_timeline_scroll(
            ui.ctx(),
            scroll_id,
            state,
            fps,
            px_per_frame,
            scroll_viewport_width,
            content_width,
            playback_active,
            panel_rect,
            kinetic_scroll_enabled,
        );

        draw_track_headers(
            ui,
            &track_kinds,
            &track_labels,
            &track_flags,
            &row_order,
            &row_y,
            &layout,
            &mut state.video_pane_height,
            &mut pending,
            state.playhead,
            fps,
        );

        egui::ScrollArea::horizontal()
            .id_salt("timeline_scroll")
            // `auto_shrink` off: otherwise the panel closes back to the content and
            // its resize springs back.
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let (rect, _resp) = ui.allocate_exact_size(
                    egui::vec2(content_width, RULER_HEIGHT),
                    egui::Sense::hover(),
                );
                let origin = rect.min;
                // Not allocated: the height of the boxes depends on the panel,
                // and allocating it would make `Panel::bottom` grow indefinitely.
                let visual_rect = egui::Rect::from_min_size(
                    origin,
                    egui::vec2(content_width, visual_height),
                );
                let painter = ui.painter_at(visual_rect);
                let to_local = |pos: egui::Pos2| egui::pos2(pos.x - origin.x, pos.y - origin.y);
                let pane_rect = |pane: egui::Rangef| {
                    egui::Rect::from_x_y_ranges(
                        visual_rect.x_range(),
                        (origin.y + pane.min)..=(origin.y + pane.max),
                    )
                };
                let track_pane_rect = |track_index: usize| pane_rect(pane_of(track_index));
                let track_painter = |track_index: usize| {
                    painter.with_clip_rect(painter.clip_rect().intersect(track_pane_rect(track_index)))
                };
                let local_pane_rect = |track_index: usize| {
                    track_pane_rect(track_index).translate(-origin.to_vec2())
                };
                // Only the part of the clip visible in its box.
                let visible_clip_rect = |v: &ClipVisual| {
                    clip_local_rect(v, px_per_frame, &row_y).intersect(local_pane_rect(v.track_index))
                };
                let press_over_a_clip = |pos: egui::Pos2| {
                    let local = to_local(pos);
                    visuals.iter().any(|v| visible_clip_rect(v).contains(local))
                };

                show_ruler(
                    ui,
                    &painter,
                    origin,
                    content_width,
                    state,
                    &visuals,
                    fps,
                    px_per_frame,
                    snapping_enabled,
                    buffered_ranges,
                    max_end_frames,
                );

                // Track backgrounds and, above them, an interactable area for clicks and
                // a rubber band from the empty space. The clips are interacted with afterwards and win the hit-test.
                for (row, &track_index) in row_order.iter().enumerate() {
                    let y = origin.y + row_y[track_index];
                    let track_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x, y),
                        egui::vec2(content_width, ROW_HEIGHT),
                    );
                    let bg = match (track_locked(track_index), row % 2 == 0) {
                        (true, _) => egui::Color32::from_gray(42),
                        (false, true) => egui::Color32::from_gray(32),
                        (false, false) => egui::Color32::from_gray(27),
                    };
                    track_painter(track_index).rect_filled(track_rect, 0.0, bg);
                }
                let over_rows = |pos: egui::Pos2| {
                    visual_rect.x_range().contains(pos.x) && layout.is_over_rows(pos.y - origin.y)
                };
                // The empty zones too: the selection rectangle can
                // start from there.
                let marquee_area_rect = egui::Rect::from_min_max(
                    egui::pos2(origin.x, origin.y + RULER_HEIGHT),
                    egui::pos2(origin.x + content_width, origin.y + visual_height),
                );
                let pointer_over_tracks = ui
                    .input(|i| i.pointer.hover_pos())
                    .is_some_and(over_rows);
                let marquee_resp = ui.interact(
                    marquee_area_rect,
                    ui.id().with("timeline_marquee"),
                    egui::Sense::click_and_drag(),
                );

                // Interacted with *after* `marquee_resp` to win the hit-test on
                // this thin band (same pattern as the clips below).
                if divider_height > 0.0 {
                    let divider_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x, origin.y + layout.video_pane.max),
                        egui::vec2(content_width, divider_height),
                    );
                    interact_divider(
                        ui,
                        &painter,
                        divider_rect,
                        ui.id().with("timeline_track_split"),
                        &layout,
                        &mut state.video_pane_height,
                    );
                }

                // `dnd_*_payload` look at `contains_pointer`: they work even if the
                // drag started from another widget. The media with video or the effect
                // land on the video track under the pointer.
                let drop_target = |drag: &TimelineDrag, pos: egui::Pos2| {
                    let track = track_at_y(pos.y - origin.y);
                    let has_video = match drag {
                        TimelineDrag::Generator(_) => true,
                        TimelineDrag::Media(set) => set
                            .items
                            .iter()
                            .any(|d| d.takes_video(&project.media_pool[d.media_id].meta)),
                    };
                    media_pool_drop_target(track, has_video, track_kinds[track], track_locked(track))
                };
                // Where a drop falls: the frame under the pointer, with the snapping.
                let playhead = state.playhead;
                let drop_frame = |drag: &TimelineDrag, pos: egui::Pos2| {
                    let raw = (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
                    snap_frame(
                        raw,
                        drag_set_timeline_len(project, timeline_fps, drag),
                        &visuals,
                        &[],
                        &[playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .max(0)
                };
                // Layer above the clips, painted further on.
                let ghost_painter = painter.clone().with_layer_id(egui::LayerId::new(
                    egui::Order::Foreground,
                    ui.id().with("timeline_drop_ghost"),
                ));
                if pointer_over_tracks
                    && let Some(drag) = TimelineDrag::hovered(&marquee_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                    && let Some(target) = drop_target(&drag, pos)
                    && !drag_set_segments(project, timeline_fps, &drag).is_empty()
                {
                    let frame = drop_frame(&drag, pos);
                    // The tracks where `insert_media_clip` puts video and audio.
                    let first_row = |kind| {
                        (0..track_count)
                            .find(|&t| track_kinds[t] == kind && !track_locked(t))
                            .map(|t| origin.y + row_y[t])
                    };
                    let video_y = match target {
                        MediaDropTarget::Track(track) => Some(origin.y + row_y[track]),
                        _ => first_row(TrackKind::Video),
                    };
                    let audio_y = first_row(TrackKind::Audio);
                    for seg in drag_set_segments(project, timeline_fps, &drag) {
                        let x = origin.x + (frame + seg.offset) as f32 * px_per_frame;
                        let rows = [
                            (seg.has_video, video_y, layout.video_pane),
                            (seg.has_audio, audio_y, layout.audio_pane),
                        ];
                        for (_, y, pane) in rows.into_iter().filter(|(present, _, _)| *present) {
                            let Some(y) = y else { continue };
                            let ghost_painter = ghost_painter
                                .with_clip_rect(ghost_painter.clip_rect().intersect(pane_rect(pane)));
                            let rect = egui::Rect::from_min_size(
                                egui::pos2(x, y),
                                egui::vec2(seg.len as f32 * px_per_frame, ROW_HEIGHT),
                            )
                            // A sliver of margin between one segment and the
                            // next: without it the edges meet and the appended
                            // clips look like a single block.
                            .shrink2(egui::vec2(1.0, 0.0));
                            ghost_painter.rect_filled(
                                rect,
                                4.0,
                                egui::Color32::from_rgba_unmultiplied(120, 220, 120, 90),
                            );
                            ghost_painter.rect_stroke(
                                rect,
                                4.0,
                                egui::Stroke::new(2.0, egui::Color32::from_rgb(120, 220, 120)),
                                egui::StrokeKind::Inside,
                            );
                        }
                    }
                }
                if pointer_over_tracks
                    && let Some(drag) = TimelineDrag::released(&marquee_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                    && let Some(target) = drop_target(&drag, pos)
                {
                    let frame = drop_frame(&drag, pos);
                    media_drop = Some((drag, frame, target));
                }

                // Ghost of a filter: a gear instead of the green rectangle,
                // anywhere on the timeline (not only on the tracks, as above: a
                // filter never lands on an empty space, but the cursor
                // stays consistent anyway while passing over it).
                if pointer_over_panel
                    && egui::DragAndDrop::has_payload_of_type::<vv_core::FilterKind>(ui.ctx())
                    && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                {
                    paint_gear_icon(&ghost_painter, pos + egui::vec2(14.0, 14.0), 10.0, egui::Color32::WHITE);
                }

                // Same gear ghost as the filters: a transition
                // too (from the panel or duplicated with Alt+drag)
                // lands only on an existing clip, never on an empty
                // space.
                if pointer_over_panel
                    && (egui::DragAndDrop::has_payload_of_type::<vv_core::TransitionKind>(ui.ctx())
                        || egui::DragAndDrop::has_payload_of_type::<vv_core::Transition>(ui.ctx()))
                    && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                {
                    paint_gear_icon(&ghost_painter, pos + egui::vec2(14.0, 14.0), 10.0, egui::Color32::WHITE);
                }

                // "Add a new track" zones: margins above/below the
                // groups (zero height if there is no margin, see above).
                let above_video_rect = egui::Rect::from_min_max(
                    egui::pos2(origin.x, origin.y + layout.video_pane.min),
                    egui::pos2(
                        origin.x + content_width,
                        origin.y + layout.video_rows_top.max(layout.video_pane.min),
                    ),
                );
                let above_video_resp = ui.interact(
                    above_video_rect,
                    ui.id().with("timeline_new_video_track_zone"),
                    egui::Sense::hover(),
                );
                if TimelineDrag::hovered(&above_video_resp).is_some() {
                    paint_drop_zone(&painter, above_video_rect, Some(&t!("timeline.new_video_track")));
                }
                if let Some(drag) = TimelineDrag::released(&above_video_resp)
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                {
                    let frame = drop_frame(&drag, pos);
                    media_drop = Some((drag, frame, MediaDropTarget::NewVideoTrack));
                }

                let below_audio_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        origin.x,
                        origin.y + layout.audio_rows_bottom().min(layout.audio_pane.max),
                    ),
                    egui::pos2(origin.x + content_width, origin.y + layout.audio_pane.max),
                );
                let below_audio_resp = ui.interact(
                    below_audio_rect,
                    ui.id().with("timeline_new_audio_track_zone"),
                    egui::Sense::hover(),
                );
                if TimelineDrag::hovered(&below_audio_resp).is_some_and(|d| d.is_media()) {
                    paint_drop_zone(&painter, below_audio_rect, Some(&t!("timeline.new_audio_track")));
                }
                if let Some(drag) = TimelineDrag::released(&below_audio_resp)
                    && drag.is_media()
                    && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
                {
                    let frame = drop_frame(&drag, pos);
                    media_drop = Some((drag, frame, MediaDropTarget::NewAudioTrack));
                }

                if marquee_resp.drag_started() {
                    if let Some(pos) = marquee_resp.interact_pointer_pos()
                        && !press_over_a_clip(pos)
                    {
                        let local = to_local(pos);
                        state.marquee = Some(MarqueeDrag {
                            start: local,
                            current: local,
                        });
                    }
                } else if marquee_resp.dragged() {
                    if let (Some(m), Some(pos)) =
                        (&mut state.marquee, marquee_resp.interact_pointer_pos())
                    {
                        m.current = to_local(pos);
                    }
                } else if marquee_resp.drag_stopped() {
                    if let Some(m) = state.marquee.take() {
                        let rect = egui::Rect::from_two_pos(m.start, m.current);
                        let hits: Vec<ClipKey> = visuals
                            .iter()
                            .filter(|v| !v.locked && visible_clip_rect(v).intersects(rect))
                            .map(|v| (v.track_index, v.clip.id))
                            .collect();
                        state.selected = expand_to_linked_groups(&visuals, hits.iter().copied());
                        state.selection_anchor = hits.first().copied();
                        state.selected_gap = None;
                        state.selected_transition = None;
                    }
                } else if marquee_resp.clicked()
                    && let Some(pos) = marquee_resp.interact_pointer_pos()
                    && !press_over_a_clip(pos)
                {
                    if row_order.is_empty() || !over_rows(pos) {
                        state.clear_selection();
                    } else {
                        // Click on a gap followed by a clip: it gets selected.
                        let local = to_local(pos);
                        let frame = ((local.x / px_per_frame).round() as FrameIdx).max(0);
                        let track_index = track_at_y(local.y);
                        match gap_at(&visuals, track_index, frame)
                            .filter(|_| !track_locked(track_index))
                        {
                            Some((gap_start, gap_end)) => {
                                state.selected.clear();
                                state.selection_anchor = None;
                                state.selected_gap = Some((track_index, gap_start, gap_end));
                                state.selected_transition = None;
                            }
                            None => state.clear_selection(),
                        }
                    }
                }
                if let Some(m) = &state.marquee {
                    let marquee_rect = egui::Rect::from_two_pos(
                        origin + m.start.to_vec2(),
                        origin + m.current.to_vec2(),
                    );
                    painter.rect_filled(
                        marquee_rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(100, 150, 255, 40),
                    );
                    painter.rect_stroke(
                        marquee_rect,
                        0.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 150, 255)),
                        egui::StrokeKind::Inside,
                    );
                }

                // Selected gap: same frame as a selected clip.
                if let Some((track_index, gap_start, gap_end)) = state.selected_gap
                    && let Some(&row_y_val) = row_y.get(track_index)
                {
                    let y = origin.y + row_y_val;
                    let gap_rect = egui::Rect::from_min_size(
                        egui::pos2(origin.x + gap_start as f32 * px_per_frame, y + 2.0),
                        egui::vec2(
                            (gap_end - gap_start) as f32 * px_per_frame,
                            ROW_HEIGHT - 4.0,
                        ),
                    );
                    let painter = track_painter(track_index);
                    painter.rect_filled(
                        gap_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30),
                    );
                    painter.rect_stroke(
                        gap_rect,
                        4.0,
                        egui::Stroke::new(2.0, egui::Color32::WHITE),
                        egui::StrokeKind::Inside,
                    );
                }

                // Candidate track of the drag, from the current pointer position.
                let drag_effective_track = state.drag.as_ref().map(|d| {
                    let kind = track_kinds[d.track_index];
                    let target = ui.input(|i| i.pointer.interact_pos()).and_then(|pos| {
                        track_drag_target(
                            to_local(pos).y,
                            kind,
                            &row_order,
                            &layout,
                        )
                    });
                    match target {
                        Some(TrackDragTarget::Track(idx)) if !track_locked(idx) => {
                            EffectiveTrack::Existing(idx)
                        }
                        Some(TrackDragTarget::NewTrack) => EffectiveTrack::New(1),
                        _ => EffectiveTrack::Existing(d.track_index),
                    }
                });
                if let (Some(d), Some(EffectiveTrack::New(_))) = (&state.drag, drag_effective_track) {
                    let rect = match track_kinds[d.track_index] {
                        TrackKind::Video => above_video_rect,
                        TrackKind::Audio => below_audio_rect,
                    };
                    paint_drop_zone(&painter, rect, None);
                }

                // See `drag_group_row_targets`.
                // If any clip of the group would land on a locked
                // track, the group stays on its own tracks.
                let drag_group_targets: Option<Vec<(ClipId, EffectiveTrack)>> =
                    state.drag.as_ref().map(|d| {
                        let targets_for = |primary_target| {
                            drag_group_row_targets(
                                d.clip_id,
                                d.track_index,
                                primary_target,
                                &d.followers,
                                &track_kinds,
                                &row_of_track,
                                &row_order,
                                video_count,
                                track_count,
                            )
                        };
                        let targets = targets_for(drag_effective_track.unwrap());
                        if targets.iter().any(|(_, t)| {
                            matches!(t, EffectiveTrack::Existing(track) if track_locked(*track))
                        }) {
                            targets_for(EffectiveTrack::Existing(d.track_index))
                        } else {
                            targets
                        }
                    });

                // Position of the dragged primary (clamped and snapped), once
                // for the whole group and for the preview during the drag.
                let dragged_primary_new_start = state.drag.as_ref().map(|d| {
                    let raw = d.original_start as f32 + d.accum_px / px_per_frame;
                    let raw_rounded = raw.round() as FrameIdx;
                    let len = visuals
                        .iter()
                        .find(|v| v.clip.id == d.clip_id)
                        .map(|v| v.clip.timeline_len)
                        .unwrap_or(0);
                    let (min_start, max_start) = group_drag_bounds(
                        &visuals,
                        raw_rounded,
                        drag_group_targets.as_deref().unwrap(),
                        &d.followers,
                    );
                    let candidate = raw_rounded.clamp(min_start, max_start);
                    let mut exclude = vec![d.clip_id];
                    exclude.extend(d.followers.iter().map(|(id, _, _)| *id));
                    snap_frame(
                        candidate,
                        len,
                        &visuals,
                        &exclude,
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    )
                    .clamp(min_start, max_start)
                });

                // As above for the edge of a trim, which changes the length too.
                let trimmed_primary_new_value = state.trim.as_ref().map(|t| {
                    let raw = t.original_value as f32 + t.accum_px / px_per_frame;
                    let exclude: Vec<ClipId> = std::iter::once(t.clip_id)
                        .chain(t.followers.iter().map(|&(id, _, _, _)| id))
                        .collect();
                    let snapped = snap_frame(
                        raw.round() as FrameIdx,
                        0,
                        &visuals,
                        &exclude,
                        &[state.playhead],
                        px_per_frame,
                        snapping_enabled,
                    );
                    snapped.clamp(t.min_value, t.max_value)
                });

                // `drag`/`trim` are cleared only after the loop: the clips of the group
                // drawn after the primary would go back for one frame to the initial
                // position.
                let mut drag_finished = false;
                let mut trim_finished = false;
                let mut fade_drag_finished = false;
                let mut transition_drag_finished = false;
                let mut crossing_drag_finished = false;
                let mut volume_drag_finished = false;
                let mut edge_cursor: Option<(egui::Pos2, EdgeCursor)> = None;
                // The mirror marker on the neighbor must be drawn after the whole loop,
                // not during the iteration of the clip under the pointer: if the
                // neighbor comes later in `draw_order` (the common case, more recent
                // clips have higher ids), its own `paint_clip_box` would
                // cover it immediately — see the comment on `drag_finished` above
                // for the same structural reason.
                let mut pending_crossing_previews: Vec<(usize, ClipId, FadeEdge)> = Vec::new();

                // The moving clips are drawn last: they invade the others.
                let trimmed_keys: Vec<ClipKey> = state
                    .trim
                    .as_ref()
                    .map(|t| {
                        std::iter::once((t.track_index, t.clip_id))
                            .chain(t.followers.iter().map(|&(id, track, _, _)| (track, id)))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut moving_keys = trimmed_keys.clone();
                if let Some(d) = &state.drag {
                    moving_keys.push((d.track_index, d.clip_id));
                    moving_keys.extend(d.followers.iter().map(|&(id, track, _)| (track, id)));
                }
                let draw_order: Vec<&ClipVisual> = visuals
                    .iter()
                    .filter(|v| !moving_keys.contains(&(v.track_index, v.clip.id)))
                    .chain(
                        visuals
                            .iter()
                            .filter(|v| moving_keys.contains(&(v.track_index, v.clip.id))),
                    )
                    .collect();
                // When duplicating, the originals stay visible in their place.
                if state.drag.as_ref().is_some_and(|d| d.duplicate) {
                    for visual in visuals
                        .iter()
                        .filter(|v| moving_keys.contains(&(v.track_index, v.clip.id)))
                    {
                        let clip_rect = egui::Rect::from_min_size(
                            egui::pos2(
                                origin.x + visual.clip.timeline_start as f32 * px_per_frame,
                                origin.y + row_y[visual.track_index] + 2.0,
                            ),
                            egui::vec2(
                                (visual.clip.timeline_len as f32 * px_per_frame).max(2.0),
                                ROW_HEIGHT - 4.0,
                            ),
                        );
                        let painter = track_painter(visual.track_index);
                        painter.rect_filled(clip_rect, 4.0, visual.color);
                        painter.rect_stroke(
                            clip_rect,
                            4.0,
                            egui::Stroke::new(1.0, egui::Color32::from_gray(15)),
                            egui::StrokeKind::Inside,
                        );
                        painter.text(
                            clip_rect.left_top() + egui::vec2(4.0, 2.0),
                            egui::Align2::LEFT_TOP,
                            &visual.label,
                            egui::FontId::proportional(12.0),
                            egui::Color32::BLACK,
                        );
                    }
                }
                for visual in draw_order {
                    let painter = track_painter(visual.track_index);
                    let is_trimming_this = trimmed_keys.contains(&(visual.track_index, visual.clip.id));
                    let (display_start, display_len) = display_range(
                        visual,
                        state,
                        is_trimming_this,
                        trimmed_primary_new_value,
                        dragged_primary_new_start,
                    );

                    let x = origin.x + display_start as f32 * px_per_frame;
                    // During a drag changing track, the preview of every
                    // clip of the group follows its own target (see
                    // `drag_group_targets`) instead of the starting track.
                    let this_target = drag_group_targets
                        .as_ref()
                        .and_then(|targets| targets.iter().find(|(id, _)| *id == visual.clip.id));
                    let y = match this_target {
                        Some((_, EffectiveTrack::Existing(track))) => origin.y + row_y[*track],
                        // Every "depth" stacks another row past the
                        // current edge (see `EffectiveTrack::New`).
                        Some((_, EffectiveTrack::New(depth))) => match track_kinds[visual.track_index] {
                            TrackKind::Video => {
                                origin.y + layout.video_rows_top - *depth as f32 * ROW_HEIGHT
                            }
                            TrackKind::Audio => {
                                origin.y + layout.audio_rows_bottom() + (*depth - 1) as f32 * ROW_HEIGHT
                            }
                        },
                        None => origin.y + row_y[visual.track_index],
                    };
                    let w = (display_len as f32 * px_per_frame).max(2.0);
                    let clip_rect = egui::Rect::from_min_size(
                        egui::pos2(x, y + 2.0),
                        egui::vec2(w, ROW_HEIGHT - 4.0),
                    );

                    let id = ui.id().with("clip").with(visual.clip.id.0);
                    let sense = if visual.locked {
                        egui::Sense::hover()
                    } else {
                        egui::Sense::click_and_drag()
                    };
                    let resp = ui.interact(
                        clip_rect.intersect(track_pane_rect(visual.track_index)),
                        id,
                        sense,
                    );

                    let is_selected = state.selected.contains(&(visual.track_index, visual.clip.id));
                    paint_clip_box(&painter, clip_rect, visual, is_selected);

                    // Filter dragged from the Effects panel: only video clips,
                    // not locked, accept it (no empty spaces or new
                    // tracks, unlike Generator/Media). The guard
                    // `has_payload_of_type` before `dnd_release_payload` is not
                    // redundant: the latter discards the global payload even
                    // when the type does not match (egui side effect, see
                    // `TimelineDrag::released`) — without it, dragging a
                    // `TransitionKind` onto the same clip would lose it here,
                    // before the block below even sees it.
                    if !visual.locked
                        && track_kinds[visual.track_index] == TrackKind::Video
                        && egui::DragAndDrop::has_payload_of_type::<vv_core::FilterKind>(ui.ctx())
                    {
                        if resp.dnd_hover_payload::<vv_core::FilterKind>().is_some() {
                            painter.rect_stroke(
                                clip_rect,
                                4.0,
                                egui::Stroke::new(3.0, FILTER_HIGHLIGHT_COLOR),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if let Some(filter) = resp.dnd_release_payload::<vv_core::FilterKind>() {
                            pending = Some(PendingAction::ApplyFilter {
                                track_index: visual.track_index,
                                clip_id: visual.clip.id,
                                filter: *filter,
                            });
                        }
                    }

                    // Transition dragged from the Effects panel: like the filters,
                    // only unlocked video clips — but in addition only near an
                    // edge (never at the center, never on an empty space or a new
                    // track): the side nearest to the pointer decides whether it becomes
                    // `transition_in` or `transition_out`. Same guard as
                    // above, same reason.
                    if !visual.locked
                        && track_kinds[visual.track_index] == TrackKind::Video
                        && egui::DragAndDrop::has_payload_of_type::<vv_core::TransitionKind>(ui.ctx())
                    {
                        let drop_zone_px = TRANSITION_DROP_ZONE_PX.min(clip_rect.width() / 2.0);
                        let hover_edge = ui.input(|i| i.pointer.hover_pos())
                            .and_then(|pos| transition_drop_edge(pos, clip_rect, drop_zone_px));
                        if resp.dnd_hover_payload::<vv_core::TransitionKind>().is_some()
                            && let Some(edge) = hover_edge
                        {
                            let x = match edge {
                                FadeEdge::In => clip_rect.left() + drop_zone_px,
                                FadeEdge::Out => clip_rect.right() - drop_zone_px,
                            };
                            let is_crossing = has_neighbor(&visuals, visual.track_index, visual.clip.id, edge);
                            paint_transition_marker(&painter, clip_rect, edge, x, false, is_crossing);
                            if is_crossing {
                                pending_crossing_previews.push((visual.track_index, visual.clip.id, edge));
                            }
                        }
                        if let Some(kind) = resp.dnd_release_payload::<vv_core::TransitionKind>()
                            && let Some(edge) = hover_edge
                        {
                            pending = Some(PendingAction::ApplyTransition {
                                track_index: visual.track_index,
                                clip_id: visual.clip.id,
                                edge,
                                kind: *kind,
                            });
                        }
                    }

                    // Alt+drag of an existing transition (duplication, see
                    // `begin_transition_duplicate_drag`): same payload as a
                    // drop from the Effects panel but with a whole `vv_core::Transition`
                    // instead of a `TransitionKind`, to keep its
                    // parameters (duration, direction, ease, curve) instead of
                    // starting from the defaults. Same guard, same reason.
                    if !visual.locked
                        && track_kinds[visual.track_index] == TrackKind::Video
                        && egui::DragAndDrop::has_payload_of_type::<vv_core::Transition>(ui.ctx())
                    {
                        let drop_zone_px = TRANSITION_DROP_ZONE_PX.min(clip_rect.width() / 2.0);
                        let hover_edge = ui.input(|i| i.pointer.hover_pos())
                            .and_then(|pos| transition_drop_edge(pos, clip_rect, drop_zone_px));
                        if resp.dnd_hover_payload::<vv_core::Transition>().is_some()
                            && let Some(edge) = hover_edge
                        {
                            let x = match edge {
                                FadeEdge::In => clip_rect.left() + drop_zone_px,
                                FadeEdge::Out => clip_rect.right() - drop_zone_px,
                            };
                            let is_crossing = has_neighbor(&visuals, visual.track_index, visual.clip.id, edge);
                            paint_transition_marker(&painter, clip_rect, edge, x, false, is_crossing);
                            if is_crossing {
                                pending_crossing_previews.push((visual.track_index, visual.clip.id, edge));
                            }
                        }
                        if let Some(transition) = resp.dnd_release_payload::<vv_core::Transition>()
                            && let Some(edge) = hover_edge
                        {
                            pending = Some(PendingAction::DuplicateTransition {
                                track_index: visual.track_index,
                                clip_id: visual.clip.id,
                                edge,
                                transition: (*transition).clone(),
                            });
                        }
                    }

                    // Waveform: maximum of the bins per column, shape independent of the zoom.
                    if track_kinds[visual.track_index] == TrackKind::Audio
                        && let ClipSource::Media(media_id) = &visual.clip.source
                        && let Some(item) = project.media_pool.get(*media_id)
                        && let Some(wf) = waveform_cache
                            .get(&(item.content_hash, visual.clip.audio_stream_index))
                    {
                        // During a trim the drawn clip covers another
                        // band of the source: without remapping it the
                        // waveform would stretch instead of being cut.
                        let (wave_start, wave_end) = if is_trimming_this {
                            (display_start, display_start + display_len)
                        } else {
                            (visual.clip.timeline_start, visual.clip.timeline_end())
                        };
                        let fps = timeline_fps.as_f64();
                        draw_clip_waveform(
                            &painter,
                            clip_rect,
                            &wf.peaks,
                            visual.clip.media_secs_at(wave_start, fps),
                            visual.clip.media_secs_at(wave_end, fps),
                            item.meta.fps.as_f64(),
                            wf.audio_duration_secs,
                            painter.clip_rect(),
                            &visual.clip.effects.gain_db,
                        );
                    }

                    let is_proxy_backed = proxy_ranges.iter().any(|&(s, e)| {
                        s < visual.clip.timeline_end() && e >= visual.clip.timeline_start
                    });
                    paint_clip_overlay(&painter, clip_rect, visual, is_proxy_backed);

                    // Volume line: a thin horizontal line draggable
                    // vertically, centered at 0 dB (see `gain_offset`). Only
                    // if the gain is not keyframed: a flat line would lie
                    // about the real curve, which is edited from the properties panel.
                    if track_kinds[visual.track_index] == TrackKind::Audio
                        && visual.clip.effects.gain_db.is_constant()
                    {
                        let dragging = state
                            .volume_drag
                            .as_ref()
                            .is_some_and(|d| d.clip_id == visual.clip.id);
                        paint_gain_line(
                            &painter,
                            clip_rect,
                            gain_line_y(visual.clip.effects.gain_db.default, clip_rect),
                            dragging,
                        );
                    }

                    // Fade-in/fade-out handles: always present if the
                    // fade is already set, otherwise only while the
                    // clip is under the mouse (to grab them from the corner).
                    let (fade_in_preview, fade_out_preview) = fade_preview(state, visual, px_per_frame);
                    let fade_in_dragging = state
                        .fade_drag
                        .as_ref()
                        .is_some_and(|d| d.clip_id == visual.clip.id && d.edge == FadeEdge::In);
                    let fade_out_dragging = state
                        .fade_drag
                        .as_ref()
                        .is_some_and(|d| d.clip_id == visual.clip.id && d.edge == FadeEdge::Out);
                    let show_fades = !visual.locked && clip_rect.width() >= MIN_FADE_CLIP_WIDTH_PX;
                    let fade_in_x = clip_rect.left()
                        + (fade_in_preview as f32 * px_per_frame).min(clip_rect.width());
                    let fade_out_x = clip_rect.right()
                        - (fade_out_preview as f32 * px_per_frame).min(clip_rect.width());
                    if show_fades && (visual.clip.fade_in > 0 || fade_in_dragging || resp.hovered()) {
                        paint_fade_wedge(&painter, clip_rect, clip_rect.left(), fade_in_x, fade_in_dragging);
                    }
                    if show_fades && (visual.clip.fade_out > 0 || fade_out_dragging || resp.hovered()) {
                        paint_fade_wedge(&painter, clip_rect, clip_rect.right(), fade_out_x, fade_out_dragging);
                    }
                    if let Some(pos) = ui.input(|i| i.pointer.latest_pos())
                        && (fade_in_dragging || fade_out_dragging)
                    {
                        let frames = if fade_in_dragging { fade_in_preview } else { fade_out_preview };
                        paint_duration_overlay(ui.ctx(), pos, frames, timeline_fps.as_f64());
                    }

                    // Markers of the already set transitions (single edge or
                    // crossing): the drop from the Effects panel (above) makes them
                    // persistent, from here on they live like the fade handle —
                    // always visible, the end (duration) draggable.
                    let track = &project.timelines[timeline_id].tracks[visual.track_index];
                    let left_marker = edge_marker(track, state, visual, px_per_frame, FadeEdge::In);
                    let right_marker = edge_marker(track, state, visual, px_per_frame, FadeEdge::Out);
                    let transition_in_x = left_marker.as_ref().map(|m| {
                        clip_rect.left() + (m.duration as f32 * px_per_frame).min(clip_rect.width())
                    });
                    let transition_out_x = right_marker.as_ref().map(|m| {
                        clip_rect.right() - (m.duration as f32 * px_per_frame).min(clip_rect.width())
                    });
                    if let (Some(m), Some(x)) = (&left_marker, transition_in_x) {
                        let selected = state.selected_transition == Some(m.selection);
                        paint_transition_marker(&painter, clip_rect, FadeEdge::In, x, selected, m.is_crossing);
                    }
                    if let (Some(m), Some(x)) = (&right_marker, transition_out_x) {
                        let selected = state.selected_transition == Some(m.selection);
                        paint_transition_marker(&painter, clip_rect, FadeEdge::Out, x, selected, m.is_crossing);
                    }
                    // Overlay with the duration during the drag of its
                    // end: general behavior (see
                    // `paint_duration_overlay`), not only for the fade above.
                    if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                        if let Some(d) = state.transition_drag.as_ref().filter(|d| d.clip_id == visual.clip.id) {
                            let frames = transition_drag_value(d, visual.clip.timeline_len, px_per_frame);
                            paint_duration_overlay(ui.ctx(), pos, frames, timeline_fps.as_f64());
                        } else if let Some(d) = state.crossing_drag.as_ref().filter(|d| {
                            [&left_marker, &right_marker].into_iter().flatten().any(|m| {
                                m.selection == TransitionSelection::Crossing(d.track_index, d.left_clip)
                            })
                        }) {
                            let frames = crossing_drag_value(d, px_per_frame);
                            paint_duration_overlay(ui.ctx(), pos, frames, timeline_fps.as_f64());
                        }
                    }

                    // Reduced zones for very narrow clips, otherwise
                    // the whole clip would be "only edges" and it could no longer
                    // be moved (Move) with a normal drag from the center.
                    let adjacent = |at: FrameIdx, edge: TrimEdge| {
                        visuals
                            .iter()
                            .find(|v| {
                                v.track_index == visual.track_index
                                    && v.clip.id != visual.clip.id
                                    && match edge {
                                        TrimEdge::Start => v.clip.timeline_end() == at,
                                        TrimEdge::End => v.clip.timeline_start == at,
                                    }
                            })
                            .map(|v| (v.track_index, v.clip.id))
                    };
                    let zones = edge_zones(
                        clip_rect.width(),
                        adjacent(visual.clip.timeline_start, TrimEdge::Start),
                        adjacent(visual.clip.timeline_end(), TrimEdge::End),
                    );
                    let edge_at = |pos: egui::Pos2| zones.at(pos.x - clip_rect.left());
                    let volume_hit = |pos: egui::Pos2| {
                        track_kinds[visual.track_index] == TrackKind::Audio
                            && visual.clip.effects.gain_db.is_constant()
                            && volume_line_hit(
                                pos,
                                clip_rect,
                                gain_line_y(visual.clip.effects.gain_db.default, clip_rect),
                            )
                    };
                    if resp.hovered()
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && state.transition_drag.is_none()
                        && state.crossing_drag.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && transition_handle_at(pos, clip_rect, transition_in_x, transition_out_x).is_some()
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    } else if resp.hovered()
                        && show_fades
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && state.fade_drag.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && fade_zone_at(pos, clip_rect, fade_in_x, fade_out_x).is_some()
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                    } else if resp.hovered()
                        && !visual.locked
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && let Some(zone) = edge_at(pos)
                    {
                        edge_cursor = Some((pos, EdgeCursor::from_zone(zone)));
                    } else if resp.hovered()
                        && !visual.locked
                        && state.drag.is_none()
                        && state.trim.is_none()
                        && state.volume_drag.is_none()
                        && let Some(pos) = resp.hover_pos()
                        && volume_hit(pos)
                    {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                    }

                    let marker_at = |edge: FadeEdge| match edge {
                        FadeEdge::In => left_marker.as_ref(),
                        FadeEdge::Out => right_marker.as_ref(),
                    };
                    if resp.drag_started() {
                        // `press_origin` and not the current position: egui declares the drag after a
                        // small movement, and towards the inside one would already have left the zone
                        // of the edge.
                        let press_pos = ui.input(|i| i.pointer.press_origin());
                        let transition_handle =
                            press_pos.and_then(|p| transition_handle_at(p, clip_rect, transition_in_x, transition_out_x));
                        // Alt+drag on the body (not on the handle) of an already
                        // present marker duplicates instead of resizing — the handle
                        // keeps priority, as the fade ignores Alt on its own.
                        let transition_duplicate_edge = if ui.input(|i| i.modifiers.alt) {
                            press_pos.and_then(|p| {
                                if transition_in_x.is_some_and(|x| transition_body_hit(p, clip_rect, FadeEdge::In, x)) {
                                    Some(FadeEdge::In)
                                } else if transition_out_x
                                    .is_some_and(|x| transition_body_hit(p, clip_rect, FadeEdge::Out, x))
                                {
                                    Some(FadeEdge::Out)
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        };
                        let fade_zone = if show_fades {
                            press_pos.and_then(|p| fade_zone_at(p, clip_rect, fade_in_x, fade_out_x))
                        } else {
                            None
                        };
                        match transition_handle.and_then(|edge| marker_at(edge).map(|m| (edge, m))) {
                            Some((edge, m)) => match m.selection {
                                TransitionSelection::Crossing(track_index, left_clip) => {
                                    begin_crossing_drag(state, project, timeline_id, track_index, left_clip, edge);
                                }
                                TransitionSelection::Edge(..) => begin_transition_drag(state, visual, edge),
                            },
                            None => match transition_duplicate_edge.and_then(marker_at) {
                            Some(m) => begin_transition_duplicate_drag(state, &resp, project, timeline_id, visual, m),
                            None => match fade_zone {
                            Some(edge) => begin_fade_drag(state, visual, edge),
                            None => match press_pos.and_then(edge_at) {
                                Some(zone) => begin_trim(state, &visuals, project, visual, zone),
                                None if press_pos.is_some_and(volume_hit) => {
                                    begin_volume_drag(state, history, visual)
                                }
                                None => {
                                    begin_drag(state, &visuals, visual, ui.input(|i| i.modifiers.alt))
                                }
                            },
                            },
                            },
                        }
                    } else if resp.dragged() {
                        if let Some(td) = &mut state.transition_drag
                            && td.clip_id == visual.clip.id
                        {
                            td.accum_px += resp.drag_delta().x;
                        } else if let Some(cd) = &mut state.crossing_drag
                            && [&left_marker, &right_marker].into_iter().flatten().any(|m| {
                                m.selection == TransitionSelection::Crossing(cd.track_index, cd.left_clip)
                            })
                        {
                            cd.accum_px += resp.drag_delta().x;
                        } else if let Some(fd) = &mut state.fade_drag
                            && fd.clip_id == visual.clip.id
                        {
                            fd.accum_px += resp.drag_delta().x;
                        } else if let Some(vd) = &mut state.volume_drag
                            && vd.clip_id == visual.clip.id
                        {
                            vd.accum_px += resp.drag_delta().y;
                            // Unlike fade/trim/move, it is applied right here,
                            // on every drag frame (see `VolumeDragState`).
                            pending = Some(PendingAction::SetGain {
                                track_index: vd.track_index,
                                clip_id: vd.clip_id,
                                new_value: volume_drag_value(vd, clip_rect.height() / 2.0),
                            });
                        } else if let Some(t) = &mut state.trim
                            && t.clip_id == visual.clip.id
                        {
                            t.accum_px += resp.drag_delta().x;
                        } else if let Some(d) = &mut state.drag
                            && d.clip_id == visual.clip.id
                        {
                            d.accum_px += resp.drag_delta().x;
                        }
                    } else if resp.drag_stopped() {
                        if let Some(td) = &state.transition_drag
                            && td.clip_id == visual.clip.id
                        {
                            pending = Some(PendingAction::SetTransitionDuration {
                                track_index: td.track_index,
                                clip_id: td.clip_id,
                                edge: td.edge,
                                new_value: transition_drag_value(td, visual.clip.timeline_len, px_per_frame),
                            });
                            transition_drag_finished = true;
                        } else if let Some(cd) = &state.crossing_drag
                            && [&left_marker, &right_marker].into_iter().flatten().any(|m| {
                                m.selection == TransitionSelection::Crossing(cd.track_index, cd.left_clip)
                            })
                        {
                            pending = Some(PendingAction::SetCrossingDuration {
                                track_index: cd.track_index,
                                left_clip: cd.left_clip,
                                new_value: crossing_drag_value(cd, px_per_frame),
                            });
                            crossing_drag_finished = true;
                        } else if state.transition_duplicate_drag == Some(visual.clip.id) {
                            // The real drop (if there was one, on another clip) is already
                            // handled by `dnd_release_payload` in that clip;
                            // here only closing the local state is left.
                            state.transition_duplicate_drag = None;
                        } else if let Some(fd) = &state.fade_drag
                            && fd.clip_id == visual.clip.id
                        {
                            pending = Some(PendingAction::SetFade {
                                track_index: fd.track_index,
                                clip_id: fd.clip_id,
                                edge: fd.edge,
                                new_value: fade_drag_value(fd, visual.clip.timeline_len, px_per_frame),
                            });
                            fade_drag_finished = true;
                        } else if let Some(vd) = &state.volume_drag
                            && vd.clip_id == visual.clip.id
                        {
                            pending = Some(PendingAction::SetGain {
                                track_index: vd.track_index,
                                clip_id: vd.clip_id,
                                new_value: volume_drag_value(vd, clip_rect.height() / 2.0),
                            });
                            volume_drag_group = Some(vd.group);
                            volume_drag_finished = true;
                        } else if let Some(t) = &state.trim
                            && t.clip_id == visual.clip.id
                        {
                            pending = Some(finish_trim(t, visual, &visuals, trimmed_primary_new_value));
                            trim_finished = true;
                        } else if let Some(d) = &state.drag
                            && d.clip_id == visual.clip.id
                        {
                            pending = Some(finish_drag(
                                d,
                                drag_group_targets.as_deref().unwrap(),
                                &track_kinds,
                                dragged_primary_new_start,
                            ));
                            drag_finished = true;
                        }
                    } else if resp.double_clicked() {
                        if let ClipSource::Media(media_id) = visual.clip.source
                            && let Some(nested_id) = project.media_pool.get(media_id).and_then(|m| m.compound)
                        {
                            enter_compound = Some(nested_id);
                        }
                    } else if resp.clicked() {
                        let clicked_transition = resp.interact_pointer_pos().and_then(|pos| {
                            if transition_in_x.is_some_and(|x| transition_body_hit(pos, clip_rect, FadeEdge::In, x)) {
                                left_marker.as_ref()
                            } else if transition_out_x
                                .is_some_and(|x| transition_body_hit(pos, clip_rect, FadeEdge::Out, x))
                            {
                                right_marker.as_ref()
                            } else {
                                None
                            }
                        });
                        if let Some(m) = clicked_transition {
                            // Replaces any clip selection, even a multiple one.
                            state.selected.clear();
                            state.selection_anchor = None;
                            state.selected_gap = None;
                            state.selected_transition = Some(m.selection);
                        } else {
                            let modifiers = click_modifiers(ui.input(|i| i.modifiers));
                            let (selected, anchor) = apply_click_selection(
                                &state.selected,
                                state.selection_anchor,
                                (visual.track_index, visual.clip.id),
                                modifiers,
                                &visuals,
                                px_per_frame,
                                &row_y,
                            );
                            state.selected = expand_to_linked_groups(&visuals, selected);
                            state.selection_anchor = anchor;
                            state.selected_gap = None;
                            state.selected_transition = None;
                        }
                    }

                    resp.context_menu(|ui| {
                        if visual.clip.linked_group.is_some() {
                            if ui.button(t!("timeline.unlink")).clicked() {
                                pending =
                                    Some(PendingAction::Unlink(visual.track_index, visual.clip.id));
                                ui.close();
                            }
                        } else if state.selected.len() >= 2 {
                            if ui.button(t!("timeline.link")).clicked() {
                                pending =
                                    Some(PendingAction::Link(state.selected.iter().copied().collect()));
                                ui.close();
                            }
                        } else {
                            ui.label(t!("timeline.link_hint"));
                        }
                        ui.separator();
                        if ui.button(t!("timeline.make_compound_clip")).clicked() {
                            // A right-click on an unselected clip acts only
                            // on it, not on the stale previous selection.
                            let base = if state.selected.is_empty() {
                                BTreeSet::from([(visual.track_index, visual.clip.id)])
                            } else {
                                state.selected.clone()
                            };
                            let selection = expand_to_linked_groups(&visuals, base);
                            pending = Some(PendingAction::MakeCompound(selection.into_iter().collect()));
                            ui.close();
                        }
                    });
                }
                // See the docs of `pending_crossing_previews`: only now, with the
                // drawing of all the clips finished, can no later `paint_clip_box`
                // cover it any more.
                for (track_index, clip_id, edge) in pending_crossing_previews {
                    paint_mirrored_marker_on_neighbor(
                        &track_painter(track_index),
                        &visuals,
                        origin,
                        &row_y,
                        px_per_frame,
                        track_index,
                        clip_id,
                        edge,
                    );
                }
                // Cleared only now, not with a `.take()` halfway through the loop
                // above — see the comment on `drag_finished`/`trim_finished`.
                if drag_finished {
                    state.drag = None;
                }
                if trim_finished {
                    state.trim = None;
                }
                if fade_drag_finished {
                    state.fade_drag = None;
                }
                if transition_drag_finished {
                    state.transition_drag = None;
                }
                if crossing_drag_finished {
                    state.crossing_drag = None;
                }
                if volume_drag_finished {
                    state.volume_drag = None;
                }
                if let Some(t) = &state.trim
                    && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
                {
                    let cursor = match (t.roll, t.edge) {
                        (true, _) => EdgeCursor::Roll,
                        (false, TrimEdge::Start) => EdgeCursor::TrimStart,
                        (false, TrimEdge::End) => EdgeCursor::TrimEnd,
                    };
                    edge_cursor = Some((pos, cursor));
                }
                if let Some((pos, cursor)) = edge_cursor {
                    paint_edge_cursor(ui.ctx(), pos, cursor);
                }

                paint_playhead(&painter, origin, state.playhead as f32 * px_per_frame, visual_height);

                // On the visible right edge, not on the content's one.
                let scrollbar_x = egui::Rangef::new(
                    ui.clip_rect().right() - PANE_SCROLLBAR_WIDTH - 2.0,
                    ui.clip_rect().right() - 2.0,
                );
                let scrollbar_rect = |pane: egui::Rangef| {
                    egui::Rect::from_x_y_ranges(
                        scrollbar_x,
                        (origin.y + pane.min + 2.0)..=(origin.y + pane.max - 2.0),
                    )
                };
                let video_max = layout.video_max_scroll;
                if let Some(offset) = pane_scrollbar(
                    ui,
                    &painter,
                    scrollbar_rect(layout.video_pane),
                    ui.id().with("timeline_video_vscroll"),
                    video_max - state.video_scroll,
                    video_max,
                ) {
                    state.video_scroll = video_max - offset;
                }
                if let Some(offset) = pane_scrollbar(
                    ui,
                    &painter,
                    scrollbar_rect(layout.audio_pane),
                    ui.id().with("timeline_audio_vscroll"),
                    state.audio_scroll,
                    layout.audio_max_scroll,
                ) {
                    state.audio_scroll = offset;
                }
            });

        });

    if let Some(action) = pending {
        apply_pending_action(project, history, state, timeline_id, action);
    }
    if let Some(mark) = volume_drag_group {
        history.end_group(mark);
    }

    (media_drop, enter_compound)
}

/// Start and length to draw `visual` with: during a trim or a drag
/// the preview position, otherwise the real one.
fn display_range(
    visual: &ClipVisual,
    state: &TimelineState,
    is_trimming_this: bool,
    trimmed_primary_new_value: Option<FrameIdx>,
    dragged_primary_new_start: Option<FrameIdx>,
) -> (FrameIdx, FrameIdx) {
    if is_trimming_this
        && let (Some(t), Some(primary_value)) = (&state.trim, trimmed_primary_new_value)
    {
        let (offset, edge) = t
            .followers
            .iter()
            .find(|&&(id, track, _, _)| id == visual.clip.id && track == visual.track_index)
            .map_or((0, t.edge), |&(_, _, offset, edge)| (offset, edge));
        let new_value = primary_value + offset;
        match edge {
            TrimEdge::Start => (new_value, (visual.clip.timeline_end() - new_value).max(1)),
            TrimEdge::End => (
                visual.clip.timeline_start,
                (new_value - visual.clip.timeline_start).max(1),
            ),
        }
    } else {
        let start = match (&state.drag, dragged_primary_new_start) {
            (Some(d), Some(new_start)) if d.clip_id == visual.clip.id => new_start,
            (Some(d), Some(new_start)) => match d
                .followers
                .iter()
                .find(|(id, track, _)| *id == visual.clip.id && *track == visual.track_index)
            {
                Some((_, _, offset)) => new_start + offset,
                None => visual.clip.timeline_start,
            },
            _ => visual.clip.timeline_start,
        };
        (start, visual.clip.timeline_len)
    }
}

/// Fill and border of a clip.
fn paint_clip_box(painter: &egui::Painter, clip_rect: egui::Rect, visual: &ClipVisual, is_selected: bool) {
    let stroke = if is_selected {
        egui::Stroke::new(2.0, egui::Color32::WHITE)
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_gray(15))
    };
    let fill = if visual.muted {
        egui::Color32::from_gray(58)
    } else {
        visual.color
    };
    painter.rect_filled(clip_rect, 4.0, fill);
    painter.rect_stroke(clip_rect, 4.0, stroke, egui::StrokeKind::Inside);
}

/// Label, "disabled" badge, link icon and veil of the locked
/// tracks, on top of the waveform.
fn paint_clip_overlay(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    visual: &ClipVisual,
    is_proxy_backed: bool,
) {
    let label_offset_y = if is_proxy_backed {
        paint_proxy_strip(painter, clip_rect);
        2.0 + PROXY_STRIP_HEIGHT
    } else {
        2.0
    };
    let mut label_pos = clip_rect.left_top() + egui::vec2(4.0, label_offset_y);
    if visual.clip.disabled {
        paint_disabled_badge(painter, label_pos);
        label_pos.x += DISABLED_BADGE_SIZE + 4.0;
    }
    painter.text(
        label_pos,
        egui::Align2::LEFT_TOP,
        &visual.label,
        egui::FontId::proportional(12.0),
        if visual.muted {
            egui::Color32::from_gray(185)
        } else {
            egui::Color32::BLACK
        },
    );
    if visual.clip.linked_group.is_some() {
        // Two hand-drawn rings: on some platforms (Asahi) egui's fonts do not
        // have 🔗.
        let center = clip_rect.right_top() + egui::vec2(-9.0, 8.0);
        let ring_color = if visual.muted {
            egui::Color32::from_gray(185)
        } else {
            egui::Color32::BLACK
        };
        let ring_stroke = egui::Stroke::new(1.3, ring_color);
        painter.circle_stroke(center + egui::vec2(-2.5, 0.0), 3.5, ring_stroke);
        painter.circle_stroke(center + egui::vec2(2.5, 0.0), 3.5, ring_stroke);
    }
    if visual.locked {
        painter.rect_filled(
            clip_rect,
            4.0,
            egui::Color32::from_rgba_unmultiplied(70, 70, 70, 140),
        );
    }
}

/// Triangular shadow of the fade (from the corner towards the handle) plus the
/// dot of the handle itself. `x` is the current position of the handle,
/// `corner_x` the corner (left for the fade-in, right for the fade-out) the
/// triangle starts from.
fn paint_fade_wedge(painter: &egui::Painter, clip_rect: egui::Rect, corner_x: f32, x: f32, dragging: bool) {
    let top = clip_rect.top();
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(corner_x, top),
            egui::pos2(x, top),
            egui::pos2(corner_x, clip_rect.bottom()),
        ],
        egui::Color32::from_black_alpha(110),
        egui::Stroke::NONE,
    ));
    let center = egui::pos2(x, top + FADE_HANDLE_ZONE_HEIGHT * 0.5);
    let radius = if dragging { FADE_HANDLE_RADIUS + 1.0 } else { FADE_HANDLE_RADIUS };
    painter.circle_filled(center, radius, egui::Color32::WHITE);
    painter.circle_stroke(center, radius, egui::Stroke::new(1.0, egui::Color32::from_gray(40)));
}

/// Overlay with the duration (fade, single transition or crossing)
/// near the pointer, during the drag of its end: same "always on top"
/// layer as the trim cursor. General behavior, not only
/// for the fades: any draggable end shows it.
fn paint_duration_overlay(ctx: &egui::Context, pos: egui::Pos2, frames: FrameIdx, fps: f64) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("timeline_duration_overlay"),
    ));
    let text = format!("+{}", format_duration(frames, fps));
    let text_pos = pos + egui::vec2(12.0, 14.0);
    let galley = painter.layout_no_wrap(text, egui::FontId::proportional(12.0), egui::Color32::WHITE);
    let bg = egui::Rect::from_min_size(text_pos, galley.size()).expand(3.0);
    painter.rect_filled(bg, 3.0, egui::Color32::from_black_alpha(200));
    painter.galley(text_pos, galley, egui::Color32::WHITE);
}

/// `S:FF`: duration in seconds and remaining frames, not a timeline
/// position (no hours/minutes, these durations are always short).
fn format_duration(frames: FrameIdx, fps: f64) -> String {
    let nominal = (fps.round() as i64).max(1);
    let frames = frames.max(0);
    let (secs, f) = (frames / nominal, frames % nominal);
    format!("{secs}:{f:02}")
}

/// Starts the trim of `visual` from the edge zone `zone`, with the clips
/// following it.
fn begin_trim(
    state: &mut TimelineState,
    visuals: &[ClipVisual],
    project: &Project,
    visual: &ClipVisual,
    zone: EdgeZone,
) {
    let edge = zone.edge();
    let key = (visual.track_index, visual.clip.id);
    let others: Vec<(ClipKey, TrimEdge)> = match zone {
        EdgeZone::Trim(_) => drag_group_for(&state.selected, visuals, key)
            .into_iter()
            .filter(|k| *k != key)
            .map(|k| (k, edge))
            .collect(),
        // Only the two clips in contact, each with its own linked group.
        EdgeZone::Roll { neighbor, .. } => {
            let opposite = match edge {
                TrimEdge::Start => TrimEdge::End,
                TrimEdge::End => TrimEdge::Start,
            };
            expand_to_linked_groups(visuals, [key])
                .into_iter()
                .filter(|k| *k != key)
                .map(|k| (k, edge))
                .chain(
                    expand_to_linked_groups(visuals, [neighbor])
                        .into_iter()
                        .map(|k| (k, opposite)),
                )
                .collect()
        }
    };
    let (min_value, max_value, followers) =
        combined_trim_range(visuals, project, key, edge, &others);
    let original_value = match edge {
        TrimEdge::Start => visual.clip.timeline_start,
        TrimEdge::End => visual.clip.timeline_end(),
    };
    state.trim = Some(TrimState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        edge,
        original_value,
        accum_px: 0.0,
        min_value,
        max_value: max_value.max(min_value),
        followers,
        roll: matches!(zone, EdgeZone::Roll { .. }),
    });
}

/// Starts dragging `visual` together with its selection group.
fn begin_drag(state: &mut TimelineState, visuals: &[ClipVisual], visual: &ClipVisual, duplicate: bool) {
    let drag_group = drag_group_for(&state.selected, visuals, (visual.track_index, visual.clip.id));
    state.selected = drag_group.clone();
    state.selection_anchor = Some((visual.track_index, visual.clip.id));

    let others: Vec<ClipKey> = drag_group.into_iter().collect();
    let (_, _, followers) =
        combined_drag_range(visuals, visual.track_index, visual.clip.id, &others);

    state.drag = Some(DragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        original_start: visual.clip.timeline_start,
        accum_px: 0.0,
        followers,
        duplicate,
    });
}

/// Starts dragging the fade-in/fade-out handle of `visual`: no
/// group nor neighbor involved, it is always local to the clip.
fn begin_fade_drag(state: &mut TimelineState, visual: &ClipVisual, edge: FadeEdge) {
    let original_value = match edge {
        FadeEdge::In => visual.clip.fade_in,
        FadeEdge::Out => visual.clip.fade_out,
    };
    state.fade_drag = Some(FadeDragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        edge,
        original_value,
        accum_px: 0.0,
    });
}

/// Value (in frames, clamped to the clip duration) of the preview of a
/// fade drag in progress: dragging the fade-in handle to the right lengthens
/// `fade_in`, dragging the fade-out one to the left lengthens
/// `fade_out` — opposite directions on the X axis for the same sign of `accum_px`.
fn fade_drag_value(d: &FadeDragState, clip_len: FrameIdx, px_per_frame: f32) -> FrameIdx {
    let signed_delta = match d.edge {
        FadeEdge::In => d.accum_px,
        FadeEdge::Out => -d.accum_px,
    };
    (d.original_value as f32 + signed_delta / px_per_frame)
        .round()
        .clamp(0.0, clip_len as f32) as FrameIdx
}

/// `(fade_in, fade_out)` to show for `visual`: the preview of the drag in
/// progress if it concerns it, otherwise the already saved values.
fn fade_preview(state: &TimelineState, visual: &ClipVisual, px_per_frame: f32) -> (FrameIdx, FrameIdx) {
    let mut fade_in = visual.clip.fade_in;
    let mut fade_out = visual.clip.fade_out;
    if let Some(d) = &state.fade_drag
        && d.clip_id == visual.clip.id
    {
        let value = fade_drag_value(d, visual.clip.timeline_len, px_per_frame);
        match d.edge {
            FadeEdge::In => fade_in = value,
            FadeEdge::Out => fade_out = value,
        }
    }
    (fade_in, fade_out)
}

/// Fade handle under `pos`, only in the band at the top of the clip: below
/// stays the trim/roll of the existing edge.
fn fade_zone_at(pos: egui::Pos2, clip_rect: egui::Rect, fade_in_x: f32, fade_out_x: f32) -> Option<FadeEdge> {
    if pos.y > clip_rect.top() + FADE_HANDLE_ZONE_HEIGHT {
        return None;
    }
    if (pos.x - fade_in_x).abs() <= FADE_HANDLE_HIT_RADIUS {
        return Some(FadeEdge::In);
    }
    if (pos.x - fade_out_x).abs() <= FADE_HANDLE_HIT_RADIUS {
        return Some(FadeEdge::Out);
    }
    None
}

/// Starts dragging the end (duration) of an already present
/// transition: like `begin_fade_drag`, always local to the single clip.
fn begin_transition_drag(state: &mut TimelineState, visual: &ClipVisual, edge: FadeEdge) {
    let original_value = match edge {
        FadeEdge::In => visual.clip.effects.transition_in.as_ref(),
        FadeEdge::Out => visual.clip.effects.transition_out.as_ref(),
    }
    .map_or(0, |t| t.duration);
    state.transition_drag = Some(TransitionDragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        edge,
        original_value,
        accum_px: 0.0,
    });
}

/// The clip adjacent to `clip_id` on the side `edge`, on the same track: `In`
/// looks for whoever touches its start, `Out` for whoever touches its end. `None` if
/// `clip_id` does not exist or has no neighbors on that side.
fn adjacent_clip(track: &vv_core::Track, clip_id: ClipId, edge: FadeEdge) -> Option<ClipId> {
    let clip = track.clip(clip_id)?;
    let at = match edge {
        FadeEdge::In => clip.timeline_start,
        FadeEdge::Out => clip.timeline_end(),
    };
    track
        .clips
        .iter()
        .find(|c| {
            c.id != clip_id
                && match edge {
                    FadeEdge::In => c.timeline_end() == at,
                    FadeEdge::Out => c.timeline_start == at,
                }
        })
        .map(|c| c.id)
}

/// Creates (or replaces) the crossing transition between `left_id` and `right_id`,
/// clamping `transition.duration` to what the two clips can really
/// "lend" it (twice the shorter of the two, see
/// `CrossTransition::split`), and selects it.
fn apply_new_crossing(
    project: &mut Project,
    history: &mut History,
    state: &mut TimelineState,
    timeline_id: TimelineId,
    track_index: usize,
    left_id: ClipId,
    right_id: ClipId,
    mut transition: vv_core::Transition,
) {
    let track = &project.timelines[timeline_id].tracks[track_index];
    let (Some(left), Some(right)) = (track.clip(left_id), track.clip(right_id)) else {
        return;
    };
    let max_duration = (2 * left.timeline_len.min(right.timeline_len)).max(1);
    transition.duration = transition.duration.clamp(1, max_duration);
    let crossing = vv_core::CrossTransition { left_clip: left_id, right_clip: right_id, transition };
    history.do_command(
        project,
        Box::new(vv_core::SetCrossTransition::new(timeline_id, track_index, left_id, Some(crossing))),
    );
    state.selected.clear();
    state.selection_anchor = None;
    state.selected_gap = None;
    state.selected_transition = Some(TransitionSelection::Crossing(track_index, left_id));
}

/// Starts a symmetric drag of the duration of a crossing transition:
/// `edge` is the edge of *this* clip the drag starts from — `Out` means
/// that this clip is the `left_clip` of the crossing (the end grabbed
/// is the one inside it), `In` that it is the `right_clip` — consistent with
/// `Track::crossing_from`/`crossing_into` used by `edge_marker`.
fn begin_crossing_drag(
    state: &mut TimelineState,
    project: &Project,
    timeline_id: TimelineId,
    track_index: usize,
    left_clip: ClipId,
    edge: FadeEdge,
) {
    let track = &project.timelines[timeline_id].tracks[track_index];
    let Some(crossing) = track.crossing_from(left_clip) else {
        return;
    };
    let (Some(left), Some(right)) = (track.clip(crossing.left_clip), track.clip(crossing.right_clip)) else {
        return;
    };
    let max_duration = (2 * left.timeline_len.min(right.timeline_len)).max(1);
    state.crossing_drag = Some(CrossingDragState {
        track_index,
        left_clip,
        grabbed_left_side: edge == FadeEdge::Out,
        original_duration: crossing.transition.duration,
        max_duration,
        accum_px: 0.0,
    });
}

/// Starts an Alt+drag duplication from the body of a transition
/// marker (single edge or crossing): the DnD payload must be set
/// right here, not in `resp.dragged()` as one might think by
/// analogy with the rest of the file — `Response::dnd_set_drag_payload` acts
/// only if `drag_started()`, not on every drag frame (the library then
/// keeps it alive by itself for the duration of the drag, see `egui::DragAndDrop`).
fn begin_transition_duplicate_drag(
    state: &mut TimelineState,
    resp: &egui::Response,
    project: &Project,
    timeline_id: TimelineId,
    visual: &ClipVisual,
    marker: &EdgeMarker,
) {
    let transition = match marker.selection {
        TransitionSelection::Edge(_, edge) => match edge {
            FadeEdge::In => visual.clip.effects.transition_in.clone(),
            FadeEdge::Out => visual.clip.effects.transition_out.clone(),
        },
        TransitionSelection::Crossing(track_index, left_clip) => project.timelines[timeline_id]
            .tracks[track_index]
            .crossing_from(left_clip)
            .map(|c| c.transition.clone()),
    };
    if let Some(transition) = transition {
        resp.dnd_set_drag_payload(transition);
    }
    state.transition_duplicate_drag = Some(visual.clip.id);
}

/// Value (in frames, clamped to 1..=clip duration) of the preview of a
/// transition drag in progress: same sign convention as
/// `fade_drag_value` (dragging the end towards the inside of the clip
/// lengthens the transition, on both edges).
fn transition_drag_value(d: &TransitionDragState, clip_len: FrameIdx, px_per_frame: f32) -> FrameIdx {
    let signed_delta = match d.edge {
        FadeEdge::In => d.accum_px,
        FadeEdge::Out => -d.accum_px,
    };
    (d.original_value as f32 + signed_delta / px_per_frame)
        .round()
        .clamp(1.0, clip_len.max(1) as f32) as FrameIdx
}

/// Total duration (in frames) of the preview of a crossing drag in progress:
/// symmetric, dragging the left end to the left lengthens it
/// (and the right one to the right in the same way), always by twice
/// the displacement in frames — it grows/shrinks on the two sides equally.
fn crossing_drag_value(d: &CrossingDragState, px_per_frame: f32) -> FrameIdx {
    // Like `transition_drag_value`: the left end is an "Out" edge
    // (inside the left clip, it grows by dragging it to the left,
    // away from the cut), the right one an "In" edge (inside the right
    // clip, it grows by dragging it to the right) — here doubled in addition
    // on the other side, see above.
    let signed_delta = if d.grabbed_left_side { -d.accum_px } else { d.accum_px };
    (d.original_duration as f32 + 2.0 * signed_delta / px_per_frame)
        .round()
        .clamp(1.0, d.max_duration.max(1) as f32) as FrameIdx
}

/// What to show/select on the edge `edge` of `visual.clip`: a single
/// edge (`EffectStack::transition_in`/`_out`), or — if that edge is
/// shared with a valid crossing transition — its own half of
/// that. The duration reflects the preview of a resize drag
/// in progress on this edge, if there is one.
struct EdgeMarker {
    duration: FrameIdx,
    selection: TransitionSelection,
    is_crossing: bool,
}

fn edge_marker(
    track: &vv_core::Track,
    state: &TimelineState,
    visual: &ClipVisual,
    px_per_frame: f32,
    edge: FadeEdge,
) -> Option<EdgeMarker> {
    let crossing = match edge {
        FadeEdge::In => track.crossing_into(visual.clip.id),
        FadeEdge::Out => track.crossing_from(visual.clip.id),
    };
    if let Some(crossing) = crossing {
        let total = if let Some(d) = &state.crossing_drag
            && d.track_index == visual.track_index
            && d.left_clip == crossing.left_clip
        {
            crossing_drag_value(d, px_per_frame)
        } else {
            crossing.transition.duration
        };
        let (split_left, split_right) = vv_core::CrossTransition::split_duration(total);
        let duration = match edge {
            FadeEdge::Out => split_left,
            FadeEdge::In => split_right,
        };
        return Some(EdgeMarker {
            duration,
            selection: TransitionSelection::Crossing(visual.track_index, crossing.left_clip),
            is_crossing: true,
        });
    }
    let transition = match edge {
        FadeEdge::In => visual.clip.effects.transition_in.as_ref(),
        FadeEdge::Out => visual.clip.effects.transition_out.as_ref(),
    }?;
    let mut duration = transition.duration;
    if let Some(d) = &state.transition_drag
        && d.clip_id == visual.clip.id
        && d.edge == edge
    {
        duration = transition_drag_value(d, visual.clip.timeline_len, px_per_frame);
    }
    Some(EdgeMarker {
        duration,
        selection: TransitionSelection::Edge((visual.track_index, visual.clip.id), edge),
        is_crossing: false,
    })
}

/// End (duration) of a transition under `pos`, only in the band at the
/// bottom of the clip — mirroring `fade_zone_at`. `None` for an edge that
/// has no transition yet: nothing to drag there.
fn transition_handle_at(
    pos: egui::Pos2,
    clip_rect: egui::Rect,
    in_x: Option<f32>,
    out_x: Option<f32>,
) -> Option<FadeEdge> {
    if pos.y < clip_rect.bottom() - TRANSITION_HANDLE_ZONE_HEIGHT {
        return None;
    }
    if let Some(x) = in_x
        && (pos.x - x).abs() <= TRANSITION_HANDLE_HIT_RADIUS
    {
        return Some(FadeEdge::In);
    }
    if let Some(x) = out_x
        && (pos.x - x).abs() <= TRANSITION_HANDLE_HIT_RADIUS
    {
        return Some(FadeEdge::Out);
    }
    None
}

/// `true` if `pos` falls in the body of a transition marker (from the edge
/// of the clip to its end `x`), not only on its handle: a
/// click anywhere there selects it, no need to aim at the end.
fn transition_body_hit(pos: egui::Pos2, clip_rect: egui::Rect, edge: FadeEdge, x: f32) -> bool {
    if pos.y < clip_rect.bottom() - TRANSITION_HANDLE_ZONE_HEIGHT {
        return false;
    }
    match edge {
        FadeEdge::In => pos.x >= clip_rect.left() && pos.x <= x,
        FadeEdge::Out => pos.x <= clip_rect.right() && pos.x >= x,
    }
}

/// Edge of `clip_rect` nearest to `pos`, within `drop_zone_px` — shared
/// by the drop of a `TransitionKind` from the Effects panel and by the drop of a
/// whole `Transition` (duplication via Alt+drag): the same "only
/// near an edge" rule in both cases.
fn transition_drop_edge(pos: egui::Pos2, clip_rect: egui::Rect, drop_zone_px: f32) -> Option<FadeEdge> {
    if pos.x - clip_rect.left() <= drop_zone_px {
        Some(FadeEdge::In)
    } else if clip_rect.right() - pos.x <= drop_zone_px {
        Some(FadeEdge::Out)
    } else {
        None
    }
}

/// The `ClipVisual` adjacent to `clip_id` on the side `edge`, on the same
/// track — `None` if there is none. Used during the drag of a
/// transition to anticipate, through the marker color, whether releasing there
/// will create a crossing or stay a single edge (see `CROSSING_COLOR`),
/// and to draw the mirror marker on the neighboring clip too (see
/// `paint_mirrored_marker_on_neighbor`).
fn neighbor_visual<'a, 'b>(
    visuals: &'a [ClipVisual<'b>],
    track_index: usize,
    clip_id: ClipId,
    edge: FadeEdge,
) -> Option<&'a ClipVisual<'b>> {
    let visual = visuals.iter().find(|v| v.track_index == track_index && v.clip.id == clip_id)?;
    let at = match edge {
        FadeEdge::In => visual.clip.timeline_start,
        FadeEdge::Out => visual.clip.timeline_end(),
    };
    visuals.iter().find(|v| {
        v.track_index == track_index
            && v.clip.id != clip_id
            && match edge {
                FadeEdge::In => v.clip.timeline_end() == at,
                FadeEdge::Out => v.clip.timeline_start == at,
            }
    })
}

fn has_neighbor(visuals: &[ClipVisual], track_index: usize, clip_id: ClipId, edge: FadeEdge) -> bool {
    neighbor_visual(visuals, track_index, clip_id, edge).is_some()
}

/// If there is an adjacent clip on the side `edge`, it draws the mirror
/// marker on it too (opposite edge): once released, a crossing already shows
/// like that on both clips (see the docs of `paint_transition_marker`) —
/// showing it only on the clip under the pointer during the drag would be
/// misleading. It must be called AFTER the loop drawing all the clips (see
/// `pending_crossing_previews`): the neighbor may come later in `draw_order`,
/// and its own `paint_clip_box` would cover it if drawn during
/// the iteration of the clip under the pointer. The neighboring clip is never in
/// a drag/trim when this function is called (one drag at a
/// time, and this is the drag of a `TransitionKind`/`Transition` from the
/// Effects panel), so its static rectangle is enough.
fn paint_mirrored_marker_on_neighbor(
    painter: &egui::Painter,
    visuals: &[ClipVisual],
    origin: egui::Pos2,
    row_y: &[f32],
    px_per_frame: f32,
    track_index: usize,
    clip_id: ClipId,
    edge: FadeEdge,
) {
    let Some(neighbor) = neighbor_visual(visuals, track_index, clip_id, edge) else {
        return;
    };
    let x = origin.x + neighbor.clip.timeline_start as f32 * px_per_frame;
    let y = origin.y + row_y[track_index];
    let w = (neighbor.clip.timeline_len as f32 * px_per_frame).max(2.0);
    let neighbor_rect = egui::Rect::from_min_size(egui::pos2(x, y + 2.0), egui::vec2(w, ROW_HEIGHT - 4.0));
    let drop_zone_px = TRANSITION_DROP_ZONE_PX.min(neighbor_rect.width() / 2.0);
    let opposite = match edge {
        FadeEdge::In => FadeEdge::Out,
        FadeEdge::Out => FadeEdge::In,
    };
    let nx = match opposite {
        FadeEdge::In => neighbor_rect.left() + drop_zone_px,
        FadeEdge::Out => neighbor_rect.right() - drop_zone_px,
    };
    paint_transition_marker(painter, neighbor_rect, opposite, nx, false, true);
}

/// The marker of a transition: a colored band at the bottom of the clip from the
/// edge to `x`. A single edge (`is_crossing: false`) shows a
/// gear on the fixed side (the real edge of the clip, against
/// transparency) and a bracket on the end `x` (the draggable side) —
/// "X]" for `In`, "[X" for `Out`. A crossing (`is_crossing: true`) has no
/// "fixed" side: the edge shared with the neighbor shows another
/// bracket, opening towards the inside of its own half — the two halves,
/// drawn one per clip, sit side by side there as "][".
fn paint_transition_marker(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    edge: FadeEdge,
    x: f32,
    selected: bool,
    is_crossing: bool,
) {
    let x = x.clamp(clip_rect.left(), clip_rect.right());
    let band = egui::Rect::from_min_max(
        egui::pos2(clip_rect.left(), clip_rect.bottom() - TRANSITION_HANDLE_ZONE_HEIGHT),
        clip_rect.max,
    );
    let body = match edge {
        FadeEdge::In => egui::Rect::from_min_max(band.min, egui::pos2(x, band.bottom())),
        FadeEdge::Out => egui::Rect::from_min_max(egui::pos2(x, band.top()), band.max),
    };
    let color = match (is_crossing, selected) {
        (false, false) => TRANSITION_COLOR,
        (false, true) => TRANSITION_SELECTED_COLOR,
        (true, false) => CROSSING_COLOR,
        (true, true) => CROSSING_SELECTED_COLOR,
    };
    painter.rect_filled(body, 0.0, color);
    let edge_x = match edge {
        FadeEdge::In => clip_rect.left(),
        FadeEdge::Out => clip_rect.right(),
    };
    if is_crossing {
        let opposite = match edge {
            FadeEdge::In => FadeEdge::Out,
            FadeEdge::Out => FadeEdge::In,
        };
        paint_bracket_icon(painter, egui::pos2(edge_x, band.center().y), band.height() * 0.7, opposite, egui::Color32::WHITE);
    } else {
        paint_gear_icon(painter, egui::pos2(edge_x, band.center().y), band.height() * 0.4, egui::Color32::WHITE);
    }
    paint_bracket_icon(painter, egui::pos2(x, band.center().y), band.height() * 0.7, edge, egui::Color32::WHITE);
}

/// Hand-drawn bracket (no Unicode glyph, see `paint_gear_icon`):
/// a vertical line with two notches opening towards the fixed edge of the
/// clip (`In`: notches on the left, towards the gear; `Out`: on the right).
pub(crate) fn paint_bracket_icon(painter: &egui::Painter, center: egui::Pos2, height: f32, edge: FadeEdge, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.6, color);
    let half = height / 2.0;
    let tick = height * 0.3;
    let tick_dir = match edge {
        FadeEdge::In => -1.0,
        FadeEdge::Out => 1.0,
    };
    painter.line_segment(
        [egui::pos2(center.x, center.y - half), egui::pos2(center.x, center.y + half)],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y - half),
            egui::pos2(center.x + tick * tick_dir, center.y - half),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(center.x, center.y + half),
            egui::pos2(center.x + tick * tick_dir, center.y + half),
        ],
        stroke,
    );
}

/// Transitions of the Effects panel, in the order they appear there — see
/// `ALL_FILTER_KINDS`, same idea.
pub const ALL_TRANSITION_KINDS: [vv_core::TransitionKind; 1] = [vv_core::TransitionKind::Push];

pub fn transition_kind_label(kind: vv_core::TransitionKind) -> std::borrow::Cow<'static, str> {
    match kind {
        vv_core::TransitionKind::Push => t!("transition.push"),
    }
}

pub fn push_direction_label(direction: vv_core::PushDirection) -> std::borrow::Cow<'static, str> {
    match direction {
        vv_core::PushDirection::Left => t!("transition.direction_left"),
        vv_core::PushDirection::Right => t!("transition.direction_right"),
        vv_core::PushDirection::Up => t!("transition.direction_up"),
        vv_core::PushDirection::Down => t!("transition.direction_down"),
    }
}

pub fn ease_label(ease: vv_core::Ease) -> std::borrow::Cow<'static, str> {
    match ease {
        vv_core::Ease::None => t!("transition.ease_none"),
        vv_core::Ease::In => t!("transition.ease_in"),
        vv_core::Ease::Out => t!("transition.ease_out"),
        vv_core::Ease::InOut => t!("transition.ease_in_out"),
    }
}

/// Normalized vertical offset (-1 at the bottom, +1 at the top) of the volume
/// line for a gain in dB: centered at 0 dB, the two branches use different scales
/// because `GAIN_DB_MIN`/`GAIN_DB_MAX` are not symmetric.
fn gain_offset(db: f32) -> f32 {
    if db >= 0.0 {
        (db / vv_core::GAIN_DB_MAX).clamp(0.0, 1.0)
    } else {
        -(db / vv_core::GAIN_DB_MIN).clamp(0.0, 1.0)
    }
}

/// Inverse of `gain_offset`.
fn gain_from_offset(offset: f32) -> f32 {
    let offset = offset.clamp(-1.0, 1.0);
    if offset >= 0.0 {
        offset * vv_core::GAIN_DB_MAX
    } else {
        -offset * vv_core::GAIN_DB_MIN
    }
}

/// Y coordinate of the volume line for a gain in dB.
fn gain_line_y(db: f32, clip_rect: egui::Rect) -> f32 {
    clip_rect.center().y - gain_offset(db) * clip_rect.height() / 2.0
}

fn paint_gain_line(painter: &egui::Painter, clip_rect: egui::Rect, line_y: f32, dragging: bool) {
    let alpha = if dragging { 220 } else { 130 };
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha));
    painter.line_segment(
        [egui::pos2(clip_rect.left(), line_y), egui::pos2(clip_rect.right(), line_y)],
        stroke,
    );
}

/// The volume line is as wide as the clip but thin: it is grabbed within
/// `VOLUME_LINE_HIT_PX` vertically, no narrow horizontal test is needed.
fn volume_line_hit(pos: egui::Pos2, clip_rect: egui::Rect, line_y: f32) -> bool {
    clip_rect.x_range().contains(pos.x) && (pos.y - line_y).abs() <= VOLUME_LINE_HIT_PX
}

/// Starts dragging the volume line: like the fade, local to the
/// single clip. It opens the undo group that will collect the `SetGain`s of
/// every drag frame (see `VolumeDragState::group`).
fn begin_volume_drag(state: &mut TimelineState, history: &mut History, visual: &ClipVisual) {
    state.volume_drag = Some(VolumeDragState {
        clip_id: visual.clip.id,
        track_index: visual.track_index,
        original_db: visual.clip.effects.gain_db.default,
        accum_px: 0.0,
        group: history.begin_group(),
    });
}

/// Gain (dB) of the preview of a volume line drag in progress: the drag is
/// vertical and linear in the drawn "offset" space, not in dB, so the
/// line follows the pointer exactly along the whole run.
fn volume_drag_value(d: &VolumeDragState, half_height: f32) -> f32 {
    if half_height <= 0.0 {
        return d.original_db;
    }
    let offset = gain_offset(d.original_db) - d.accum_px / half_height;
    gain_from_offset(offset)
}

/// Trim to apply on release: the same (already clamped) value shown
/// in the preview.
fn finish_trim(
    t: &TrimState,
    visual: &ClipVisual,
    visuals: &[ClipVisual],
    trimmed_primary_new_value: Option<FrameIdx>,
) -> PendingAction {
    let new_value = trimmed_primary_new_value.unwrap_or(t.original_value);
    let mut trims = vec![(t.clip_id, t.track_index, t.edge, new_value)];
    let mut overwritten: Vec<_> =
        grown_range(&visual.clip, visual.track_index, t.edge, new_value).into_iter().collect();
    for &(other_id, other_track, offset, edge) in &t.followers {
        let Some(other) =
            visuals.iter().find(|v| v.clip.id == other_id && v.track_index == other_track)
        else {
            continue;
        };
        let other_value = new_value + offset;
        trims.push((other_id, other_track, edge, other_value));
        overwritten.extend(grown_range(&other.clip, other_track, edge, other_value));
    }
    PendingAction::Trim { trims, overwritten }
}

/// Move to apply on release, at the same position shown
/// during the drag.
fn finish_drag(
    d: &DragState,
    targets: &[(ClipId, EffectiveTrack)],
    track_kinds: &[TrackKind],
    dragged_primary_new_start: Option<FrameIdx>,
) -> PendingAction {
    let new_start = dragged_primary_new_start.unwrap_or(d.original_start);
    let original_tracks =
        std::iter::once(d.track_index).chain(d.followers.iter().map(|(_, t, _)| *t));
    let starts = std::iter::once(new_start)
        .chain(d.followers.iter().map(|(_, _, offset)| new_start + offset));

    let mut new_video_tracks = 0usize;
    let mut new_audio_tracks = 0usize;
    let moves: Vec<(ClipId, usize, EffectiveTrack, FrameIdx)> = targets
        .iter()
        .zip(original_tracks)
        .zip(starts)
        .map(|(((id, target), from_track), start)| {
            if let EffectiveTrack::New(depth) = *target {
                let count = match track_kinds[from_track] {
                    TrackKind::Video => &mut new_video_tracks,
                    TrackKind::Audio => &mut new_audio_tracks,
                };
                *count = (*count).max(depth);
            }
            (*id, from_track, *target, start)
        })
        .collect();
    PendingAction::Move {
        new_video_tracks,
        new_audio_tracks,
        moves,
        duplicate: d.duplicate,
    }
}

/// Applies one kinetic scroll step to `scroll_val`: damps `vel` (px/s)
/// with the same friction physics as egui's native drag-to-scroll, and
/// zeroes the speed if the resulting scroll hits a limit.
/// Returns `true` if `scroll_val` was updated (a repaint is needed).
fn apply_kinetic_scroll(scroll_val: &mut f32, vel: &mut f32, max_scroll: f32, dt: f32) -> bool {
    if *vel == 0.0 {
        return false;
    }
    let friction = KINETIC_FRICTION * dt;
    if friction > vel.abs() || vel.abs() < KINETIC_STOP_SPEED {
        *vel = 0.0;
        return false;
    }
    *vel -= friction * vel.signum();
    let raw = *scroll_val + *vel * dt;
    let clamped = raw.clamp(0.0, max_scroll);
    if clamped != raw {
        *vel = 0.0;
    }
    *scroll_val = clamped;
    true
}

/// Corrects the saved scroll of the ScrollArea `scroll_id` before its `show`:
/// on zoom the playhead stays still on screen, during playback it stays visible;
/// it also handles the horizontal kinetic touchpad scroll (swipe + inertia
/// after release), since the vertical wheel scroll is already consumed
/// elsewhere for the Video/Audio boxes.
fn sync_timeline_scroll(
    ctx: &egui::Context,
    scroll_id: egui::Id,
    state: &mut TimelineState,
    fps: f64,
    px_per_frame: f32,
    viewport_width: f32,
    content_width: f32,
    playback_active: bool,
    panel_rect: egui::Rect,
    kinetic_scroll_enabled: bool,
) {
    // Zoom changed in this frame: the saved scroll of the
    // ScrollArea (same id) is corrected before the `show`, so the playhead stays still on
    // screen.
    if state.pixels_per_sec != state.last_rendered_pps {
        if let Some(mut scroll_state) =
            egui::containers::scroll_area::State::load(ctx, scroll_id)
        {
            let playhead_secs = state.playhead as f64 / fps;
            scroll_state.offset.x +=
                (playhead_secs as f32) * (state.pixels_per_sec - state.last_rendered_pps);
            scroll_state.store(ctx, scroll_id);
        }
    }
    state.last_rendered_pps = state.pixels_per_sec;

    // During playback the playhead stays visible: if it leaves, the view "turns the page"
    // bringing it to a third from the left. Clamp like egui's in `begin`.
    if playback_active {
        let playhead_x = state.playhead as f32 * px_per_frame;
        let visible_start =
            match egui::containers::scroll_area::State::load(ctx, scroll_id) {
                Some(st) => st.offset.x,
                None => 0.0,
            };
        let visible_end = visible_start + viewport_width;
        const FOLLOW_MARGIN_FRAC: f32 = 1.0 / 3.0;
        if playhead_x < visible_start || playhead_x > visible_end {
            let target = (playhead_x - viewport_width * FOLLOW_MARGIN_FRAC)
                .clamp(0.0, (content_width - viewport_width).max(0.0));
            if let Some(mut scroll_state) =
                egui::containers::scroll_area::State::load(ctx, scroll_id)
            {
                scroll_state.offset.x = target;
                scroll_state.store(ctx, scroll_id);
            }
        }
    }

    let max_offset_x = (content_width - viewport_width).max(0.0);
    let dt = ctx.input(|i| i.stable_dt).min(0.1);
    // During playback the view follows the playhead: a residual inertia would
    // make it slide away from the point it just centered it on above.
    if !kinetic_scroll_enabled || playback_active {
        state.hscroll_vel = 0.0;
    }
    let hovering_panel = pointer_over(ctx, panel_rect);
    // Horizontal swipe in progress: applied immediately (as the
    // ScrollArea would), and its instantaneous speed becomes the inertia to
    // damp when the gesture ends.
    let wheel_x = if hovering_panel {
        ctx.input(|i| i.smooth_scroll_delta.x)
    } else {
        0.0
    };
    if wheel_x != 0.0 {
        if let Some(mut scroll_state) = egui::containers::scroll_area::State::load(ctx, scroll_id)
        {
            scroll_state.offset.x = (scroll_state.offset.x - wheel_x).clamp(0.0, max_offset_x);
            scroll_state.store(ctx, scroll_id);
        }
        state.hscroll_vel = if kinetic_scroll_enabled && !playback_active && dt > 0.0 {
            -KINETIC_VELOCITY_GAIN * wheel_x / dt
        } else {
            0.0
        };
        // Consumed here: the ScrollArea must not reapply it in its `show`.
        ctx.input_mut(|i| i.smooth_scroll_delta.x = 0.0);
    } else if let Some(mut scroll_state) = egui::containers::scroll_area::State::load(ctx, scroll_id)
    {
        let mut offset_x = scroll_state.offset.x;
        if apply_kinetic_scroll(&mut offset_x, &mut state.hscroll_vel, max_offset_x, dt) {
            scroll_state.offset.x = offset_x;
            scroll_state.store(ctx, scroll_id);
            ctx.request_repaint();
        }
    }
}

/// Ruler: ticks, cached frames strip and export markers; click and
/// drag move the playhead.
fn show_ruler(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    origin: egui::Pos2,
    content_width: f32,
    state: &mut TimelineState,
    visuals: &[ClipVisual],
    fps: f64,
    px_per_frame: f32,
    snapping_enabled: bool,
    buffered_ranges: &[(FrameIdx, FrameIdx)],
    max_end_frames: FrameIdx,
) {
    let ruler_rect =
        egui::Rect::from_min_size(origin, egui::vec2(content_width, RULER_HEIGHT));
    painter.rect_filled(ruler_rect, 0.0, egui::Color32::from_gray(45));
    let ruler_resp = ui.interact(
        ruler_rect,
        ui.id().with("timeline_ruler"),
        egui::Sense::click_and_drag(),
    );
    // On a click what counts is where it was released: if the frame
    // arrived late, `interact_pointer_pos` is already
    // the last mouse position after the release.
    let ruler_pos = if ruler_resp.clicked() {
        ui.input(|i| {
            i.events.iter().rev().find_map(|e| match e {
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    ..
                } => Some(*pos),
                _ => None,
            })
        })
        .or_else(|| ruler_resp.interact_pointer_pos())
    } else {
        ruler_resp.interact_pointer_pos()
    };
    if let Some(pos) = ruler_pos {
        let raw_frame =
            (((pos.x - origin.x) / px_per_frame).round() as FrameIdx).max(0);
        state.playhead =
            snap_frame(raw_frame, 0, &visuals, &[], &[], px_per_frame, snapping_enabled);
    }

    // Only the visible ticks: a long timeline zoomed to the frame would have
    // thousands of them off screen.
    let visible_x = ui.clip_rect().intersect(ruler_rect);
    draw_ruler_ticks(&painter, origin, visible_x, state.pixels_per_sec, fps);

    // Below the playhead line, so it stays visible.
    const BUFFERED_STRIP_HEIGHT: f32 = 4.0;
    let buffered_color = egui::Color32::from_rgba_unmultiplied(120, 190, 255, 140);
    for &(start, end) in buffered_ranges {
        let x0 = origin.x + start as f32 * px_per_frame;
        let x1 = origin.x + (end + 1) as f32 * px_per_frame;
        let strip_rect = egui::Rect::from_min_max(
            egui::pos2(x0, origin.y + RULER_HEIGHT - BUFFERED_STRIP_HEIGHT),
            egui::pos2(x1, origin.y + RULER_HEIGHT),
        );
        painter.rect_filled(strip_rect, 0.0, buffered_color);
    }

    if !state.export_marks.is_full(max_end_frames) {
        let (mark_in, mark_out) = state.export_marks.resolve(max_end_frames);
        let band = egui::Rect::from_min_max(
            egui::pos2(origin.x + mark_in as f32 * px_per_frame, origin.y),
            egui::pos2(
                origin.x + mark_out as f32 * px_per_frame,
                origin.y + RULER_HEIGHT - BUFFERED_STRIP_HEIGHT,
            ),
        );
        painter.rect_filled(
            band,
            0.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28),
        );
        for x in [band.left(), band.right()] {
            painter.vline(
                x,
                band.y_range(),
                egui::Stroke::new(1.0, egui::Color32::from_gray(185)),
            );
        }
    }
}

/// Playhead line plus a triangular head in the ruler.
fn paint_playhead(painter: &egui::Painter, origin: egui::Pos2, x_offset: f32, visual_height: f32) {
    let px = origin.x + x_offset;
    let playhead_color = egui::Color32::from_rgb(220, 50, 50);
    painter.line_segment(
        [
            egui::pos2(px, origin.y),
            egui::pos2(px, origin.y + visual_height),
        ],
        egui::Stroke::new(2.0, playhead_color),
    );
    const PLAYHEAD_HEAD_HALF_WIDTH: f32 = 6.0;
    painter.add(egui::Shape::convex_polygon(
        vec![
            egui::pos2(px - PLAYHEAD_HEAD_HALF_WIDTH, origin.y),
            egui::pos2(px + PLAYHEAD_HEAD_HALF_WIDTH, origin.y),
            egui::pos2(px, origin.y + RULER_HEIGHT),
        ],
        playhead_color,
        egui::Stroke::NONE,
    ));
}

fn apply_pending_action(
    project: &mut Project,
    history: &mut History,
    state: &mut TimelineState,
    timeline_id: TimelineId,
    action: PendingAction,
) {
    match action {
        PendingAction::Move {
            new_video_tracks,
            new_audio_tracks,
            moves,
            duplicate,
        } => {
            // Creating in increasing depth order is enough: both for
            // video and for audio, the depth-th created ends up
            // by itself on the right row (see `EffectiveTrack::New`).
            let mut video_tracks = Vec::with_capacity(new_video_tracks);
            for _ in 0..new_video_tracks {
                video_tracks.push(add_track(project, history, timeline_id, TrackKind::Video));
            }
            let mut audio_tracks = Vec::with_capacity(new_audio_tracks);
            for _ in 0..new_audio_tracks {
                audio_tracks.push(add_track(project, history, timeline_id, TrackKind::Audio));
            }
            let moves: Vec<(ClipId, usize, usize, FrameIdx)> = moves
                .into_iter()
                .map(|(id, from_track, dest, start)| {
                    let to_track = match dest {
                        EffectiveTrack::Existing(track) => track,
                        // The kind of the new track is the starting one.
                        EffectiveTrack::New(depth) => {
                            match project.timelines[timeline_id].tracks[from_track].kind {
                                TrackKind::Video => video_tracks[depth - 1],
                                TrackKind::Audio => audio_tracks[depth - 1],
                            }
                        }
                    };
                    (id, from_track, to_track, start)
                })
                .collect();
            if duplicate {
                duplicate_clips(project, history, state, timeline_id, &moves);
                return;
            }
            // The destinations overwrite what was there; the moved clips stay
            // out, even at the starting position.
            let ranges: Vec<(usize, FrameIdx, FrameIdx)> = moves
                .iter()
                .filter_map(|&(id, from_track, to_track, start)| {
                    let len = project.timelines[timeline_id]
                        .clip(from_track, id)?
                        .timeline_len;
                    Some((to_track, start, start + len))
                })
                .collect();
            let exclude: Vec<(usize, ClipId)> = moves
                .iter()
                .flat_map(|&(id, from_track, to_track, _)| [(from_track, id), (to_track, id)])
                .collect();
            let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
            vv_core::make_room_for_ranges(
                project,
                timeline_id,
                &ranges,
                &exclude,
                &mut commands,
            );
            commands.push(Box::new(vv_core::MoveClips::new(timeline_id, moves)));
            history.do_command(project, Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::MoveClips, commands)));
        }
        PendingAction::Trim { trims, overwritten } => {
            // The stretch gained by lengthening overwrites what was there; the trimmed
            // clips stay out, or they would cut themselves.
            let exclude: Vec<(usize, ClipId)> = trims
                .iter()
                .map(|&(clip_id, track_index, _, _)| (track_index, clip_id))
                .collect();
            let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
            vv_core::make_room_for_ranges(
                project,
                timeline_id,
                &overwritten,
                &exclude,
                &mut commands,
            );
            commands.extend(trims.into_iter().map(
                |(clip_id, track_index, edge, new_value)| {
                    Box::new(vv_core::TrimClip::new(
                        timeline_id,
                        track_index,
                        clip_id,
                        edge,
                        new_value,
                    )) as Box<dyn vv_core::Command>
                },
            ));
            history.do_command(project, Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::TrimClips, commands)));
        }
        PendingAction::SetFade { track_index, clip_id, edge, new_value } => {
            history.do_command(
                project,
                Box::new(vv_core::SetClipFade::new(timeline_id, track_index, clip_id, edge, new_value)),
            );
        }
        PendingAction::SetGain { track_index, clip_id, new_value } => {
            history.do_command(
                project,
                Box::new(vv_core::set_clip_gain(timeline_id, track_index, clip_id, new_value)),
            );
        }
        PendingAction::ApplyFilter { track_index, clip_id, filter } => {
            // If it is already present (dropping it again) it is only re-enabled,
            // instead of being duplicated at the end.
            let new_filters = project.timelines[timeline_id].clip(track_index, clip_id).map(|clip| {
                let mut filters = clip.effects.filters.clone();
                match filters.iter_mut().find(|f| f.kind == filter) {
                    Some(existing) => existing.enabled = true,
                    None => filters.push(vv_core::ClipFilter { kind: filter, enabled: true }),
                }
                filters
            });
            if let Some(filters) = new_filters {
                history.do_command(
                    project,
                    Box::new(vv_core::set_clip_filters(timeline_id, track_index, clip_id, filters)),
                );
            }
        }
        PendingAction::ApplyTransition { track_index, clip_id, edge, kind } => {
            let default_duration =
                (project.timelines[timeline_id].fps.as_f64() * 0.45).round() as FrameIdx;
            let transition = vv_core::Transition {
                kind,
                duration: default_duration.max(1),
                direction: vv_core::PushDirection::Right,
                ease: vv_core::Ease::InOut,
                curve: 0.5,
            };
            let neighbor = adjacent_clip(&project.timelines[timeline_id].tracks[track_index], clip_id, edge);
            if let Some(neighbor_id) = neighbor {
                let (left_id, right_id) = match edge {
                    FadeEdge::In => (neighbor_id, clip_id),
                    FadeEdge::Out => (clip_id, neighbor_id),
                };
                apply_new_crossing(project, history, state, timeline_id, track_index, left_id, right_id, transition);
            } else if let Some(clip) = project.timelines[timeline_id].clip(track_index, clip_id) {
                let mut transition = transition;
                transition.duration = transition.duration.clamp(1, clip.timeline_len.max(1));
                history.do_command(
                    project,
                    Box::new(vv_core::set_clip_transition(timeline_id, track_index, clip_id, edge, Some(transition))),
                );
                state.selected.clear();
                state.selection_anchor = None;
                state.selected_gap = None;
                state.selected_transition = Some(TransitionSelection::Edge((track_index, clip_id), edge));
            }
        }
        PendingAction::SetTransitionDuration { track_index, clip_id, edge, new_value } => {
            if let Some(clip) = project.timelines[timeline_id].clip(track_index, clip_id) {
                let mut transition = match edge {
                    FadeEdge::In => clip.effects.transition_in.clone(),
                    FadeEdge::Out => clip.effects.transition_out.clone(),
                };
                if let Some(t) = &mut transition {
                    t.duration = new_value.clamp(1, clip.timeline_len.max(1));
                }
                if let Some(transition) = transition {
                    history.do_command(
                        project,
                        Box::new(vv_core::set_clip_transition(timeline_id, track_index, clip_id, edge, Some(transition))),
                    );
                }
            }
        }
        PendingAction::SetCrossingDuration { track_index, left_clip, new_value } => {
            let track = &project.timelines[timeline_id].tracks[track_index];
            if let Some(mut crossing) = track.crossing_from(left_clip).cloned() {
                let max_duration = match (track.clip(crossing.left_clip), track.clip(crossing.right_clip)) {
                    (Some(left), Some(right)) => (2 * left.timeline_len.min(right.timeline_len)).max(1),
                    _ => new_value.max(1),
                };
                crossing.transition.duration = new_value.clamp(1, max_duration);
                history.do_command(
                    project,
                    Box::new(vv_core::SetCrossTransition::new(timeline_id, track_index, left_clip, Some(crossing))),
                );
            }
        }
        PendingAction::DuplicateTransition { track_index, clip_id, edge, transition } => {
            let neighbor = adjacent_clip(&project.timelines[timeline_id].tracks[track_index], clip_id, edge);
            if let Some(neighbor_id) = neighbor {
                let (left_id, right_id) = match edge {
                    FadeEdge::In => (neighbor_id, clip_id),
                    FadeEdge::Out => (clip_id, neighbor_id),
                };
                apply_new_crossing(project, history, state, timeline_id, track_index, left_id, right_id, transition);
            } else if let Some(clip) = project.timelines[timeline_id].clip(track_index, clip_id) {
                let mut transition = transition;
                transition.duration = transition.duration.clamp(1, clip.timeline_len.max(1));
                history.do_command(
                    project,
                    Box::new(vv_core::set_clip_transition(timeline_id, track_index, clip_id, edge, Some(transition))),
                );
                state.selected.clear();
                state.selection_anchor = None;
                state.selected_gap = None;
                state.selected_transition = Some(TransitionSelection::Edge((track_index, clip_id), edge));
            }
        }
        PendingAction::Unlink(track_index, clip_id) => {
            history.do_command(
                project,
                Box::new(vv_core::UnlinkClip::new(timeline_id, track_index, clip_id)),
            );
        }
        PendingAction::Link(targets) => {
            history.do_command(
                project,
                Box::new(vv_core::LinkClips::new(timeline_id, targets)),
            );
        }
        PendingAction::SetTrackFlag(track_index, flag, value) => {
            history.do_command(
                project,
                Box::new(vv_core::SetTrackFlag::new(timeline_id, track_index, flag, value)),
            );
            if flag == TrackFlag::Locked && value {
                state.drop_locked(&project.timelines[timeline_id]);
            }
        }
        PendingAction::RemoveTrack(track_index) => {
            history.do_command(
                project,
                Box::new(vv_core::RemoveTrack::new(timeline_id, track_index)),
            );
            // The track indices of the selection are no longer valid: it is cleared.
            state.clear_selection();
        }
        PendingAction::MakeCompound(clips) => {
            make_compound_clip(project, history, timeline_id, clips);
            state.clear_selection();
        }
    }
}

/// Removes `clips` from the timeline (several clips too, video and audio together, on
/// several tracks) and puts a compound clip in their place: the pool and its
/// nested timeline stay out of the history like an import (see the docs
/// of `vv_core::compound_clip_commands`), only the insertion of the resulting
/// clip is undoable.
fn make_compound_clip(
    project: &mut Project,
    history: &mut History,
    timeline_id: TimelineId,
    clips: Vec<ClipKey>,
) {
    let Some(plan) = vv_core::plan_compound_clip(project, timeline_id, &clips) else {
        return;
    };
    let fps = plan.nested_timeline.fps;
    let resolution = plan.nested_timeline.resolution;
    let (has_video, has_audio, len) = (plan.has_video, plan.has_audio, plan.len);
    let nested_id = project.timelines.insert(plan.nested_timeline.clone());
    let name = project.alloc_compound_name();
    let content_hash = project.alloc_compound_generation();
    let media_id = project.media_pool.insert(vv_core::MediaItem {
        path: name.into(),
        meta: vv_core::MediaMeta {
            duration_frames: len,
            fps,
            width: resolution.0,
            height: resolution.1,
            has_video,
            has_audio,
            sample_rate: 48_000,
            channels: 2,
            audio_streams: 1,
        },
        content_hash,
        compound: Some(nested_id),
    });
    let commands = vv_core::compound_clip_commands(project, timeline_id, &clips, &plan, media_id);
    history.do_command(
        project,
        Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::MakeCompoundClip, commands)),
    );
}

/// Appends a track and returns its index.
pub fn add_track(
    project: &mut Project,
    history: &mut History,
    timeline_id: TimelineId,
    kind: TrackKind,
) -> usize {
    let index = project.timelines[timeline_id].tracks.len();
    history.do_command(project, Box::new(vv_core::AddTrack::new(timeline_id, kind)));
    index
}

/// Inserts a copy of every clip of `moves` at the destination,
/// overwriting like a move. The copies become the selection.
fn duplicate_clips(
    project: &mut Project,
    history: &mut History,
    state: &mut TimelineState,
    timeline_id: TimelineId,
    moves: &[(ClipId, usize, usize, FrameIdx)],
) {
    let copies: Vec<(usize, Clip, Option<vv_core::LinkGroupId>)> = moves
        .iter()
        .filter_map(|&(id, from_track, to_track, start)| {
            let original = project.timelines[timeline_id].clip(from_track, id)?;
            let mut clip = original.clone();
            clip.timeline_start = start;
            clip.linked_group = None;
            Some((to_track, clip, original.linked_group))
        })
        .collect();
    let copies: Vec<_> = copies
        .into_iter()
        .map(|(track, mut clip, group)| {
            clip.id = project.alloc_clip_id();
            (track, clip, group)
        })
        .collect();
    let new_selection: BTreeSet<ClipKey> =
        copies.iter().map(|(track, clip, _)| (*track, clip.id)).collect();
    let commands = vv_core::insert_overwriting(project, timeline_id, copies);
    history.do_command(project, Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::DuplicateClips, commands)));

    let anchor = new_selection.iter().next().copied();
    state.set_selection(new_selection, anchor);
}

/// `offline`: the media of the clip is no longer in the media pool (deleted from
/// there, see `vv_core::RemoveMedia`) — the clip stays on the timeline but turns
/// red, and the player shows "Media offline".
fn clip_label_and_color(
    clip: &Clip,
    track: &Track,
    offline: bool,
    media_labels: &dyn Fn(vv_core::MediaId) -> String,
) -> (String, egui::Color32) {
    match &clip.source {
        vv_core::ClipSource::Media(media_id) => {
            if offline {
                return (t!("timeline.media_offline").into_owned(), OFFLINE_COLOR);
            }
            let label = media_labels(*media_id);
            let color = if track.kind == TrackKind::Video {
                egui::Color32::from_rgb(90, 140, 200)
            } else {
                egui::Color32::from_rgb(90, 190, 140)
            };
            (label, darken_if_edited(color, clip))
        }
        vv_core::ClipSource::SolidColor => (
            t!("generator.solid_color").into_owned(),
            darken_if_edited(egui::Color32::from_rgb(200, 170, 90), clip),
        ),
        vv_core::ClipSource::Text => (
            clip.effects
                .title
                .as_ref()
                .and_then(|t| t.content.lines().next())
                .map_or_else(|| t!("generator.text").into_owned(), str::to_string),
            darken_if_edited(egui::Color32::from_rgb(170, 110, 200), clip),
        ),
    }
}

/// Clips with some effect changed from the default stand out
/// at a glance on the timeline: same color, darker shade.
fn darken_if_edited(color: egui::Color32, clip: &Clip) -> egui::Color32 {
    if clip.effects.is_pristine() {
        return color;
    }
    const F: f32 = 0.62;
    egui::Color32::from_rgb(
        (color.r() as f32 * F) as u8,
        (color.g() as f32 * F) as u8,
        (color.b() as f32 * F) as u8,
    )
}

const DISABLED_BADGE_SIZE: f32 = 12.0;

/// Small red crossed-out square in front of the name of a disabled clip.
fn paint_disabled_badge(painter: &egui::Painter, top_left: egui::Pos2) {
    let rect = egui::Rect::from_min_size(
        top_left + egui::vec2(0.0, 1.0),
        egui::vec2(DISABLED_BADGE_SIZE, DISABLED_BADGE_SIZE),
    );
    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(200, 60, 60));
    let inner = rect.shrink(3.0);
    painter.line_segment(
        [inner.left_bottom(), inner.right_top()],
        egui::Stroke::new(1.5, egui::Color32::WHITE),
    );
}

/// Rectangle of a clip in content-local coordinates: same
/// geometry for drawing, selection rectangle and shift+click.
fn clip_local_rect(visual: &ClipVisual, px_per_frame: f32, row_y: &[f32]) -> egui::Rect {
    let x = visual.clip.timeline_start as f32 * px_per_frame;
    let y = row_y[visual.track_index];
    let w = (visual.clip.timeline_len as f32 * px_per_frame).max(2.0);
    egui::Rect::from_min_size(egui::pos2(x, y + 2.0), egui::vec2(w, ROW_HEIGHT - 4.0))
}

/// Waveform of an audio clip, one line per visible column. The bin of
/// each column comes from the absolute time in the audio: splitting the clip does not
/// move the waveform.
fn draw_clip_waveform(
    painter: &egui::Painter,
    clip_rect: egui::Rect,
    peaks: &[f32],
    clip_start_secs: f64,
    clip_end_secs: f64,
    media_fps: f64,
    audio_duration_secs: f64,
    visible_rect: egui::Rect,
    gain_db: &Keyframed<f32>,
) {
    if peaks.is_empty() || audio_duration_secs <= 0.0 || media_fps <= 0.0 {
        return;
    }
    if clip_end_secs <= clip_start_secs {
        return;
    }

    // Visible portion of the clip (no column outside the viewport).
    let vis = clip_rect.intersect(visible_rect);
    if !vis.is_positive() {
        return;
    }

    let center_y = clip_rect.center().y;
    let half_height = clip_rect.height() / 2.0;
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 140));
    let clipped_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 90, 90, 200));

    let width = clip_rect.width();
    let mut x = vis.min.x;
    while x < vis.max.x {
        let frac = ((x - clip_rect.min.x) / width) as f64;
        let bin = waveform_bin_for_column(
            frac,
            clip_start_secs,
            clip_end_secs,
            audio_duration_secs,
            peaks.len(),
        );
        // The gain lives in *source* frames, as in the mixer: the shape
        // drawn is the one that will really be heard, clipping included.
        let secs = clip_start_secs + frac * (clip_end_secs - clip_start_secs);
        let source_frame = (secs * media_fps).floor() as FrameIdx;
        let amplified = peaks[bin] * vv_audio::mixer::db_to_linear(gain_db.value_at(source_frame));
        let h = (half_height * amplified.min(1.0)).max(0.5);
        painter.line_segment(
            [egui::pos2(x, center_y - h), egui::pos2(x, center_y + h)],
            if amplified > 1.0 { clipped_stroke } else { stroke },
        );
        x += 1.0;
    }
}

/// Bin of the column at `frac` of the clip, from the absolute time in the audio.
fn waveform_bin_for_column(
    frac: f64,
    clip_start_secs: f64,
    clip_end_secs: f64,
    audio_duration_secs: f64,
    num_peaks: usize,
) -> usize {
    let t_secs = clip_start_secs + frac * (clip_end_secs - clip_start_secs);
    ((t_secs / audio_duration_secs * num_peaks as f64) as usize).min(num_peaks.saturating_sub(1))
}

/// The clips whose rectangle intersects `rect` (local coordinates): the core
/// shared by marquee-select and shift+click (which uses the rectangle
/// joining the anchor and the clicked clip).
fn clips_intersecting_rect(
    visuals: &[ClipVisual],
    px_per_frame: f32,
    row_y: &[f32],
    rect: egui::Rect,
) -> Vec<ClipKey> {
    visuals
        .iter()
        .filter(|v| !v.locked && clip_local_rect(v, px_per_frame, row_y).intersects(rect))
        .map(|v| (v.track_index, v.clip.id))
        .collect()
}

/// The gap covering `frame` on the track, if followed by another clip.
fn gap_at(
    visuals: &[ClipVisual],
    track_index: usize,
    frame: FrameIdx,
) -> Option<(FrameIdx, FrameIdx)> {
    let mut track_clips: Vec<&Clip> = visuals
        .iter()
        .filter(|v| v.track_index == track_index)
        .map(|v| v.clip.as_ref())
        .collect();
    track_clips.sort_by_key(|c| c.timeline_start);

    if track_clips
        .iter()
        .any(|c| c.contains(frame))
    {
        return None; // `frame` is inside a clip, not in a gap.
    }
    let next = track_clips.iter().find(|c| c.timeline_start > frame)?;
    let gap_start = track_clips
        .iter()
        .filter(|c| c.timeline_end() <= frame)
        .map(|c| c.timeline_end())
        .max()
        .unwrap_or(0);
    Some((gap_start, next.timeline_start))
}

/// Plain click, ctrl (adds/removes), shift (range with the rectangle
/// between anchor and clip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickModifiers {
    Plain,
    Toggle,
    Range,
}

fn click_modifiers(modifiers: egui::Modifiers) -> ClickModifiers {
    if modifiers.shift {
        ClickModifiers::Range
    } else if modifiers.command {
        ClickModifiers::Toggle
    } else {
        ClickModifiers::Plain
    }
}

/// Applies a click (with any modifiers) on the clip `clicked`,
/// given the current selection and anchor. A pure function, without any
/// `egui::Ui`: testable with simple data.
fn apply_click_selection(
    current: &BTreeSet<ClipKey>,
    anchor: Option<ClipKey>,
    clicked: ClipKey,
    modifiers: ClickModifiers,
    visuals: &[ClipVisual],
    px_per_frame: f32,
    row_y: &[f32],
) -> (BTreeSet<ClipKey>, Option<ClipKey>) {
    match modifiers {
        ClickModifiers::Plain => (BTreeSet::from([clicked]), Some(clicked)),
        ClickModifiers::Toggle => {
            let mut set = current.clone();
            if !set.remove(&clicked) {
                set.insert(clicked);
            }
            (set, Some(clicked))
        }
        ClickModifiers::Range => {
            let effective_anchor = anchor.unwrap_or(clicked);
            let anchor_rect = visuals
                .iter()
                .find(|v| (v.track_index, v.clip.id) == effective_anchor)
                .map(|v| clip_local_rect(v, px_per_frame, row_y));
            let clicked_rect = visuals
                .iter()
                .find(|v| (v.track_index, v.clip.id) == clicked)
                .map(|v| clip_local_rect(v, px_per_frame, row_y));
            let set = match (anchor_rect, clicked_rect) {
                (Some(a), Some(c)) => {
                    clips_intersecting_rect(visuals, px_per_frame, row_y, a.union(c))
                        .into_iter()
                        .collect()
                }
                _ => BTreeSet::from([clicked]),
            };
            (set, Some(effective_anchor))
        }
    }
}

/// Bounds imposed by the neighbors on `track_index` for a clip of length `len`
/// positioned (only to decide who comes before/after) at `reference_start` —
/// it does not have to already be there: also used for a track change mid-drag.
fn neighbor_bounds_at(
    visuals: &[ClipVisual],
    track_index: usize,
    exclude: &[ClipId],
    reference_start: FrameIdx,
    len: FrameIdx,
) -> (FrameIdx, FrameIdx) {
    let mut lower_bound: FrameIdx = 0;
    let mut upper_bound: FrameIdx = FrameIdx::MAX;
    let reference_end = reference_start + len;

    for v in visuals {
        if v.track_index != track_index || exclude.contains(&v.clip.id) {
            continue;
        }
        if v.clip.timeline_end() <= reference_start {
            lower_bound = lower_bound.max(v.clip.timeline_end());
        }
        if v.clip.timeline_start >= reference_end {
            upper_bound = upper_bound.min(v.clip.timeline_start);
        }
    }

    (lower_bound, upper_bound)
}

fn max_start_in_slot(lower: FrameIdx, upper: FrameIdx, len: FrameIdx) -> FrameIdx {
    upper.saturating_sub(len).max(lower)
}

/// Like `drag_range`, for a clip of length `len` evaluated at `reference_start`
/// on `track_index` (even if it does not fit there yet) ignoring `exclude`.
fn drag_range_at(
    visuals: &[ClipVisual],
    track_index: usize,
    exclude: &[ClipId],
    reference_start: FrameIdx,
    len: FrameIdx,
) -> (FrameIdx, FrameIdx) {
    let (lower, upper) = neighbor_bounds_at(visuals, track_index, exclude, reference_start, len);
    (lower, max_start_in_slot(lower, upper, len))
}

/// Valid range (min/max) for the new `timeline_start` of a clip on its
/// own, already resolved (not a raw "upper"): used both directly and
/// as a base to combine with that of a linked twin.
fn drag_range(visuals: &[ClipVisual], track_index: usize, clip_id: ClipId) -> (FrameIdx, FrameIdx) {
    let Some(v) = visuals
        .iter()
        .find(|v| v.track_index == track_index && v.clip.id == clip_id)
    else {
        return (0, FrameIdx::MAX);
    };
    drag_range_at(
        visuals,
        track_index,
        &[clip_id],
        v.clip.timeline_start,
        v.clip.timeline_len,
    )
}

/// Extends the clips to their linked groups. The single place doing it for the
/// selections made on the timeline.
fn expand_to_linked_groups(
    visuals: &[ClipVisual],
    keys: impl IntoIterator<Item = ClipKey>,
) -> BTreeSet<ClipKey> {
    let mut result: BTreeSet<ClipKey> = BTreeSet::new();
    for (track_index, clip_id) in keys {
        let Some(visual) = visuals
            .iter()
            .find(|v| v.track_index == track_index && v.clip.id == clip_id && !v.locked)
        else {
            continue;
        };
        result.insert((track_index, clip_id));
        if let Some(group) = visual.clip.linked_group {
            for v in visuals
                .iter()
                .filter(|v| v.clip.linked_group == Some(group) && !v.locked)
            {
                result.insert((v.track_index, v.clip.id));
            }
        }
    }
    result
}

/// Clips moving with `clicked`: the selection if it contains it,
/// otherwise it and its group.
fn drag_group_for(
    selected: &BTreeSet<ClipKey>,
    visuals: &[ClipVisual],
    clicked: ClipKey,
) -> BTreeSet<ClipKey> {
    if selected.contains(&clicked) {
        selected.clone()
    } else {
        expand_to_linked_groups(visuals, [clicked])
    }
}

/// Range of the `timeline_start` of `clip_id` respecting the constraints of all
/// the `others`, and their offsets for `DragState::followers`.
fn combined_drag_range(
    visuals: &[ClipVisual],
    track_index: usize,
    clip_id: ClipId,
    others: &[ClipKey],
) -> (FrameIdx, FrameIdx, Vec<(ClipId, usize, FrameIdx)>) {
    let (mut min_start, mut max_start) = drag_range(visuals, track_index, clip_id);

    let Some(this_start) = visuals
        .iter()
        .find(|v| v.track_index == track_index && v.clip.id == clip_id)
        .map(|v| v.clip.timeline_start)
    else {
        return (min_start, max_start, Vec::new());
    };

    let mut followers = Vec::new();
    for &(other_track, other_id) in others {
        if other_track == track_index && other_id == clip_id {
            continue;
        }
        let Some(other) = visuals
            .iter()
            .find(|v| v.track_index == other_track && v.clip.id == other_id)
        else {
            continue;
        };
        let offset = other.clip.timeline_start - this_start;
        let (o_min, o_max) = drag_range(visuals, other_track, other_id);
        // Saturating: without neighbors `o_max` is ~`FrameIdx::MAX`.
        min_start = min_start.max(o_min.saturating_sub(offset));
        max_start = max_start.min(o_max.saturating_sub(offset));
        followers.push((other_id, other_track, offset));
    }
    (min_start, max_start, followers)
}

/// Like `combined_drag_range`, with every clip on its own target track.
/// The neighbors exclude the whole group, or two clips headed for the same
/// track would block each other.
fn group_drag_bounds(
    visuals: &[ClipVisual],
    reference_start: FrameIdx,
    targets: &[(ClipId, EffectiveTrack)],
    followers: &[(ClipId, usize, FrameIdx)],
) -> (FrameIdx, FrameIdx) {
    let exclude: Vec<ClipId> = targets.iter().map(|(id, _)| *id).collect();
    let bound_for = |id: ClipId, target: EffectiveTrack, reference: FrameIdx| -> (FrameIdx, FrameIdx) {
        let len = visuals
            .iter()
            .find(|v| v.clip.id == id)
            .map(|v| v.clip.timeline_len)
            .unwrap_or(0);
        match target {
            EffectiveTrack::Existing(track) => {
                drag_range_at(visuals, track, &exclude, reference, len)
            }
            EffectiveTrack::New(_) => (0, max_start_in_slot(0, FrameIdx::MAX, len)),
        }
    };

    let (primary_id, primary_target) = targets[0];
    let (mut min_start, mut max_start) = bound_for(primary_id, primary_target, reference_start);

    for (i, &(follower_id, _, offset)) in followers.iter().enumerate() {
        let (_, follower_target) = targets[i + 1];
        let (o_min, o_max) = bound_for(follower_id, follower_target, reference_start + offset);
        // Saturating: without neighbors `o_max` is ~`FrameIdx::MAX`.
        min_start = min_start.max(o_min.saturating_sub(offset));
        max_start = max_start.min(o_max.saturating_sub(offset));
    }
    (min_start, max_start.max(min_start))
}

/// Range of the trimmed edge combined with that of the `others`, and their
/// offsets for `TrimState::followers`.
fn combined_trim_range(
    visuals: &[ClipVisual],
    project: &Project,
    primary: ClipKey,
    edge: TrimEdge,
    others: &[(ClipKey, TrimEdge)],
) -> (FrameIdx, FrameIdx, Vec<(ClipId, usize, FrameIdx, TrimEdge)>) {
    let find = |(track, id): ClipKey| {
        visuals
            .iter()
            .find(|v| v.track_index == track && v.clip.id == id)
    };
    let Some(primary_visual) = find(primary) else {
        return (0, FrameIdx::MAX, Vec::new());
    };
    let edge_value = |clip: &Clip, edge: TrimEdge| match edge {
        TrimEdge::Start => clip.timeline_start,
        TrimEdge::End => clip.timeline_end(),
    };
    let primary_value = edge_value(&primary_visual.clip, edge);
    let trimmed: Vec<(&ClipVisual, TrimEdge)> = std::iter::once((primary_visual, edge))
        .chain(others.iter().filter_map(|&(k, e)| find(k).map(|v| (v, e))))
        .collect();

    let mut min_value = FrameIdx::MIN;
    let mut max_value = FrameIdx::MAX;
    let mut followers = Vec::new();
    for &(v, v_edge) in &trimmed {
        let offset = edge_value(&v.clip, v_edge) - primary_value;
        let (mut o_min, mut o_max) = single_trim_range(project, &v.clip, v_edge);
        // Two clips trimmed together on the same track must not
        // lengthen over each other; in a roll, instead, the edge of the
        // neighbor moves with this one.
        for &(w, w_edge) in trimmed.iter().filter(|(w, w_edge)| {
            w.track_index == v.track_index && w.clip.id != v.clip.id && *w_edge == v_edge
        }) {
            match w_edge {
                TrimEdge::End if w.clip.timeline_start >= v.clip.timeline_end() => {
                    o_max = o_max.min(w.clip.timeline_start);
                }
                TrimEdge::Start if w.clip.timeline_end() <= v.clip.timeline_start => {
                    o_min = o_min.max(w.clip.timeline_end());
                }
                _ => {}
            }
        }
        min_value = min_value.max(o_min.saturating_sub(offset));
        max_value = max_value.min(o_max.saturating_sub(offset));
        if v.clip.id != primary_visual.clip.id || v.track_index != primary_visual.track_index {
            followers.push((v.clip.id, v.track_index, offset, v_edge));
        }
    }
    (min_value, max_value, followers)
}

/// Edge-sensitive zones of a clip `width` pixels wide, given who is in
/// contact with it on the left (`start_neighbor`) and on the right (`end_neighbor`).
struct EdgeZones {
    width: f32,
    roll_px: f32,
    trim_px: f32,
    start_neighbor: Option<ClipKey>,
    end_neighbor: Option<ClipKey>,
}

fn edge_zones(
    width: f32,
    start_neighbor: Option<ClipKey>,
    end_neighbor: Option<ClipKey>,
) -> EdgeZones {
    EdgeZones {
        width,
        roll_px: ROLL_HANDLE_PX.min(width / 6.0),
        trim_px: TRIM_HANDLE_PX.min(width / 3.0),
        start_neighbor,
        end_neighbor,
    }
}

impl EdgeZones {
    /// `local_x`: distance from the left edge of the clip.
    fn at(&self, local_x: f32) -> Option<EdgeZone> {
        let sides = [
            (local_x, TrimEdge::Start, self.start_neighbor),
            (self.width - local_x, TrimEdge::End, self.end_neighbor),
        ];
        for (distance, edge, neighbor) in sides {
            match neighbor {
                Some(neighbor) if distance < self.roll_px => {
                    return Some(EdgeZone::Roll { edge, neighbor });
                }
                Some(_) if distance < self.roll_px + self.trim_px => {
                    return Some(EdgeZone::Trim(edge));
                }
                None if distance < self.trim_px => return Some(EdgeZone::Trim(edge)),
                _ => {}
            }
        }
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EdgeCursor {
    TrimStart,
    TrimEnd,
    Roll,
}

impl EdgeCursor {
    fn from_zone(zone: EdgeZone) -> Self {
        match zone {
            EdgeZone::Roll { .. } => EdgeCursor::Roll,
            EdgeZone::Trim(TrimEdge::Start) => EdgeCursor::TrimStart,
            EdgeZone::Trim(TrimEdge::End) => EdgeCursor::TrimEnd,
        }
    }
}

/// egui has no custom cursors: the system one is hidden and
/// this is drawn in its place. Brackets "[" / "]" like the edges of a
/// clip, with the drag arrows.
fn paint_edge_cursor(ctx: &egui::Context, pos: egui::Pos2, cursor: EdgeCursor) {
    ctx.set_cursor_icon(egui::CursorIcon::None);
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("timeline_edge_cursor"),
    ));
    const HALF_H: f32 = 8.0;
    const TICK: f32 = 4.0;
    let bracket = |x: f32, towards: f32| {
        vec![
            egui::pos2(x + towards * TICK, pos.y - HALF_H),
            egui::pos2(x, pos.y - HALF_H),
            egui::pos2(x, pos.y + HALF_H),
            egui::pos2(x + towards * TICK, pos.y + HALF_H),
        ]
    };
    // (tip x, direction)
    let arrow = |tip: f32, dir: f32| {
        vec![
            egui::pos2(tip, pos.y),
            egui::pos2(tip - dir * 5.0, pos.y - 4.5),
            egui::pos2(tip - dir * 5.0, pos.y + 4.5),
        ]
    };
    let (brackets, arrows) = match cursor {
        EdgeCursor::TrimEnd => (vec![bracket(pos.x, -1.0)], vec![arrow(pos.x - 11.0, -1.0), arrow(pos.x + 8.0, 1.0)]),
        EdgeCursor::TrimStart => (vec![bracket(pos.x, 1.0)], vec![arrow(pos.x - 8.0, -1.0), arrow(pos.x + 11.0, 1.0)]),
        EdgeCursor::Roll => (
            vec![bracket(pos.x - 2.0, -1.0), bracket(pos.x + 2.0, 1.0)],
            vec![arrow(pos.x - 10.0, -1.0), arrow(pos.x + 10.0, 1.0)],
        ),
    };
    for points in &brackets {
        painter.line(points.clone(), egui::Stroke::new(4.0, egui::Color32::BLACK));
    }
    for points in brackets {
        painter.line(points, egui::Stroke::new(2.0, egui::Color32::WHITE));
    }
    for points in arrows {
        painter.add(egui::Shape::convex_polygon(
            points,
            egui::Color32::WHITE,
            egui::Stroke::new(1.0, egui::Color32::BLACK),
        ));
    }
}

/// The neighbors do not limit the trim (they get overwritten on release): only the
/// source and the opposite edge do.
fn single_trim_range(project: &Project, clip: &Clip, edge: TrimEdge) -> (FrameIdx, FrameIdx) {
    match edge {
        TrimEdge::Start => {
            // Not past the end minus 1 frame (at least one frame
            // of content must remain) and not before the start of the source
            // (source_in cannot go below 0).
            let min_value = clip.timeline_frame_at(0).max(0);
            let max_value = clip.timeline_end() - 1;
            (min_value, max_value.max(min_value))
        }
        TrimEdge::End => {
            // Not past the start plus 1 frame and not past the real duration
            // of the source (unlimited for a SolidColor generator, which
            // does not have one).
            let max_value = media_duration_frames(project, clip)
                .map(|max_source_out| clip.timeline_frame_at(max_source_out))
                .unwrap_or(FrameIdx::MAX);
            let min_value = clip.timeline_start + 1;
            (min_value, max_value.max(min_value))
        }
    }
}

fn media_duration_frames(project: &Project, clip: &Clip) -> Option<FrameIdx> {
    match &clip.source {
        ClipSource::Media(media_id) => project
            .media_pool
            .get(*media_id)
            .map(|item| item.meta.duration_frames),
        ClipSource::SolidColor | ClipSource::Text => None,
    }
}

/// The stretch of timeline a clip takes by lengthening `edge` up to
/// `new_value`, if it lengthened: `None` if it shortened instead.
fn grown_range(
    clip: &Clip,
    track_index: usize,
    edge: TrimEdge,
    new_value: FrameIdx,
) -> Option<(usize, FrameIdx, FrameIdx)> {
    match edge {
        TrimEdge::Start if new_value < clip.timeline_start => {
            Some((track_index, new_value, clip.timeline_start))
        }
        TrimEdge::End if new_value > clip.timeline_end() => {
            Some((track_index, clip.timeline_end(), new_value))
        }
        _ => None,
    }
}

/// Snapping threshold, in screen pixels (not in frames:
/// it stays the same visual distance at any zoom level, converted
/// into frames by `snap_frame` based on `px_per_frame`).
const SNAP_THRESHOLD_PX: f32 = 10.0;

/// Edges of the non-excluded clips plus `extra_targets` (the playhead).
fn snap_targets<'a>(
    visuals: &'a [ClipVisual],
    exclude: &'a [ClipId],
    extra_targets: &'a [FrameIdx],
) -> impl Iterator<Item = FrameIdx> + 'a {
    visuals
        .iter()
        .filter(|v| !exclude.contains(&v.clip.id))
        .flat_map(|v| [v.clip.timeline_start, v.clip.timeline_end()])
        .chain(extra_targets.iter().copied())
}

/// With snapping on, it snaps the start or the end of the clip of length
/// `len` to the nearest edge within `SNAP_THRESHOLD_PX`. `exclude` does not count.
fn snap_frame(
    candidate_start: FrameIdx,
    len: FrameIdx,
    visuals: &[ClipVisual],
    exclude: &[ClipId],
    extra_targets: &[FrameIdx],
    px_per_frame: f32,
    enabled: bool,
) -> FrameIdx {
    if !enabled {
        return candidate_start;
    }
    let threshold = (SNAP_THRESHOLD_PX / px_per_frame).round() as FrameIdx;
    if threshold <= 0 {
        return candidate_start;
    }
    let candidate_end = candidate_start + len;

    let mut best: Option<(FrameIdx, FrameIdx)> = None; // (|gap|, new candidate_start)
    for edge in snap_targets(visuals, exclude, extra_targets) {
        // (point of the dragged clip to compare with the edge, new
        // candidate_start if this is the chosen snap)
        for (point, new_start) in [(candidate_start, edge), (candidate_end, edge - len)] {
            let delta = (point - edge).abs();
            if delta > threshold {
                continue;
            }
            if best.is_none_or(|(best_delta, _)| delta < best_delta) {
                best = Some((delta, new_start));
            }
        }
    }
    best.map_or(candidate_start, |(_, new_start)| new_start)
}

/// Highlights a "new track" zone under a drag in progress.
fn paint_drop_zone(painter: &egui::Painter, rect: egui::Rect, label: Option<&str>) {
    let green = egui::Color32::from_rgb(120, 220, 120);
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgba_unmultiplied(120, 220, 120, 60));
    painter.rect_stroke(rect, 4.0, egui::Stroke::new(2.0, green), egui::StrokeKind::Inside);
    if let Some(label) = label {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(13.0),
            egui::Color32::from_rgb(200, 255, 200),
        );
    }
}

const PROXY_STRIP_HEIGHT: f32 = 4.0;
/// Clips whose media is no longer in the media pool.
pub const OFFLINE_COLOR: egui::Color32 = egui::Color32::from_rgb(170, 50, 50);

/// "Proxy available" indicator, shared with the media pool.
pub const PROXY_COLOR: egui::Color32 = egui::Color32::from_rgba_premultiplied(220, 151, 52, 220);

/// Border of the clip while a filter from the Effects panel is dragged over it.
const FILTER_HIGHLIGHT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 190, 60);

fn paint_proxy_strip(painter: &egui::Painter, rect: egui::Rect) {
    let strip_rect = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(1.0, 1.0),
        egui::vec2(rect.width() - 2.0, PROXY_STRIP_HEIGHT),
    );
    painter.rect_filled(strip_rect, 2.0, PROXY_COLOR);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reported bug: splitting an audio clip (`SplitClip`) visibly
    /// moved the drawn waveform exactly at the cut
    /// point, because each half rounded its own bins to its own edges.
    /// With `waveform_bin_for_column` computed from the *absolute* position
    /// in the audio time, the same instant must always map to the same
    /// bin both before and after the split.
    #[test]
    fn waveform_bin_for_column_is_continuous_across_a_clip_split() {
        // Realistic values from a real case (bbb_sunflower): video fps and
        // audio duration of the *media* slightly different from the duration of the
        // video, the cause of the rounding the bug exposed.
        let media_fps = 60.0_f64;
        let full_source_out: FrameIdx = 38074;
        let audio_duration_secs = 634.144; // slightly < 38074/60.0
        let num_peaks = 63457;

        let split_at: FrameIdx = 3120; // 52s at 60fps

        for probe_secs in [51.5, 51.9, 52.0, 52.1, 52.5, 53.0] {
            // Bin according to the whole (unsplit) clip: source_in=0,
            // source_out=full_source_out.
            let whole_clip_start = 0.0_f64;
            let whole_clip_end = full_source_out as f64 / media_fps;
            let frac_whole = probe_secs / whole_clip_end;
            let bin_whole = waveform_bin_for_column(
                frac_whole,
                whole_clip_start,
                whole_clip_end,
                audio_duration_secs,
                num_peaks,
            );

            // Same instant, but from the half (left or right) produced by
            // a split at `split_at`.
            let (clip_start_frame, clip_end_frame) = if probe_secs * media_fps < split_at as f64 {
                (0, split_at)
            } else {
                (split_at, full_source_out)
            };
            let clip_start_secs = clip_start_frame as f64 / media_fps;
            let clip_end_secs = clip_end_frame as f64 / media_fps;
            let frac_half = (probe_secs - clip_start_secs) / (clip_end_secs - clip_start_secs);
            let bin_half = waveform_bin_for_column(
                frac_half,
                clip_start_secs,
                clip_end_secs,
                audio_duration_secs,
                num_peaks,
            );

            assert_eq!(
                bin_whole, bin_half,
                "a t={probe_secs}s la clip intera sceglie il bin {bin_whole} ma la metà dopo lo split sceglie {bin_half}: la forma d'onda si sposterebbe al taglio"
            );
        }
    }

    /// It must stay among the first candidates (1-2-5) at very low zoom, and
    /// rise enough to keep the ticks readable even at very
    /// high zoom — not a fixed value whatever `pixels_per_sec` is.
    #[test]
    fn nice_tick_interval_secs_grows_with_zoom_to_keep_ticks_readable() {
        assert_eq!(nice_tick_interval_secs(800.0), 1.0);
        assert_eq!(nice_tick_interval_secs(60.0), 2.0);
        assert_eq!(nice_tick_interval_secs(10.0), 10.0);
        assert_eq!(nice_tick_interval_secs(0.1), 900.0);
    }

    #[test]
    fn format_timecode_includes_frames_and_hours() {
        // 25 fps: 5 seconds = frame 125, shows HH:MM:SS:FF
        assert_eq!(format_timecode(5.0, 25.0), "00:00:05:00");
        // 65 seconds = 1 min 5 sec
        assert_eq!(format_timecode(65.0, 25.0), "00:01:05:00");
        // 3665 seconds = 1h 1m 5s
        assert_eq!(format_timecode(3665.0, 25.0), "01:01:05:00");
        // With a fraction of a second: 0.4s at 25fps = frame 10
        assert_eq!(format_timecode(5.4, 25.0), "00:00:05:10");
        // 29.97: 30 frames per nominal second, not 29.
        assert_eq!(format_timecode(1799.0 / (30_000.0 / 1001.0), 30_000.0 / 1001.0), "00:00:59:29");
    }

    #[test]
    fn format_duration_is_seconds_and_leftover_frames() {
        assert_eq!(format_duration(0, 10.0), "0:00");
        // 10 frames at 10fps = exactly 1s, no remaining frames.
        assert_eq!(format_duration(10, 10.0), "1:00");
        // 14 frames at 10fps = 1s + 4 frames.
        assert_eq!(format_duration(14, 10.0), "1:04");
        assert_eq!(format_duration(93, 30.0), "3:03");
    }

    #[test]
    fn fade_zone_at_only_matches_the_top_band_near_the_handle() {
        let clip_rect = egui::Rect::from_min_size(egui::pos2(100.0, 20.0), egui::vec2(200.0, 40.0));
        let fade_in_x = 120.0; // fade-in handle 20px from the left edge
        let fade_out_x = 280.0; // fade-out handle 20px from the right edge
        assert_eq!(
            fade_zone_at(egui::pos2(120.0, 22.0), clip_rect, fade_in_x, fade_out_x),
            Some(FadeEdge::In)
        );
        assert_eq!(
            fade_zone_at(egui::pos2(280.0, 22.0), clip_rect, fade_in_x, fade_out_x),
            Some(FadeEdge::Out)
        );
        // Same X as the fade-in handle, but below the top band: it is trim/roll, not fade.
        assert_eq!(fade_zone_at(egui::pos2(120.0, 50.0), clip_rect, fade_in_x, fade_out_x), None);
        // Far from both handles.
        assert_eq!(fade_zone_at(egui::pos2(200.0, 22.0), clip_rect, fade_in_x, fade_out_x), None);
    }

    #[test]
    fn fade_drag_value_moves_in_opposite_screen_directions_for_in_and_out() {
        let px_per_frame = 2.0;
        let drag_right = |edge| FadeDragState {
            clip_id: ClipId(1),
            track_index: 0,
            edge,
            original_value: 10,
            accum_px: 20.0, // 20px to the right = 10 frames
        };
        // Fade-in: dragging to the right lengthens the fade.
        assert_eq!(fade_drag_value(&drag_right(FadeEdge::In), 100, px_per_frame), 20);
        // Fade-out: the same movement to the right shortens it (the handle
        // approaches the corner).
        assert_eq!(fade_drag_value(&drag_right(FadeEdge::Out), 100, px_per_frame), 0);
        // Clamped to the duration of the clip.
        let far = FadeDragState {
            clip_id: ClipId(1),
            track_index: 0,
            edge: FadeEdge::In,
            original_value: 10,
            accum_px: 1000.0,
        };
        assert_eq!(fade_drag_value(&far, 30, px_per_frame), 30);
    }

    #[test]
    fn crossing_drag_value_grows_when_the_grabbed_extremity_moves_away_from_the_cut() {
        let px_per_frame = 2.0;
        // Left end dragged further left (away from the
        // cut, towards the inside of the left clip): the crossing
        // lengthens, by twice the frames moved (it grows on both
        // sides together).
        let left_extends = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: true,
            original_duration: 10,
            max_duration: 100,
            accum_px: -20.0, // 20px to the left = 10 frames
        };
        assert_eq!(crossing_drag_value(&left_extends, px_per_frame), 30);
        // Same displacement in pixels but on the right side, to the right:
        // same effect, it moves away from the cut in the opposite direction.
        let right_extends = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: false,
            original_duration: 10,
            max_duration: 100,
            accum_px: 20.0,
        };
        assert_eq!(crossing_drag_value(&right_extends, px_per_frame), 30);
        // Dragging the left end to the right (towards the cut)
        // shortens it, clamped to a minimum of 1 frame.
        let shrinking = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: true,
            original_duration: 10,
            max_duration: 100,
            accum_px: 20.0,
        };
        assert_eq!(crossing_drag_value(&shrinking, px_per_frame), 1);
        // Clamped to the maximum allowed by the two clips involved.
        let far = CrossingDragState {
            track_index: 0,
            left_clip: ClipId(1),
            grabbed_left_side: true,
            original_duration: 10,
            max_duration: 40,
            accum_px: -1000.0,
        };
        assert_eq!(crossing_drag_value(&far, px_per_frame), 40);
    }

    #[test]
    fn gain_offset_is_zero_at_zero_db_and_reaches_the_edges_at_the_range_extremes() {
        assert_eq!(gain_offset(0.0), 0.0);
        assert_eq!(gain_offset(vv_core::GAIN_DB_MAX), 1.0);
        assert_eq!(gain_offset(vv_core::GAIN_DB_MIN), -1.0);
        // Past the extremes it stays clamped, it does not exceed [-1, 1].
        assert_eq!(gain_offset(vv_core::GAIN_DB_MAX + 10.0), 1.0);
        assert_eq!(gain_offset(vv_core::GAIN_DB_MIN - 10.0), -1.0);
    }

    #[test]
    fn gain_from_offset_is_the_inverse_of_gain_offset() {
        for db in [vv_core::GAIN_DB_MIN, -50.0, -6.0, 0.0, 6.0, vv_core::GAIN_DB_MAX] {
            assert!((gain_from_offset(gain_offset(db)) - db).abs() < 1e-4, "db={db}");
        }
    }

    #[test]
    fn volume_drag_value_follows_the_pointer_and_clamps_at_the_range_extremes() {
        let half_height = 18.0; // (ROW_HEIGHT - 4.0) / 2.0
        let group = History::default().begin_group();
        let drag = |accum_px| VolumeDragState {
            clip_id: ClipId(1),
            track_index: 0,
            original_db: 0.0,
            accum_px,
            group,
        };
        // At 0 dB the line is at the center: dragging upwards (negative
        // accum_px) raises the gain, downwards lowers it.
        assert!(volume_drag_value(&drag(-half_height), half_height) > 0.0);
        assert!(volume_drag_value(&drag(half_height), half_height) < 0.0);
        // Past the available run it clamps to the extremes of the range.
        assert_eq!(volume_drag_value(&drag(-half_height * 10.0), half_height), vv_core::GAIN_DB_MAX);
        assert_eq!(volume_drag_value(&drag(half_height * 10.0), half_height), vv_core::GAIN_DB_MIN);
    }

    /// "Identity" `row_y` (no grouping/margin) for the tests.
    fn test_row_y(n: usize) -> Vec<f32> {
        (0..n).map(|i| RULER_HEIGHT + i as f32 * ROW_HEIGHT).collect()
    }

    fn visual(track_index: usize, id: u64, start: FrameIdx, len: FrameIdx) -> ClipVisual<'static> {
        ClipVisual {
            track_index,
            clip: std::borrow::Cow::Owned(Clip::from_source_range(
                ClipId(id),
                vv_core::ClipSource::SolidColor,
                0,
                len,
                start,
                vv_core::Rational::one(),
            )),
            label: String::new(),
            color: egui::Color32::WHITE,
            locked: false,
            muted: false,
        }
    }

    fn visual_linked(
        track_index: usize,
        id: u64,
        start: FrameIdx,
        len: FrameIdx,
        group: u64,
    ) -> ClipVisual<'static> {
        let mut v = visual(track_index, id, start, len);
        v.clip.to_mut().linked_group = Some(vv_core::LinkGroupId(group));
        v
    }

    #[test]
    fn expand_to_linked_groups_includes_the_whole_group() {
        let visuals = vec![visual_linked(0, 1, 0, 10, 100), visual_linked(1, 2, 0, 10, 100)];
        assert_eq!(
            expand_to_linked_groups(&visuals, [(0, ClipId(1))]),
            BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))]),
            "selezionando il video deve espandere anche all'audio collegato"
        );
        assert_eq!(
            expand_to_linked_groups(&visuals, [(1, ClipId(2))]),
            BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))]),
            "e viceversa, partendo dall'audio"
        );
    }

    #[test]
    fn expand_to_linked_groups_is_a_noop_for_unlinked_clips() {
        let visuals = vec![visual(0, 1, 0, 10)];
        assert_eq!(
            expand_to_linked_groups(&visuals, [(0, ClipId(1))]),
            BTreeSet::from([(0, ClipId(1))])
        );
        assert_eq!(expand_to_linked_groups(&visuals, []), BTreeSet::new());
    }

    #[test]
    fn expand_to_linked_groups_handles_independent_groups_and_groups_larger_than_two() {
        // A group of 2 and one of 3, independent, both starting
        // points: the whole group of each must show up in the
        // result, not just one partner.
        let visuals = vec![
            visual_linked(0, 1, 0, 10, 100),
            visual_linked(1, 2, 0, 10, 100),
            visual_linked(0, 3, 20, 10, 200),
            visual_linked(1, 4, 20, 10, 200),
            visual_linked(2, 5, 20, 10, 200),
        ];
        let result = expand_to_linked_groups(&visuals, [(0, ClipId(1)), (0, ClipId(3))]);
        assert_eq!(
            result,
            BTreeSet::from([
                (0, ClipId(1)),
                (1, ClipId(2)),
                (0, ClipId(3)),
                (1, ClipId(4)),
                (2, ClipId(5)),
            ])
        );
    }

    /// Reported bug: with CTRL+click/rubber band I selected 2+ clips *not*
    /// linked to each other, then dragging one the others did not follow —
    /// the drag looked only at the linked group of the clicked clip,
    /// ignoring the rest of the selection.
    #[test]
    fn drag_group_for_follows_the_whole_multi_selection_even_without_a_link() {
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(1, 2, 30, 10),
            visual(2, 3, 60, 10),
        ];
        // 3 clips not linked to each other, all selected by hand (CTRL+click).
        let selected = BTreeSet::from([(0, ClipId(1)), (1, ClipId(2)), (2, ClipId(3))]);

        // Dragging any one of them, the drag must follow the whole
        // selection — not just it.
        assert_eq!(
            drag_group_for(&selected, &visuals, (1, ClipId(2))),
            selected
        );
    }

    #[test]
    fn drag_group_for_replaces_the_selection_when_dragging_an_unselected_clip() {
        let visuals = vec![visual(0, 1, 0, 10), visual(1, 2, 30, 10)];
        // Previous, unrelated selection: dragging a clip outside
        // it must not drag it along.
        let selected = BTreeSet::from([(0, ClipId(1))]);
        assert_eq!(
            drag_group_for(&selected, &visuals, (1, ClipId(2))),
            BTreeSet::from([(1, ClipId(2))])
        );
    }

    #[test]
    fn drag_group_for_expands_to_the_link_group_when_dragging_an_unselected_linked_clip() {
        let visuals = vec![visual_linked(0, 1, 0, 10, 100), visual_linked(1, 2, 0, 10, 100)];
        let selected = BTreeSet::new();
        assert_eq!(
            drag_group_for(&selected, &visuals, (0, ClipId(1))),
            BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))])
        );
    }

    #[test]
    fn apply_click_selection_plain_replaces_selection() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, anchor) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (0, ClipId(2)),
            ClickModifiers::Plain,
            &visuals,
            10.0,
            &test_row_y(2),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(2))]));
        assert_eq!(anchor, Some((0, ClipId(2))));
    }

    #[test]
    fn apply_click_selection_toggle_adds_and_removes() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, _) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (0, ClipId(2)),
            ClickModifiers::Toggle,
            &visuals,
            10.0,
            &test_row_y(2),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(1)), (0, ClipId(2))]));

        // Ctrl+click on an already selected clip removes it.
        let (selected2, _) = apply_click_selection(
            &selected,
            Some((0, ClipId(2))),
            (0, ClipId(1)),
            ClickModifiers::Toggle,
            &visuals,
            10.0,
            &test_row_y(2),
        );
        assert_eq!(selected2, BTreeSet::from([(0, ClipId(2))]));
    }

    #[test]
    fn apply_click_selection_range_selects_bounding_box_from_anchor() {
        // Three clips on the same track: [0,10) [20,30) [40,50). Anchor=1,
        // shift+click on 3 must select the 2 in between too.
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(0, 2, 20, 10),
            visual(0, 3, 40, 10),
        ];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, anchor) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (0, ClipId(3)),
            ClickModifiers::Range,
            &visuals,
            1.0,
            &test_row_y(1),
        );
        assert_eq!(
            selected,
            BTreeSet::from([(0, ClipId(1)), (0, ClipId(2)), (0, ClipId(3))])
        );
        // The anchor does not change with shift+click.
        assert_eq!(anchor, Some((0, ClipId(1))));
    }

    #[test]
    fn apply_click_selection_range_spans_multiple_tracks() {
        let visuals = vec![
            visual(0, 1, 0, 10),  // video, anchor
            visual(1, 2, 0, 10),  // audio, inside the range (same column)
            visual(0, 3, 20, 10), // outside the horizontal range
        ];
        let current = BTreeSet::from([(0, ClipId(1))]);
        let (selected, _) = apply_click_selection(
            &current,
            Some((0, ClipId(1))),
            (1, ClipId(2)),
            ClickModifiers::Range,
            &visuals,
            1.0,
            &test_row_y(2),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(1)), (1, ClipId(2))]));
    }

    #[test]
    fn apply_click_selection_range_without_prior_anchor_uses_clicked_as_anchor() {
        let visuals = vec![visual(0, 1, 0, 10)];
        let current = BTreeSet::new();
        let (selected, anchor) = apply_click_selection(
            &current,
            None,
            (0, ClipId(1)),
            ClickModifiers::Range,
            &visuals,
            1.0,
            &test_row_y(1),
        );
        assert_eq!(selected, BTreeSet::from([(0, ClipId(1))]));
        assert_eq!(anchor, Some((0, ClipId(1))));
    }

    #[test]
    fn clips_intersecting_rect_finds_overlapping_clips_only() {
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(0, 2, 20, 10),
            visual(1, 3, 0, 10),
        ];
        // Rectangle covering only the area of clips 1 and 3 (starting
        // column, both tracks), not 2.
        let row_y = test_row_y(2);
        let rect =
            clip_local_rect(&visuals[0], 1.0, &row_y).union(clip_local_rect(&visuals[2], 1.0, &row_y));
        let hits: BTreeSet<_> = clips_intersecting_rect(&visuals, 1.0, &row_y, rect)
            .into_iter()
            .collect();
        assert_eq!(hits, BTreeSet::from([(0, ClipId(1)), (1, ClipId(3))]));
    }

    #[test]
    fn neighbor_bounds_no_neighbors_is_unbounded() {
        let visuals = vec![visual(0, 1, 10, 5)];
        assert_eq!(
            neighbor_bounds_at(&visuals, 0, &[ClipId(1)], 10, 5),
            (0, FrameIdx::MAX)
        );
    }

    #[test]
    fn neighbor_bounds_clamped_by_prev_and_next_on_same_track() {
        let visuals = vec![
            visual(0, 1, 0, 10),  // ends at 10
            visual(0, 2, 20, 30), // the moving one
            visual(0, 3, 50, 5),  // starts at 50
            visual(1, 4, 15, 3),  // other track: ignored
        ];
        assert_eq!(neighbor_bounds_at(&visuals, 0, &[ClipId(2)], 20, 30), (10, 50));
    }

    #[test]
    fn max_start_in_slot_keeps_clip_inside_slot() {
        // slot [10, 50), clip of length 30: it can only sit between 10 and 20.
        assert_eq!(0.clamp(10, max_start_in_slot(10, 50, 30)), 10);
        assert_eq!(15.clamp(10, max_start_in_slot(10, 50, 30)), 15);
        assert_eq!(100.clamp(10, max_start_in_slot(10, 50, 30)), 20);
    }

    #[test]
    fn max_start_in_slot_degenerate_slot_does_not_invert_range() {
        // slot smaller than the clip: it must not produce an inverted range.
        assert_eq!(max_start_in_slot(10, 15, 30), 10);
    }

    #[test]
    fn drag_range_matches_neighbor_bounds_minus_own_length() {
        let visuals = vec![
            visual(0, 1, 0, 10),  // ends at 10
            visual(0, 2, 20, 30), // length 30: it can sit between 10 and 50-30=20
            visual(0, 3, 50, 5),
        ];
        assert_eq!(drag_range(&visuals, 0, ClipId(2)), (10, 20));
    }

    #[test]
    fn combined_drag_range_with_no_others_matches_plain_drag_range() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 30)];
        let (min, max, followers) = combined_drag_range(&visuals, 0, ClipId(2), &[]);
        assert_eq!((min, max), drag_range(&visuals, 0, ClipId(2)));
        assert!(followers.is_empty());
    }

    #[test]
    fn combined_drag_range_intersects_both_clips_constraints() {
        // Track 0: [0,10) then clip 2 (video, [20,50)).
        // Track 1: its twin (audio, same [20,50)) but with a
        // narrower following neighbor: it ends at 55 instead of free.
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(0, 2, 20, 30), // video, linked to 3
            visual(1, 3, 20, 30), // audio, linked to 2
            visual(1, 4, 55, 5),  // constrains the audio twin to stay <= 55-30=25
        ];
        // On its own track 0 would allow [10, MAX-30]; the twin on
        // track 1 narrows it to max_start <= 25 (same offset, 0).
        let (min, max, followers) = combined_drag_range(&visuals, 0, ClipId(2), &[(1, ClipId(3))]);
        assert_eq!(min, 10);
        assert_eq!(max, 25);
        assert_eq!(followers, vec![(ClipId(3), 1, 0)]);
    }

    #[test]
    fn combined_drag_range_respects_nonzero_offset_between_linked_clips() {
        // The twin is not aligned: it starts 5 frames after the primary.
        let visuals = vec![
            visual(0, 1, 10, 20), // primary, track 0, start=10
            visual(1, 2, 15, 20), // twin, track 1, start=15 (offset=5)
            visual(1, 3, 60, 5),  // constrains the twin: max_start <= 60-20=40
        ];
        let (_, max, followers) = combined_drag_range(&visuals, 0, ClipId(1), &[(1, ClipId(2))]);
        // twin constraint translated: primary.max_start <= 40 - offset(5) = 35
        assert_eq!(max, 35);
        assert_eq!(followers, vec![(ClipId(2), 1, 5)]);
    }

    #[test]
    fn combined_drag_range_intersects_a_group_of_three() {
        // A group of 3 on 3 different tracks, each with a different constraint.
        let visuals = vec![
            visual(0, 1, 10, 10), // primary, track 0, start=10, no neighbor
            visual(1, 2, 10, 10), // same start, constrained by a neighbor at 25
            visual(1, 5, 35, 5),
            visual(2, 3, 10, 10), // same start, constrained by a neighbor at 22
            visual(2, 6, 32, 5),
        ];
        let (min, max, followers) =
            combined_drag_range(&visuals, 0, ClipId(1), &[(1, ClipId(2)), (2, ClipId(3))]);
        assert_eq!(min, 0);
        // track1: max_start <= 35-10=25; track2: max_start <= 32-10=22 (tighter)
        assert_eq!(max, 22);
        assert_eq!(
            followers,
            vec![(ClipId(2), 1, 0), (ClipId(3), 2, 0)],
            "entrambe le altre clip del gruppo, con offset 0 (stesso start)"
        );
    }

    #[test]
    fn drag_range_at_checks_neighbors_on_a_track_the_clip_isnt_on() {
        let visuals = vec![visual(0, 1, 0, 10), visual(1, 2, 20, 10)];
        // Clip 1 evaluated as if it were about to land on track 1: it must
        // respect the neighbor there (clip 2), not those of its real track.
        assert_eq!(drag_range_at(&visuals, 1, &[ClipId(1)], 5, 10), (0, 10));
    }

    #[test]
    fn group_drag_bounds_intersects_primary_and_followers_on_their_own_targets() {
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(1, 2, 0, 10),
            visual(1, 3, 30, 10),
        ];
        let targets = vec![
            (ClipId(1), EffectiveTrack::Existing(0)),
            (ClipId(2), EffectiveTrack::Existing(1)),
        ];
        let followers = vec![(ClipId(2), 1, -5)];
        let (min_start, max_start) = group_drag_bounds(&visuals, 0, &targets, &followers);
        assert_eq!((min_start, max_start), (5, 25));
    }

    #[test]
    fn group_drag_bounds_lets_group_members_land_on_the_same_track_without_blocking_each_other() {
        // Two clips of the group (1 and 2) both land on track 1: they must
        // not block each other, only the outside clip (9) counts as a
        // neighbor.
        let visuals = vec![
            visual(0, 1, 0, 10),
            visual(2, 2, 5, 10),
            visual(1, 9, 50, 5),
        ];
        let targets = vec![
            (ClipId(1), EffectiveTrack::Existing(1)),
            (ClipId(2), EffectiveTrack::Existing(1)),
        ];
        let followers = vec![(ClipId(2), 2, 5)];
        let (min_start, max_start) = group_drag_bounds(&visuals, 0, &targets, &followers);
        assert_eq!((min_start, max_start), (0, 35));
    }

    #[test]
    fn dragging_from_the_middle_of_a_group_onto_an_empty_track_does_not_overflow() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10), visual(0, 3, 40, 10)];
        let targets = vec![
            (ClipId(2), EffectiveTrack::Existing(1)),
            (ClipId(1), EffectiveTrack::Existing(1)),
            (ClipId(3), EffectiveTrack::New(1)),
        ];
        let followers = vec![(ClipId(1), 0, -20), (ClipId(3), 0, 20)];
        let (min_start, max_start) = group_drag_bounds(&visuals, 20, &targets, &followers);
        assert_eq!(min_start, 20);
        assert!(max_start > 1_000_000);

        let (min_start, _, _) = combined_drag_range(&visuals, 0, ClipId(2), &[(0, ClipId(1)), (0, ClipId(3))]);
        assert_eq!(min_start, 20);
    }

    /// 2 video tracks and 1 audio; 228px below the ruler = 50px of empty
    /// zone above and below the groups, 0 = no empty zone.
    fn test_layout(slack: f32) -> PaneLayout {
        let avail = 2.0 * slack + 3.0 * ROW_HEIGHT + GROUP_DIVIDER_HEIGHT;
        PaneLayout::new(avail, 2, 1, &mut TimelineState::default())
    }

    #[test]
    fn track_drag_target_above_video_group_is_new_track_when_margin_exists() {
        let row_order = [1, 0, 2]; // 2 video tracks (descending), 1 audio
        let target = track_drag_target(40.0, TrackKind::Video, &row_order, &test_layout(50.0));
        assert!(matches!(target, Some(TrackDragTarget::NewTrack)));
    }

    #[test]
    fn track_drag_target_above_video_group_without_margin_is_the_top_track() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(15.0, TrackKind::Video, &row_order, &test_layout(0.0));
        assert!(matches!(target, Some(TrackDragTarget::Track(1))));
    }

    #[test]
    fn track_drag_target_lands_on_the_right_video_row() {
        let row_order = [1, 0, 2];
        let layout = test_layout(50.0);
        let first_row = track_drag_target(80.0, TrackKind::Video, &row_order, &layout);
        assert!(matches!(first_row, Some(TrackDragTarget::Track(1))));
        let second_row = track_drag_target(120.0, TrackKind::Video, &row_order, &layout);
        assert!(matches!(second_row, Some(TrackDragTarget::Track(0))));
    }

    #[test]
    fn track_drag_target_video_over_audio_group_is_none() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(170.0, TrackKind::Video, &row_order, &test_layout(50.0));
        assert!(target.is_none());
    }

    /// An effect (always video) or a media with video over a free
    /// video track land exactly there, not on the first free one —
    /// the very same criterion, regardless of which of the two
    /// is being dragged.
    #[test]
    fn media_pool_drop_target_lands_on_the_hovered_video_track_for_a_generator_or_a_video_media() {
        for has_video in [true, false] {
            let target = media_pool_drop_target(2, has_video, TrackKind::Video, false);
            if has_video {
                assert_eq!(target, Some(MediaDropTarget::Track(2)));
            } else {
                // A media without video (audio-only) on a video track
                // does not force that track: it falls back on the usual
                // resolution, as it already did.
                assert_eq!(target, Some(MediaDropTarget::Default));
            }
        }
    }

    #[test]
    fn media_pool_drop_target_over_an_audio_track_falls_back_to_default() {
        assert_eq!(
            media_pool_drop_target(0, true, TrackKind::Audio, false),
            Some(MediaDropTarget::Default)
        );
    }

    #[test]
    fn media_pool_drop_target_over_a_locked_track_refuses_the_drop() {
        assert_eq!(media_pool_drop_target(2, true, TrackKind::Video, true), None);
    }

    #[test]
    fn track_drag_target_below_audio_group_is_new_track_when_margin_exists() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(200.0, TrackKind::Audio, &row_order, &test_layout(50.0));
        assert!(matches!(target, Some(TrackDragTarget::NewTrack)));
    }

    #[test]
    fn track_drag_target_below_audio_group_without_margin_is_the_bottom_track() {
        let row_order = [1, 0, 2];
        let target = track_drag_target(200.0, TrackKind::Audio, &row_order, &test_layout(0.0));
        assert!(matches!(target, Some(TrackDragTarget::Track(2))));
    }

    /// Reported bug: with many video tracks the separator did not rise past
    /// the topmost track, so room could not be made for the audio.
    #[test]
    fn divider_can_shrink_an_overflowing_video_pane_which_then_scrolls() {
        let mut state = TimelineState::default();
        let avail = 200.0;
        let unconstrained = PaneLayout::new(avail, 6, 1, &mut state);
        assert_eq!(unconstrained.video_height_range.min, MIN_PANE_HEIGHT);

        state.video_pane_height = Some(60.0);
        let layout = PaneLayout::new(avail, 6, 1, &mut state);
        assert_eq!(layout.video_height(), 60.0);
        // Resting against the separator until it scrolls.
        assert_eq!(layout.video_rows_bottom(), layout.video_pane.max);
        assert_eq!(layout.video_max_scroll, 6.0 * ROW_HEIGHT + NEW_TRACK_ZONE_HEIGHT - 60.0);

        state.video_scroll = 10_000.0;
        let scrolled = PaneLayout::new(avail, 6, 1, &mut state);
        assert_eq!(state.video_scroll, layout.video_max_scroll);
        assert_eq!(scrolled.video_rows_top, scrolled.video_pane.min + NEW_TRACK_ZONE_HEIGHT);
    }

    #[test]
    fn audio_pane_scrolls_when_its_tracks_overflow() {
        let mut state = TimelineState::default();
        state.audio_scroll = 10_000.0;
        let layout = PaneLayout::new(200.0, 1, 8, &mut state);
        assert!(layout.audio_max_scroll > 0.0);
        assert_eq!(
            layout.audio_rows_bottom() + NEW_TRACK_ZONE_HEIGHT,
            layout.audio_pane.max
        );
        let last_row = layout.row_at_y(layout.audio_pane.max - NEW_TRACK_ZONE_HEIGHT - 1.0);
        assert_eq!(last_row, 8);
    }

    #[test]
    fn drag_group_row_targets_shifts_a_same_kind_follower_by_the_same_amount() {
        // track_kinds: [Video, Audio, Video, Video] -> row_order [3,2,0,1]
        // (video descending, audio ascending), row_of_track [2,3,1,0].
        let track_kinds = [TrackKind::Video, TrackKind::Audio, TrackKind::Video, TrackKind::Video];
        let row_of_track = [2, 3, 1, 0];
        let row_order = [3, 2, 0, 1];
        // Primary (track 2, row 1) rises one row -> track 3 (row 0).
        // Video follower (track 0, row 2, "below" the primary) must
        // snap into the row just freed by the primary (row
        // 1 -> track 2), exactly as C1 follows C2 in the user's
        // example.
        let followers = vec![(ClipId(9), 0, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            2,
            EffectiveTrack::Existing(3),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            3,
            4,
        );
        assert_eq!(
            targets,
            vec![
                (ClipId(1), EffectiveTrack::Existing(3)),
                (ClipId(9), EffectiveTrack::Existing(2)),
            ]
        );
    }

    #[test]
    fn drag_group_row_targets_moves_an_audio_follower_in_the_opposite_row_direction() {
        // track_kinds: [Video, Audio, Audio, Video] -> row_order [3,0,1,2]
        // (2 video tracks, 2 audio), row_of_track [1,2,3,0].
        let track_kinds = [TrackKind::Video, TrackKind::Audio, TrackKind::Audio, TrackKind::Video];
        let row_of_track = [1, 2, 3, 0];
        let row_order = [3, 0, 1, 2];
        // Video primary (track 0, row 1) rises one row -> track 3
        // (row 0). The audio follower (track 1, row 2) must go down
        // one row (track 1 -> track 2), not up: video and audio
        // number the tracks in opposite directions.
        let followers = vec![(ClipId(9), 1, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            0,
            EffectiveTrack::Existing(3),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            2,
            4,
        );
        assert_eq!(
            targets,
            vec![
                (ClipId(1), EffectiveTrack::Existing(3)),
                (ClipId(9), EffectiveTrack::Existing(2)),
            ]
        );
    }

    #[test]
    fn drag_group_row_targets_creates_a_new_track_for_a_follower_that_would_overflow() {
        let track_kinds = [TrackKind::Video, TrackKind::Audio, TrackKind::Video, TrackKind::Video];
        let row_of_track = [2, 3, 1, 0];
        let row_order = [3, 2, 0, 1];
        // Single audio track: the audio follower has nowhere to go among
        // the existing ones, so (reported bug) it must ask for a new
        // track instead of staying stuck on its own — exactly as
        // it would if it were the grabbed clip.
        let followers = vec![(ClipId(9), 1, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            2,
            EffectiveTrack::Existing(3),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            3,
            4,
        );
        assert_eq!(targets[1], (ClipId(9), EffectiveTrack::New(1)));
    }

    #[test]
    fn drag_group_row_targets_can_need_more_than_one_new_track_for_a_follower() {
        // The primary is not the nearest to the edge of its own group: if
        // it jumps directly into a new track, the follower that was already
        // at the edge must "break through" by more than one track to keep
        // the relative spacing.
        let track_kinds = [TrackKind::Video, TrackKind::Video, TrackKind::Video];
        let row_of_track = [2, 1, 0]; // 3 video tracks, descending rows
        let row_order = [2, 1, 0];
        // Primary on track 0 (row 2, the farthest from the edge) goes to
        // New(1); the follower on track 2 (row 0, already at the edge) follows
        // with the same delta (-3) -> New(3).
        let followers = vec![(ClipId(9), 2, 0)];
        let targets = drag_group_row_targets(
            ClipId(1),
            0,
            EffectiveTrack::New(1),
            &followers,
            &track_kinds,
            &row_of_track,
            &row_order,
            3,
            3,
        );
        assert_eq!(targets[1], (ClipId(9), EffectiveTrack::New(3)));
    }

    fn media_clip_visual(
        track_index: usize,
        id: u64,
        start: FrameIdx,
        source_in: FrameIdx,
        source_out: FrameIdx,
        media_id: vv_core::MediaId,
    ) -> ClipVisual<'static> {
        ClipVisual {
            track_index,
            clip: std::borrow::Cow::Owned(Clip::from_source_range(
                ClipId(id),
                ClipSource::Media(media_id),
                source_in,
                source_out,
                start,
                vv_core::Rational::one(),
            )),
            label: String::new(),
            color: egui::Color32::WHITE,
            locked: false,
            muted: false,
        }
    }

    fn project_with_media(duration_frames: FrameIdx) -> (Project, vv_core::MediaId) {
        let mut project = Project::default();
        let media_id = project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/x.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames,
                fps: vv_core::Rational::new(25, 1),
                width: 100,
                height: 100,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        (project, media_id)
    }

    #[test]
    fn drag_set_segments_are_queued_one_after_the_other() {
        let (mut project, a) = project_with_media(30);
        let b = project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/y.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 50,
                fps: vv_core::Rational::new(25, 1),
                width: 100,
                height: 100,
                has_video: true,
                has_audio: true,
                sample_rate: 48000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 1,
            compound: None,
        });
        let fps = vv_core::Rational::new(25, 1);
        let set = MediaDragSet {
            items: vec![
                MediaDrag::whole(a, &project.media_pool[a].meta),
                MediaDrag::whole(b, &project.media_pool[b].meta),
            ],
        };
        let segments = drag_set_segments(&project, fps, &TimelineDrag::Media(set));
        assert_eq!(
            segments
                .iter()
                .map(|s| (s.offset, s.len, s.has_audio))
                .collect::<Vec<_>>(),
            vec![(0, 30, false), (30, 50, true)]
        );
    }

    /// The neighbor no longer limits the trim: lengthening over it
    /// overwrites it (see `PendingAction::Trim`), so the only limit
    /// left is the start of the source.
    #[test]
    fn single_trim_range_start_is_not_clamped_by_the_previous_neighbor() {
        let project = Project::default();
        let visuals = vec![
            visual(0, 1, 0, 5), // ends at 5
            media_clip_visual(0, 2, 10, 8, 20, vv_core::MediaId::default()),
        ];
        let (min_value, _) = single_trim_range(&project, &visuals[1].clip, TrimEdge::Start);
        assert_eq!(min_value, 2, "10 - source_in 8, non il bordo del vicino");
    }

    #[test]
    fn single_trim_range_start_is_clamped_by_source_in() {
        let project = Project::default();
        // No neighbor, but source_in=3: one cannot go back past
        // the start of the source, so timeline_start cannot go
        // below 10-3=7.
        let visuals = vec![media_clip_visual(
            0,
            1,
            10,
            3,
            20,
            vv_core::MediaId::default(),
        )];
        let (min_value, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::Start);
        assert_eq!(min_value, 7);
        assert_eq!(
            max_value, 26,
            "timeline_end() - 1 (timeline_end = 10 + (20-3) = 27)"
        );
    }

    #[test]
    fn single_trim_range_end_is_not_clamped_by_the_next_neighbor() {
        let project = Project::default();
        let visuals = vec![
            visual(0, 1, 0, 10),  // being trimmed: [0,10)
            visual(0, 2, 15, 10), // following neighbor: it can be overwritten
        ];
        let (_, max_value) = single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(max_value, FrameIdx::MAX);
    }

    #[test]
    fn single_trim_range_end_is_clamped_by_media_duration() {
        let (project, media_id) = project_with_media(25);
        // source_out starts at 20 on a media 25 frames long: the end cannot
        // be extended past timeline_start + (25 - source_in) = 25.
        let visuals = vec![media_clip_visual(0, 1, 0, 0, 20, media_id)];
        let (_, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(max_value, 25);
    }

    /// Conformed clip (media at 29.97 on a timeline at 30): the trim limits
    /// are in *timeline* frames, so the duration of the source must be
    /// converted with the `rate` — 1000 source frames are 1001 timeline ones.
    /// With the previous 1:1 conversion it would give 1000 and 5000.
    #[test]
    fn single_trim_range_of_a_conformed_clip_is_in_timeline_frames() {
        let (project, media_id) = project_with_media(4000);
        let mut visuals = vec![media_clip_visual(0, 1, 3000, 2000, 2400, media_id)];
        visuals[0].clip = std::borrow::Cow::Owned(Clip::from_source_range(
            visuals[0].clip.id,
            visuals[0].clip.source.clone(),
            2000,
            2400,
            3000,
            vv_core::Rational::conform_rate(
                vv_core::Rational::new(30, 1),
                vv_core::Rational::new(30_000, 1001),
            ),
        ));

        let (min_value, _) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::Start);
        assert_eq!(
            min_value, 998,
            "2000 frame sorgente prima = 2002 di timeline prima di 3000"
        );

        let (_, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(
            max_value, 5002,
            "2000 frame sorgente residui = 2002 frame di timeline dopo 3000"
        );
    }

    #[test]
    fn single_trim_range_end_is_unbounded_for_solid_color() {
        let project = Project::default();
        let visuals = vec![visual(0, 1, 0, 10)];
        let (_, max_value) =
            single_trim_range(&project, &visuals[0].clip, TrimEdge::End);
        assert_eq!(max_value, FrameIdx::MAX);
    }

    #[test]
    fn combined_trim_range_intersects_both_clips_constraints() {
        // Video [10,30) generator (unlimited end), linked to the audio
        // [10,30) on track 1, whose media ends at source frame 25:
        // the twin's limit applies to the video too.
        let (project, media_id) = project_with_media(25);
        let group = Some(vv_core::LinkGroupId(9));
        let mut video = visual(0, 1, 10, 20);
        video.clip.to_mut().linked_group = group;
        let mut audio = media_clip_visual(1, 2, 10, 0, 20, media_id);
        audio.clip.to_mut().linked_group = group;
        let visuals = vec![video, audio];

        let (_, max_value, followers) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(1)),
            TrimEdge::End,
            &[((1, ClipId(2)), TrimEdge::End)],
        );
        assert_eq!(
            max_value, 35,
            "vincolo della gemella si applica anche al video"
        );
        assert_eq!(followers, vec![(ClipId(2), 1, 0, TrimEdge::End)]);
    }

    #[test]
    fn combined_trim_range_shifts_each_selected_clip_by_its_offset() {
        // End of the primary at 10, of the other (track 1) at 25: same
        // delta for both, and the other cannot go below 21.
        let project = Project::default();
        let visuals = vec![visual(0, 1, 0, 10), visual(1, 2, 20, 5)];
        let (min_value, max_value, followers) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(1)),
            TrimEdge::End,
            &[((1, ClipId(2)), TrimEdge::End)],
        );
        assert_eq!(followers, vec![(ClipId(2), 1, 15, TrimEdge::End)]);
        assert_eq!(min_value, 6, "21 - 15");
        assert_eq!(max_value, FrameIdx::MAX - 15);
    }

    #[test]
    fn combined_trim_range_stops_before_another_trimmed_clip_on_the_same_track() {
        let project = Project::default();
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 5)];
        let (_, max_value, _) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(1)),
            TrimEdge::End,
            &[((0, ClipId(2)), TrimEdge::End)],
        );
        assert_eq!(max_value, 20);
    }

    #[test]
    fn combined_trim_range_rolls_between_two_adjacent_clips() {
        // [0,10) and [10,25) in contact, the second with 5 frames of source
        // before its start: the contact point goes from 5 to 24 (the
        // second stays at least 1 long).
        let project = Project::default();
        let mut second = visual(0, 2, 10, 15);
        second.clip = std::borrow::Cow::Owned(Clip::from_source_range(
            ClipId(2),
            vv_core::ClipSource::SolidColor,
            5,
            20,
            10,
            vv_core::Rational::one(),
        ));
        let visuals = vec![visual(0, 1, 0, 10), second];
        let (min_value, max_value, followers) = combined_trim_range(
            &visuals,
            &project,
            (0, ClipId(2)),
            TrimEdge::Start,
            &[((0, ClipId(1)), TrimEdge::End)],
        );
        assert_eq!((min_value, max_value), (5, 24));
        assert_eq!(followers, vec![(ClipId(1), 0, 0, TrimEdge::End)]);
    }

    #[test]
    fn edge_zones_roll_at_the_contact_point_and_trim_just_inside() {
        let neighbor = Some((0, ClipId(9)));
        let zones = edge_zones(100.0, None, neighbor);
        assert_eq!(zones.at(2.0), Some(EdgeZone::Trim(TrimEdge::Start)));
        assert_eq!(zones.at(50.0), None);
        assert_eq!(zones.at(90.0), Some(EdgeZone::Trim(TrimEdge::End)));
        assert_eq!(
            zones.at(98.0),
            Some(EdgeZone::Roll { edge: TrimEdge::End, neighbor: (0, ClipId(9)) })
        );
    }

    /// Reported bug: with snapping on, the edge stopped one frame before
    /// or after the playhead, which was not a snap point.
    #[test]
    fn snap_frame_of_an_edge_snaps_to_the_playhead() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        assert_eq!(snap_frame(14, 0, &visuals, &[ClipId(1)], &[15], 5.0, true), 15);
        assert_eq!(snap_frame(4, 10, &visuals, &[ClipId(1)], &[15], 5.0, true), 5);
    }

    #[test]
    fn snap_frame_of_an_edge_snaps_the_trimmed_edge_to_a_nearby_clip_edge() {
        // Neighboring clip [20,30): the edge dragged to 18, within the threshold
        // (10px / 5px per frame = 2 frames), snaps to 20.
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        assert_eq!(snap_frame(18, 0, &visuals, &[ClipId(1)], &[], 5.0, true), 20);
    }

    #[test]
    fn snap_frame_of_an_edge_ignores_the_clip_being_trimmed_and_far_edges() {
        let visuals = vec![visual(0, 1, 0, 10), visual(0, 2, 20, 10)];
        // Its own edge (10) is not a valid snap.
        assert_eq!(snap_frame(11, 0, &visuals, &[ClipId(1)], &[], 5.0, true), 11);
        // Outside the threshold: no snap.
        assert_eq!(snap_frame(15, 0, &visuals, &[ClipId(1)], &[], 5.0, true), 15);
        // Snapping off: no snap even within the threshold.
        assert_eq!(snap_frame(18, 0, &visuals, &[ClipId(1)], &[], 5.0, false), 18);
    }

    #[test]
    fn snap_frame_snaps_start_to_nearby_clip_end() {
        // Existing clip [0,10): its end edge is 10. A candidate at
        // 12 (within the threshold) must snap exactly there.
        let visuals = vec![visual(0, 1, 0, 10)];
        let px_per_frame = 5.0; // threshold 10px / 5px_per_frame = 2 frames
        let snapped = snap_frame(12, 20, &visuals, &[], &[], px_per_frame, true);
        assert_eq!(snapped, 10);
    }

    #[test]
    fn snap_frame_snaps_end_of_dragged_clip_to_nearby_clip_start() {
        // Existing clip [50,60): the dragged clip (length 20) must
        // snap its own *end* to 50, i.e. candidate_start=30.
        let visuals = vec![visual(0, 1, 50, 10)];
        let snapped = snap_frame(32, 20, &visuals, &[], &[], 5.0, true);
        assert_eq!(snapped, 30);
    }

    #[test]
    fn snap_frame_ignores_clips_beyond_threshold() {
        let visuals = vec![visual(0, 1, 0, 10)];
        // 20 frames away from the edge (10): at px_per_frame=5.0 the threshold
        // is only 2 frames, so it stays unchanged.
        let snapped = snap_frame(30, 5, &visuals, &[], &[], 5.0, true);
        assert_eq!(snapped, 30);
    }

    #[test]
    fn snap_frame_disabled_is_a_no_op() {
        let visuals = vec![visual(0, 1, 0, 10)];
        let snapped = snap_frame(12, 20, &visuals, &[], &[], 5.0, false);
        assert_eq!(snapped, 12);
    }

    #[test]
    fn snap_frame_excludes_given_clip_ids() {
        // Clip 1 would be a valid snap, but it is excluded (it is the clip
        // being dragged itself, or its linked twin).
        let visuals = vec![visual(0, 1, 0, 10)];
        let snapped = snap_frame(12, 20, &visuals, &[ClipId(1)], &[], 5.0, true);
        assert_eq!(snapped, 12);
    }

    #[test]
    fn duplicate_clips_keeps_the_originals_relinks_the_copies_and_cuts_what_they_cover() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();
        let solid = |id, start, len| {
            Clip::from_source_range(
                id,
                vv_core::ClipSource::SolidColor,
                0,
                len,
                start,
                vv_core::Rational::one(),
            )
        };
        // Video [0,20) linked to the audio [0,20); further along on the video
        // another clip [30,60) that the copy will partly cover.
        let (v, a, other) = (project.alloc_clip_id(), project.alloc_clip_id(), project.alloc_clip_id());
        for (track_index, clip) in [(0, solid(v, 0, 20)), (1, solid(a, 0, 20)), (0, solid(other, 30, 30))] {
            history.do_command(
                &mut project,
                Box::new(vv_core::InsertClip { timeline: timeline_id, track_index, clip }),
            );
        }
        history.do_command(
            &mut project,
            Box::new(vv_core::LinkClips::new(timeline_id, vec![(0, v), (1, a)])),
        );

        let mut state = TimelineState::default();
        duplicate_clips(
            &mut project,
            &mut history,
            &mut state,
            timeline_id,
            &[(v, 0, 0, 25), (a, 1, 1, 25)],
        );

        let tl = &project.timelines[timeline_id];
        let spans = |track: usize| -> Vec<(FrameIdx, FrameIdx)> {
            tl.tracks[track].clips.iter().map(|c| (c.timeline_start, c.timeline_end())).collect()
        };
        assert_eq!(spans(0), vec![(0, 20), (25, 45), (45, 60)], "l'altra clip viene tagliata");
        assert_eq!(spans(1), vec![(0, 20), (25, 45)]);
        let copy_v = &tl.tracks[0].clips[1];
        let copy_a = &tl.tracks[1].clips[1];
        assert!(copy_v.linked_group.is_some());
        assert_eq!(copy_v.linked_group, copy_a.linked_group);
        assert_ne!(copy_v.linked_group, tl.tracks[0].clips[0].linked_group);
        assert_eq!(
            state.selected,
            BTreeSet::from([(0, copy_v.id), (1, copy_a.id)])
        );

        history.undo(&mut project);
        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2, "un solo passo di undo");
    }

    #[test]
    fn make_compound_clip_replaces_the_selection_and_names_it_in_order() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        let mut history = History::default();
        let solid = |id, start, len| {
            Clip::from_source_range(id, vv_core::ClipSource::SolidColor, 0, len, start, vv_core::Rational::one())
        };
        let (v, a) = (project.alloc_clip_id(), project.alloc_clip_id());
        for (track_index, clip) in [(0, solid(v, 10, 30)), (1, solid(a, 20, 30))] {
            history.do_command(
                &mut project,
                Box::new(vv_core::InsertClip { timeline: timeline_id, track_index, clip }),
            );
        }

        make_compound_clip(&mut project, &mut history, timeline_id, vec![(0, v), (1, a)]);

        let video_track = &project.timelines[timeline_id].tracks[0];
        assert_eq!(video_track.clips.len(), 1, "le due originali diventano una sola clip video");
        let vv_core::ClipSource::Media(media_id) = video_track.clips[0].source else {
            panic!("la compound clip risultante deve puntare al media pool");
        };
        assert_eq!(video_track.clips[0].timeline_start, 10);
        assert_eq!(video_track.clips[0].timeline_len, 40);
        let audio_track = &project.timelines[timeline_id].tracks[1];
        assert_eq!(audio_track.clips.len(), 1);
        assert_eq!(video_track.clips[0].linked_group, audio_track.clips[0].linked_group);

        let item = project.media_pool.get(media_id).expect("inserita nel pool");
        assert_eq!(item.path.to_str(), Some("Compound Clip 1"));
        assert!(item.meta.has_video && item.meta.has_audio);
        let nested_id = item.compound.expect("è una compound clip");
        let nested = &project.timelines[nested_id];
        assert_eq!(nested.tracks[0].clips[0].timeline_start, 0, "riofsettata sull'inizio della selezione");
        assert_eq!(nested.tracks[1].clips[0].timeline_start, 10);

        // An undo gives back the original clips, but the pool (like an
        // import) does not go back.
        history.undo(&mut project);
        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 1);
        assert_eq!(project.timelines[timeline_id].tracks[0].clips[0].id, v);
        assert_eq!(project.media_pool.len(), 1);

        // A second compound clip continues the numbering.
        let (v2, a2) = (project.alloc_clip_id(), project.alloc_clip_id());
        for (track_index, clip) in [(0, solid(v2, 100, 10)), (1, solid(a2, 100, 10))] {
            history.do_command(
                &mut project,
                Box::new(vv_core::InsertClip { timeline: timeline_id, track_index, clip }),
            );
        }
        make_compound_clip(&mut project, &mut history, timeline_id, vec![(0, v2), (1, a2)]);
        let second = project
            .media_pool
            .values()
            .find(|m| m.path.to_str() != Some("Compound Clip 1"))
            .unwrap();
        assert_eq!(second.path.to_str(), Some("Compound Clip 2"));
    }

    /// Really runs `show_timeline` inside a headless `egui::Context`,
    /// with real clips on several tracks: it catches panics/bugs in the
    /// drawing code (indices, borrows) that the purely logical tests
    /// above do not touch.
    #[test]
    fn show_timeline_renders_without_panicking_with_real_clips() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();

        for (track_index, start, len) in [(0usize, 0i64, 50i64), (0, 50, 30), (1, 0, 50)] {
            let clip = Clip::from_source_range(
                project.alloc_clip_id(),
                vv_core::ClipSource::SolidColor,
                0,
                len,
                start,
                vv_core::Rational::one(),
            );
            history.do_command(
                &mut project,
                Box::new(vv_core::InsertClip {
                    timeline: timeline_id,
                    track_index,
                    clip,
                }),
            );
        }

        let mut state = TimelineState {
            selected: BTreeSet::from([(0, ClipId(0))]),
            ..TimelineState::default()
        };

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                show_timeline(
                    ui,
                    &mut project,
                    &mut history,
                    timeline_id,
                    &|_id| "media".to_string(),
                    &mut state,
                    true,
                    true,
                    &[],
                    &[],
                    &std::collections::HashMap::new(),
                    false,
                );
            });
        });
        // The font atlas generates a texture delta: it must be consumed explicitly
        // or egui panics on drop (diagnostics meant for a real renderer).
        output.textures_delta.clear();

        assert_eq!(project.timelines[timeline_id].tracks[0].clips.len(), 2);
    }

    /// Runs `show_timeline` with more than one video track (REFACTOR_PIPELINE.md
    /// B4): the header column (labels + add/remove track buttons,
    /// `draw_track_headers`) must hold any N tracks, not
    /// only the fixed video/audio pair of before.
    #[test]
    fn show_timeline_renders_without_panicking_with_more_than_two_tracks() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();
        let mut state = TimelineState::default();

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                show_timeline(
                    ui,
                    &mut project,
                    &mut history,
                    timeline_id,
                    &|_id| "media".to_string(),
                    &mut state,
                    true,
                    true,
                    &[],
                    &[],
                    &std::collections::HashMap::new(),
                    false,
                );
            });
        });
        output.textures_delta.clear();

        assert_eq!(project.timelines[timeline_id].tracks.len(), 3);
    }

    /// Bug: the zoom "grew" from the *visible* left edge (or from 0) instead
    /// of from the playhead. Fix: when `pixels_per_sec` changes,
    /// `show_timeline` corrects the horizontal scroll offset so the
    /// playhead stays at the same position on screen — `offset' = offset +
    /// t_playhead * (pps' - pps)` — and a repeated zoom in/out does not move it.
    #[test]
    fn zoom_keeps_playhead_at_same_screen_position() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                vv_core::Track::new(TrackKind::Video),
                vv_core::Track::new(TrackKind::Audio),
            ],
        });
        let mut history = History::default();
        // Clip long enough to make the timeline scrollable (content
        // wider than the viewport): without it the offset would be clamped to 0 and the
        // test would say nothing.
        let clip = Clip::from_source_range(
            project.alloc_clip_id(),
            vv_core::ClipSource::SolidColor,
            0,
            2500, // 100s at 25fps
            0,
            vv_core::Rational::one(),
        );
        history.do_command(
            &mut project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip,
            }),
        );

        let mut state = TimelineState::default();
        state.playhead = 125; // t = 5s at 25fps

        let ctx = egui::Context::default();
        // Realistic viewport (800x600): with `RawInput::default()` the
        // headless viewport is enormous (10000x10000) and the content would
        // not be scrollable — the offset would be clamped to 0 and the test
        // would say nothing.
        let frame_input = || {
            let mut input = egui::RawInput::default();
            input.screen_rect = Some(egui::Rect::from_min_max(
                egui::pos2(0.0, 0.0),
                egui::pos2(800.0, 600.0),
            ));
            input
        };
        let mut render_frame = |state: &mut TimelineState| {
            let mut output = ctx.run_ui(frame_input(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    show_timeline(
                        ui,
                        &mut project,
                        &mut history,
                        timeline_id,
                        &|_id| "media".to_string(),
                        state,
                        true,
                        true,
                        &[],
                        &[],
                        &std::collections::HashMap::new(),
                        false,
                    );
                });
            });
            output.textures_delta.clear();
        };

        // Frame 1: initializes the state of the ScrollArea.
        render_frame(&mut state);
        let scroll_id = {
            // Same id `show_timeline` uses for its ScrollArea:
            // `make_persistent_id` uses the *stable* id of the Ui (not the
            // auto-id counter), so it is enough to replicate the same
            // nesting structure — CentralPanel -> `horizontal_top`,
            // where inside `show_timeline` the ScrollArea lives. Careful to
            // use the `IdSalt` form and not the string: `Id::with(IdSalt)` and
            // `Id::with(&str)` give different ids for the same string.
            let mut captured = None;
            let mut output = ctx.run_ui(frame_input(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        captured = Some(ui.make_persistent_id(egui::IdSalt::new("timeline_scroll")));
                    });
                });
            });
            output.textures_delta.clear();
            captured.expect("id della ScrollArea")
        };

        // Simulates the user having already scrolled: offset 30px.
        {
            let mut st = egui::containers::scroll_area::State::load(&ctx, scroll_id)
                .expect("stato ScrollArea dopo frame 1");
            assert_eq!(st.offset.x, 0.0);
            st.offset.x = 30.0;
            st.store(&ctx, scroll_id);
        }

        // Zoom in: pps 60 -> 75. The playhead (t=5s) must stay at the same
        // position on screen: offset' = 30 + 5 * (75 - 60) = 105.
        state.zoom_in();
        render_frame(&mut state);
        let st = egui::containers::scroll_area::State::load(&ctx, scroll_id).unwrap();
        assert_eq!(st.offset.x, 105.0);

        // Zoom out: pps 75 -> 60. The playhead did not move, so the offset
        // goes back exactly to 30.
        state.zoom_out();
        render_frame(&mut state);
        let st = egui::containers::scroll_area::State::load(&ctx, scroll_id).unwrap();
        assert_eq!(st.offset.x, 30.0);
    }

    /// Bug: the timeline panel (`Panel::bottom` with `show_timeline`
    /// inside, see `main.rs`) went back to the size of the
    /// content instead of staying at the one the user had
    /// resized it to, as soon as a frame passed without interaction.
    /// Cause: `ScrollArea` by default shrinks to the content instead
    /// of filling the space assigned by the `Panel` (`auto_shrink` is
    /// `true` on both axes by default). With few short clips
    /// (real content much shorter than 240px) the panel, over several
    /// consecutive frames without any interaction, must not shrink
    /// below the requested size.
    #[test]
    fn show_timeline_panel_does_not_shrink_to_short_content() {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![vv_core::Track::new(TrackKind::Video)],
        });
        let mut history = History::default();
        let clip = Clip::from_source_range(
            project.alloc_clip_id(),
            vv_core::ClipSource::SolidColor,
            0,
            10,
            0,
            vv_core::Rational::one(),
        );
        history.do_command(
            &mut project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip,
            }),
        );
        let mut state = TimelineState::default();

        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let mut last_height = 0.0_f32;
        for _ in 0..4 {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                let panel_resp = egui::Panel::bottom("timeline_repro")
                    .default_size(240.0)
                    .resizable(true)
                    .show(ui, |ui| {
                        show_timeline(
                            ui,
                            &mut project,
                            &mut history,
                            timeline_id,
                            &|_id| "media".to_string(),
                            &mut state,
                            true,
                            true,
                            &[],
                            &[],
                            &std::collections::HashMap::new(),
                            false,
                        );
                    });
                last_height = panel_resp.response.rect.height();
            });
            output.textures_delta.clear();
        }
        assert!(
            last_height > 200.0,
            "il pannello si è ristretto al contenuto ({last_height}px) invece di restare vicino ai 240px richiesti"
        );
    }
}
