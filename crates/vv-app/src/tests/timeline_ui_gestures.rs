//! Pointer gestures driven through `show_timeline` with synthetic input,
//! frame by frame: they pin down what each drag commits.

use super::*;
use vv_core::{CrossTransition, Ease, PushDirection, Rational, Track, Transition, TransitionKind};

/// 25 fps at the default 60 px/s: 2.4 px per frame.
const PX_PER_FRAME: f32 = 2.4;

struct Harness {
    ctx: egui::Context,
    project: Project,
    history: History,
    timeline_id: TimelineId,
    state: TimelineState,
    time: f64,
    /// `clip_rects` of the frame the button was released in.
    release_rects: std::collections::HashMap<ClipId, egui::Rect>,
}

impl Harness {
    fn new(tracks: Vec<Track>) -> Self {
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks,
        });
        let mut harness = Self {
            ctx: egui::Context::default(),
            project,
            history: History::default(),
            timeline_id,
            state: TimelineState::default(),
            time: 0.0,
            release_rects: std::collections::HashMap::new(),
        };
        // Two frames: the first one only lays the panes out.
        harness.frame(Vec::new());
        harness.frame(Vec::new());
        harness
    }

    fn frame(&mut self, events: Vec<egui::Event>) {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 700.0)));
        input.time = Some(self.time);
        self.time += 1.0 / 60.0;
        input.events = events;
        let mut output = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                show_timeline(
                    ui,
                    &mut self.project,
                    &mut self.history,
                    self.timeline_id,
                    &|_id| "media".to_string(),
                    &mut self.state,
                    false,
                    false,
                    &[],
                    &[],
                    &std::collections::HashMap::new(),
                    false,
                );
            });
        });
        output.textures_delta.clear();
    }

    fn rect(&self, clip: ClipId) -> egui::Rect {
        self.state.clip_rects[&clip]
    }

    /// Presses at `from` and drags by `delta`. The movement that makes egui
    /// start the drag is not part of it (the timeline accumulates only from
    /// the next frame): a pickup step past egui's threshold comes first, in
    /// the same direction, so the gesture itself moves by exactly `delta`.
    fn drag(&mut self, from: egui::Pos2, delta: egui::Vec2) {
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let picked_up = from + delta.normalized() * 12.0;
        self.frame(vec![egui::Event::PointerMoved(from)]);
        self.frame(vec![button(from, true)]);
        self.frame(vec![egui::Event::PointerMoved(picked_up)]);
        for step in 1..=5 {
            self.frame(vec![egui::Event::PointerMoved(picked_up + delta * (step as f32 / 5.0))]);
        }
        self.frame(vec![button(picked_up + delta, false)]);
        self.release_rects = self.state.clip_rects.clone();
        self.frame(Vec::new());
    }

    fn clip(&self, track_index: usize, id: ClipId) -> &Clip {
        self.project.timelines[self.timeline_id].clip(track_index, id).unwrap()
    }

    fn no_gesture(&self) -> bool {
        self.state.drag.is_none()
            && self.state.marquee.is_none()
            && self.state.trim.is_none()
            && self.state.fade_drag.is_none()
            && self.state.transition_drag.is_none()
            && self.state.crossing_drag.is_none()
            && self.state.transition_duplicate_drag.is_none()
            && self.state.volume_drag.is_none()
    }
}

fn solid(id: u64, start: FrameIdx, len: FrameIdx) -> Clip {
    Clip::from_source_range(ClipId(id), ClipSource::SolidColor, 0, len, start, Rational::one())
}

fn track(kind: TrackKind, clips: Vec<Clip>) -> Track {
    Track {
        clips,
        ..Track::new(kind)
    }
}

fn push(duration: FrameIdx) -> Transition {
    Transition {
        kind: TransitionKind::Push,
        duration,
        direction: PushDirection::Left,
        ease: Ease::None,
        curve: 0.0,
    }
}

#[test]
fn dragging_a_clip_body_moves_it() {
    let mut h = Harness::new(vec![track(TrackKind::Video, vec![solid(1, 0, 100)])]);
    let from = h.rect(ClipId(1)).center();
    h.drag(from, egui::vec2(50.0 * PX_PER_FRAME, 0.0));
    assert_eq!(h.clip(0, ClipId(1)).timeline_start, 50);
    assert_eq!(h.history.position(), 1);
    assert!(h.no_gesture());
}

#[test]
fn dragging_a_clip_edge_trims_it() {
    let mut h = Harness::new(vec![track(TrackKind::Video, vec![solid(1, 0, 100)])]);
    let rect = h.rect(ClipId(1));
    let from = egui::pos2(rect.right() - 2.0, rect.center().y);
    h.drag(from, egui::vec2(-25.0 * PX_PER_FRAME, 0.0));
    let clip = h.clip(0, ClipId(1));
    assert_eq!((clip.timeline_start, clip.timeline_len), (0, 75));
    assert!(h.no_gesture());
}

#[test]
fn dragging_the_fade_handle_sets_the_fade() {
    let mut h = Harness::new(vec![track(TrackKind::Video, vec![solid(1, 0, 100)])]);
    let rect = h.rect(ClipId(1));
    let from = egui::pos2(rect.left() + 1.0, rect.top() + 5.0);
    h.drag(from, egui::vec2(20.0 * PX_PER_FRAME, 0.0));
    assert_eq!(h.clip(0, ClipId(1)).fade_in, 20);
    assert!(h.no_gesture());
}

#[test]
fn dragging_a_transition_handle_sets_its_duration() {
    let mut clip = solid(1, 0, 100);
    clip.effects.transition_in = Some(push(10));
    let mut h = Harness::new(vec![track(TrackKind::Video, vec![clip])]);
    let rect = h.rect(ClipId(1));
    let from = egui::pos2(rect.left() + 10.0 * PX_PER_FRAME, rect.bottom() - 5.0);
    h.drag(from, egui::vec2(20.0 * PX_PER_FRAME, 0.0));
    let duration = h.clip(0, ClipId(1)).effects.transition_in.as_ref().unwrap().duration;
    assert_eq!(duration, 30);
    assert!(h.no_gesture());
}

#[test]
fn dragging_a_crossing_handle_sets_its_duration_on_both_sides() {
    let mut video = track(TrackKind::Video, vec![solid(1, 0, 100), solid(2, 100, 100)]);
    video.crossings.push(CrossTransition {
        left_clip: ClipId(1),
        right_clip: ClipId(2),
        transition: push(20),
    });
    let mut h = Harness::new(vec![video]);
    let rect = h.rect(ClipId(1));
    // The left half of the window (10 frames) ends inside the left clip.
    let from = egui::pos2(rect.right() - 10.0 * PX_PER_FRAME, rect.bottom() - 5.0);
    h.drag(from, egui::vec2(-10.0 * PX_PER_FRAME, 0.0));
    let crossing = &h.project.timelines[h.timeline_id].tracks[0].crossings[0];
    assert_eq!(crossing.transition.duration, 40);
    assert!(h.no_gesture());
}

#[test]
fn dragging_the_volume_line_sets_the_gain_in_one_undo_step() {
    let mut h = Harness::new(vec![
        track(TrackKind::Video, Vec::new()),
        track(TrackKind::Audio, vec![solid(1, 0, 100)]),
    ]);
    let from = h.rect(ClipId(1)).center();
    h.drag(from, egui::vec2(0.0, -10.0));
    assert!(h.clip(1, ClipId(1)).effects.gain_db.default > 0.0);
    assert!(h.no_gesture());
    h.history.undo(&mut h.project);
    assert_eq!(h.clip(1, ClipId(1)).effects.gain_db.default, 0.0);
}

#[test]
fn a_marquee_from_an_empty_area_selects_the_clips_it_touches() {
    let mut h = Harness::new(vec![track(TrackKind::Video, vec![solid(1, 0, 40), solid(2, 60, 40)])]);
    let first = h.rect(ClipId(1));
    let second = h.rect(ClipId(2));
    let from = egui::pos2(second.right() + 30.0, second.center().y);
    h.drag(from, egui::pos2(second.center().x, first.center().y + 1.0) - from);
    assert_eq!(h.state.selected, BTreeSet::from([(0, ClipId(2))]));
    assert!(h.no_gesture());
}

/// The gesture state must outlive the whole draw loop of the release frame:
/// cleared as soon as the dragged clip handles its release, a linked clip
/// drawn after it would flash back to its old place for one frame.
#[test]
fn a_linked_clip_drawn_after_the_dragged_one_does_not_flash_back_on_release() {
    let group = Some(vv_core::LinkGroupId(0));
    let mut video = solid(1, 0, 100);
    video.linked_group = group;
    let mut audio = solid(2, 0, 100);
    audio.linked_group = group;
    let mut h = Harness::new(vec![track(TrackKind::Video, vec![video]), track(TrackKind::Audio, vec![audio])]);
    let from = h.rect(ClipId(1)).center();
    h.drag(from, egui::vec2(50.0 * PX_PER_FRAME, 0.0));
    assert_eq!(h.clip(1, ClipId(2)).timeline_start, 50);
    assert_eq!(h.release_rects[&ClipId(2)].left(), h.rect(ClipId(2)).left());
}
