use super::*;
use vv_core::{ClipSource, Interpolation, Rational};

fn clip(id: u64, source_in: FrameIdx, source_out: FrameIdx) -> Clip {
    Clip::from_source_range(
        ClipId(id),
        ClipSource::SolidColor,
        source_in,
        source_out,
        0,
        Rational::one(),
    )
}

fn source_clip() -> Clip {
    let mut clip = clip(1, 0, 20);
    clip.effects.transform.track_mut(TransformParam::ZoomX).default = 2.0;
    clip.effects.transform.track_mut(TransformParam::Opacity).default = 0.5;
    clip.fade_in = 4;
    clip
}

#[test]
fn only_the_ticked_attributes_are_pasted() {
    let target = clip(2, 0, 20);
    let selected = HashSet::from([Attribute::Zoom]);
    let pasted = merged_attributes(&source_clip(), &target, &selected, KeyframeMode::MaintainTiming);

    assert_eq!(pasted.effects.transform.track(TransformParam::ZoomX).default, 2.0);
    assert_eq!(
        pasted.effects.transform.track(TransformParam::Opacity).default,
        target.effects.transform.track(TransformParam::Opacity).default,
        "unselected opacity stays that of the destination clip"
    );
    assert_eq!(pasted.fade_in, 0, "unselected fades are left alone");
}

/// The keyframes are in source frames: a target clip starting from
/// another point of its media must keep the same distances from its
/// own start.
#[test]
fn maintain_timing_keeps_the_offsets_from_the_start_of_the_clip() {
    let mut source = source_clip();
    source
        .effects
        .transform
        .track_mut(TransformParam::ZoomX)
        .upsert(5, 3.0, Interpolation::Linear);
    let target = clip(2, 100, 120);

    let pasted = merged_attributes(
        &source,
        &target,
        &HashSet::from([Attribute::Zoom]),
        KeyframeMode::MaintainTiming,
    );

    let keyframes = pasted.effects.transform.track(TransformParam::ZoomX).keyframes();
    assert_eq!(keyframes.len(), 1);
    assert_eq!(keyframes[0].0, 105);
}

#[test]
fn stretch_to_fit_scales_the_keyframes_on_the_target_duration() {
    let mut source = source_clip();
    let track = source.effects.transform.track_mut(TransformParam::ZoomX);
    track.upsert(10, 3.0, Interpolation::Linear);
    track.upsert(15, 4.0, Interpolation::Linear);
    // Half the source: 20 source frames against 40.
    let target = clip(2, 0, 40);

    let pasted = merged_attributes(
        &source,
        &target,
        &HashSet::from([Attribute::Zoom]),
        KeyframeMode::StretchToFit,
    );

    let keyframes = pasted.effects.transform.track(TransformParam::ZoomX).keyframes();
    assert_eq!(keyframes.iter().map(|k| k.0).collect::<Vec<_>>(), vec![20, 30]);
}

/// A keyframe past the end of the target clip has nowhere to go.
#[test]
fn keyframes_outside_the_target_clip_are_dropped() {
    let mut source = source_clip();
    source
        .effects
        .transform
        .track_mut(TransformParam::ZoomX)
        .upsert(15, 3.0, Interpolation::Linear);
    let target = clip(2, 0, 5);

    let pasted = merged_attributes(
        &source,
        &target,
        &HashSet::from([Attribute::Zoom]),
        KeyframeMode::MaintainTiming,
    );

    assert!(pasted.effects.transform.track(TransformParam::ZoomX).is_constant());
    assert_eq!(pasted.effects.transform.track(TransformParam::ZoomX).default, 2.0);
}

#[test]
fn fades_and_transitions_are_clamped_to_the_target_length() {
    let mut source = source_clip();
    source.fade_out = 8;
    let target = clip(2, 0, 5);

    let pasted = merged_attributes(
        &source,
        &target,
        &HashSet::from([Attribute::Fades]),
        KeyframeMode::MaintainTiming,
    );

    assert_eq!(pasted.fade_in, 4);
    assert_eq!(pasted.fade_out, 5);
}

/// The whole path: copy, select another clip, apply the dialog.
#[test]
fn applying_the_dialog_writes_a_single_undoable_step() {
    let mut app = VenturiApp::default();
    let timeline_id = app.ensure_timeline();
    let track_index = app.project.timelines[timeline_id]
        .tracks_of_kind(TrackKind::Video)
        .next()
        .map(|(i, _)| i)
        .expect("the timeline is created with a video track");

    let mut source = clip(0, 0, 20);
    source.id = app.project.alloc_clip_id();
    source.effects.transform.track_mut(TransformParam::ZoomX).default = 2.0;
    let source_id = source.id;
    let mut target = clip(0, 0, 20);
    target.id = app.project.alloc_clip_id();
    target.timeline_start = 40;
    let target_id = target.id;
    let track = &mut app.project.timelines[timeline_id].tracks[track_index];
    track.insert_sorted(source);
    track.insert_sorted(target);

    app.timeline_state.selected.insert((track_index, source_id));
    app.copy_selected_clips();
    app.timeline_state
        .set_selection(std::collections::BTreeSet::from([(track_index, target_id)]), None);
    app.paste_attributes_selection = HashSet::from([Attribute::Zoom]);
    app.open_paste_attributes_dialog();
    let dialog = app.paste_attributes.take().expect("the dialog opens");
    app.apply_paste_attributes(&dialog);

    let zoom_of = |app: &VenturiApp, id| {
        app.project.timelines[timeline_id]
            .clip(track_index, id)
            .unwrap()
            .effects
            .transform
            .track(TransformParam::ZoomX)
            .default
    };
    assert_eq!(zoom_of(&app, target_id), 2.0);
    app.history.undo(&mut app.project);
    assert_eq!(zoom_of(&app, target_id), 1.0, "a single undo step");
}
