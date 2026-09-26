use super::*;

fn project_with_tracks(kinds: &[TrackKind]) -> (Project, TimelineId) {
    let mut project = Project::default();
    let timeline = project.timelines.insert(Timeline {
        name: "Timeline 1".into(),
        fps: Rational::new(25, 1),
        resolution: (1920, 1080),
        tracks: kinds.iter().map(|k| Track::new(*k)).collect(),
    });
    (project, timeline)
}

fn insert_clip(
    project: &mut Project,
    timeline: TimelineId,
    track_index: usize,
    start: FrameIdx,
    len: FrameIdx,
) -> ClipId {
    let clip = Clip::from_source_range(
        project.alloc_clip_id(),
        ClipSource::SolidColor,
        0,
        len,
        start,
        Rational::one(),
    );
    let id = clip.id;
    project.timelines[timeline].tracks[track_index].insert_sorted(clip);
    id
}

/// Inserts into the pool a compound clip referencing `plan`, as the real
/// caller (main.rs) would before running `compound_clip_commands`.
fn insert_compound_media(project: &mut Project, plan: &CompoundPlan) -> MediaId {
    let nested = project.timelines.insert(Timeline {
        name: plan.nested_timeline.name.clone(),
        fps: plan.nested_timeline.fps,
        resolution: plan.nested_timeline.resolution,
        tracks: plan.nested_timeline.tracks.clone(),
    });
    let path = project.alloc_compound_name().into();
    let content_hash = project.alloc_compound_generation();
    project.media_pool.insert(MediaItem {
        path,
        meta: MediaMeta {
            duration_frames: plan.len,
            fps: plan.nested_timeline.fps,
            width: plan.nested_timeline.resolution.0,
            height: plan.nested_timeline.resolution.1,
            has_video: plan.has_video,
            has_audio: plan.has_audio,
            sample_rate: 48_000,
            channels: 2,
            audio_streams: 1,
        },
        content_hash,
        compound: Some(nested),
    })
}

#[test]
fn plan_rebases_clips_into_the_nested_timeline_preserving_relative_track_order() {
    let (mut project, timeline) = project_with_tracks(&[TrackKind::Video, TrackKind::Audio]);
    let video_id = insert_clip(&mut project, timeline, 0, 10, 30);
    let audio_id = insert_clip(&mut project, timeline, 1, 20, 30);

    let plan = plan_compound_clip(&project, timeline, &[(0, video_id), (1, audio_id)]).unwrap();

    assert_eq!(plan.range_start, 10);
    assert_eq!(plan.len, 40); // up to 50 (end of the audio clip), from 10
    assert!(plan.has_video && plan.has_audio);
    assert_eq!(plan.video_track, Some(0));
    assert_eq!(plan.audio_track, Some(1));

    assert_eq!(plan.nested_timeline.tracks.len(), 2);
    assert_eq!(plan.nested_timeline.tracks[0].kind, TrackKind::Video);
    assert_eq!(plan.nested_timeline.tracks[0].clips[0].timeline_start, 0);
    assert_eq!(plan.nested_timeline.tracks[1].kind, TrackKind::Audio);
    assert_eq!(plan.nested_timeline.tracks[1].clips[0].timeline_start, 10);
}

#[test]
fn plan_folds_multiple_video_tracks_into_a_single_result_clip_on_the_bottommost() {
    let (mut project, timeline) = project_with_tracks(&[TrackKind::Video, TrackKind::Video]);
    let bottom = insert_clip(&mut project, timeline, 0, 0, 20);
    let top = insert_clip(&mut project, timeline, 1, 10, 20);

    let plan = plan_compound_clip(&project, timeline, &[(0, bottom), (1, top)]).unwrap();

    assert!(plan.has_video);
    assert!(!plan.has_audio);
    assert_eq!(
        plan.video_track,
        Some(0),
        "the lowest of the tracks involved"
    );
    assert_eq!(plan.audio_track, None);
    assert_eq!(
        plan.nested_timeline.tracks.len(),
        2,
        "one nested track per video track involved"
    );
}

#[test]
fn deleting_a_compound_clip_frees_its_name_and_its_nested_timeline() {
    let (mut project, timeline) = project_with_tracks(&[TrackKind::Video]);
    let id = insert_clip(&mut project, timeline, 0, 0, 20);
    let plan = plan_compound_clip(&project, timeline, &[(0, id)]).unwrap();
    let media = insert_compound_media(&mut project, &plan);
    assert_eq!(
        project.media_pool[media].path.to_str(),
        Some("Compound Clip 1")
    );

    let mut history = History::default();
    history.do_command(&mut project, Box::new(command::RemoveMedia::new(media)));
    assert_eq!(
        project.timelines.len(),
        1,
        "the nested timeline goes away with the media"
    );

    let again = insert_compound_media(&mut project, &plan);
    assert_eq!(
        project.media_pool[again].path.to_str(),
        Some("Compound Clip 1")
    );
}

#[test]
fn undoing_the_deletion_of_a_compound_clip_restores_its_nested_timeline() {
    let (mut project, timeline) = project_with_tracks(&[TrackKind::Video]);
    let id = insert_clip(&mut project, timeline, 0, 0, 20);
    let plan = plan_compound_clip(&project, timeline, &[(0, id)]).unwrap();
    let media = insert_compound_media(&mut project, &plan);

    let mut history = History::default();
    history.do_command(&mut project, Box::new(command::RemoveMedia::new(media)));
    history.undo(&mut project);

    assert_eq!(project.media_pool.len(), 1);
    let nested = project
        .media_pool
        .values()
        .next()
        .unwrap()
        .compound
        .unwrap();
    assert!(project.timelines.contains_key(nested));
}

#[test]
fn plan_returns_none_for_an_empty_selection() {
    let (project, timeline) = project_with_tracks(&[TrackKind::Video]);
    assert!(plan_compound_clip(&project, timeline, &[]).is_none());
}

#[test]
fn compound_clip_commands_replace_selection_with_linked_result_and_undo_restores_it() {
    let (mut project, timeline) = project_with_tracks(&[TrackKind::Video, TrackKind::Audio]);
    let video_id = insert_clip(&mut project, timeline, 0, 10, 30);
    let audio_id = insert_clip(&mut project, timeline, 1, 20, 30);
    let selection = vec![(0, video_id), (1, audio_id)];

    let plan = plan_compound_clip(&project, timeline, &selection).unwrap();
    let media_id = insert_compound_media(&mut project, &plan);
    let commands = compound_clip_commands(&mut project, timeline, &selection, &plan, media_id);

    let mut history = History::default();
    history.do_command(
        &mut project,
        Box::new(CompositeCommand::new(
            CommandLabel::MakeCompoundClip,
            commands,
        )),
    );

    let video_track = &project.timelines[timeline].tracks[0];
    assert_eq!(video_track.clips.len(), 1);
    assert!(matches!(video_track.clips[0].source, ClipSource::Media(id) if id == media_id));
    assert_eq!(video_track.clips[0].timeline_start, 10);
    assert_eq!(video_track.clips[0].timeline_len, 40);

    let audio_track = &project.timelines[timeline].tracks[1];
    assert_eq!(audio_track.clips.len(), 1);
    assert!(matches!(audio_track.clips[0].source, ClipSource::Media(id) if id == media_id));
    let group = video_track.clips[0]
        .linked_group
        .expect("video linked to the audio");
    assert_eq!(audio_track.clips[0].linked_group, Some(group));

    // The pool and the nested timeline stay out of the history.
    assert_eq!(project.media_pool.len(), 1);
    assert_eq!(project.timelines.len(), 2);

    history.undo(&mut project);
    assert_eq!(project.timelines[timeline].tracks[0].clips.len(), 1);
    assert_eq!(project.timelines[timeline].tracks[0].clips[0].id, video_id);
    assert_eq!(
        project.timelines[timeline].tracks[0].clips[0].timeline_start,
        10
    );
    assert_eq!(project.timelines[timeline].tracks[1].clips[0].id, audio_id);
    // Undo does not un-import the compound clip from the pool, like an import.
    assert_eq!(project.media_pool.len(), 1);
}

fn compound_media_for(project: &mut Project, nested: TimelineId) -> MediaId {
    project.media_pool.insert(MediaItem {
        path: "Compound".into(),
        meta: MediaMeta {
            duration_frames: 10,
            fps: Rational::new(25, 1),
            width: 1920,
            height: 1080,
            has_video: true,
            has_audio: false,
            sample_rate: 0,
            channels: 0,
            audio_streams: 0,
        },
        content_hash: 1,
        compound: Some(nested),
    })
}

#[test]
fn would_create_a_cycle_catches_importing_a_timeline_into_itself() {
    let mut project = Project::default();
    let timeline = project.timelines.insert(Timeline {
        name: "T".into(),
        fps: Rational::new(25, 1),
        resolution: (1920, 1080),
        tracks: vec![],
    });
    let media = compound_media_for(&mut project, timeline);

    assert!(
        project.would_create_a_cycle(media, timeline),
        "T inside itself"
    );
}

#[test]
fn would_create_a_cycle_catches_an_indirect_cycle_through_a_nested_compound_clip() {
    let mut project = Project::default();
    let a = project.timelines.insert(Timeline {
        name: "A".into(),
        fps: Rational::new(25, 1),
        resolution: (1920, 1080),
        tracks: vec![],
    });
    let b = project.timelines.insert(Timeline {
        name: "B".into(),
        fps: Rational::new(25, 1),
        resolution: (1920, 1080),
        tracks: vec![],
    });
    let media_a = compound_media_for(&mut project, a);
    let media_b = compound_media_for(&mut project, b);
    // B already contains a clip referencing A: importing B inside A
    // would close the cycle A -> B -> A.
    project.timelines[b].tracks.push(Track {
        kind: TrackKind::Video,
        clips: vec![Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(media_a),
            0,
            10,
            0,
            Rational::one(),
        )],
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
    });

    assert!(project.would_create_a_cycle(media_b, a), "A -> B -> A");
    assert!(
        !project.would_create_a_cycle(media_a, b),
        "B -> A not yet a cycle on B"
    );
}

#[test]
fn would_create_a_cycle_is_false_for_unrelated_timelines() {
    let mut project = Project::default();
    let a = project.timelines.insert(Timeline {
        name: "A".into(),
        fps: Rational::new(25, 1),
        resolution: (1920, 1080),
        tracks: vec![],
    });
    let b = project.timelines.insert(Timeline {
        name: "B".into(),
        fps: Rational::new(25, 1),
        resolution: (1920, 1080),
        tracks: vec![],
    });
    let media_a = compound_media_for(&mut project, a);

    assert!(!project.would_create_a_cycle(media_a, b));
}
