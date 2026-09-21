pub mod command;
pub mod model;
pub mod otio;
pub mod persistence;

pub use command::{
    AddTrack, Command, CommandLabel, CompositeCommand, CompoundPlan, FadeEdge, GroupMark, History, InsertClip,
    KeyframePick, KeyframeTarget, KeyframeValue, LiftDelete, LinkClips, MoveClips, MoveKeyframes, RemoveKeyframe,
    RemoveMedia, RemoveTrack,
    ResetTransformParams, RippleDeleteGap, SetClipColor, SetClipFade, SetClipValue, SetClipsDisabled,
    SetKeyframeInterpolation,
    SetCrossTransition, SetMediaPath, SetTrackFlag, SplitClip, TrackFlag, TrimClip, TrimEdge, UnlinkClip,
    UpsertKeyframe, compound_clip_commands, cut_overlaps, insert_overwriting, make_room_for_ranges,
    plan_compound_clip, reset_clip_gain, set_clip_blend_mode, set_clip_filters, set_clip_flip, set_clip_gain, set_clip_title,
    set_clip_transform_param, set_clip_transition,
};
pub use model::*;
pub use otio::{OtioError, OtioImport, OtioWarning, export_otio, import_otio};
pub use persistence::{PersistenceError, load_project, save_project};

#[cfg(test)]
mod tests {
    use super::*;

    fn make_project_with_two_tracks() -> (Project, TimelineId) {
        let mut project = Project::default();
        let timeline = project.timelines.insert(Timeline {
            name: "Timeline 1".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        (project, timeline)
    }

    fn make_clip(project: &mut Project, start: FrameIdx, len: FrameIdx) -> Clip {
        Clip::from_source_range(
            project.alloc_clip_id(),
            ClipSource::SolidColor,
            0,
            len,
            start,
            Rational::one(),
        )
    }

    fn insert_media(project: &mut Project) -> MediaId {
        project.media_pool.insert(MediaItem {
            path: "a.mp4".into(),
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 1920,
                height: 1080,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 7,
            compound: None,
        })
    }

    #[test]
    fn remove_media_leaves_its_clips_offline_and_undo_reconnects_them() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let media = insert_media(&mut project);

        let mut clip = make_clip(&mut project, 0, 10);
        clip.source = ClipSource::Media(media);
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip,
            }),
        );

        history.do_command(&mut project, Box::new(command::RemoveMedia::new(media)));
        assert!(project.media_pool.is_empty());
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        let ClipSource::Media(dangling) = clip.source else {
            panic!("la clip deve restare in timeline, solo senza media");
        };
        assert!(project.media_pool.get(dangling).is_none());

        // L'undo reinserisce con una chiave slotmap nuova: la clip deve
        // puntare a quella, non alla vecchia.
        history.undo(&mut project);
        assert_eq!(project.media_pool.len(), 1);
        let restored = project.media_pool.keys().next().unwrap();
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert!(matches!(clip.source, ClipSource::Media(id) if id == restored));

        history.redo(&mut project);
        assert!(project.media_pool.is_empty());
    }

    #[test]
    fn set_media_path_relinks_and_undo_restores_the_old_path() {
        let (mut project, _timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let media = insert_media(&mut project);
        let old_path = project.media_pool[media].path.clone();

        history.do_command(
            &mut project,
            Box::new(command::SetMediaPath::new(
                media,
                "/altra/postazione/clip.mp4".into(),
                42,
            )),
        );
        assert_eq!(project.media_pool[media].path, std::path::PathBuf::from("/altra/postazione/clip.mp4"));
        assert_eq!(project.media_pool[media].content_hash, 42);

        history.undo(&mut project);
        assert_eq!(project.media_pool[media].path, old_path);
        assert_eq!(project.media_pool[media].content_hash, 7);
    }

    #[test]
    fn add_track_appends_and_undo_removes_it() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        history.do_command(
            &mut project,
            Box::new(command::AddTrack::new(timeline, TrackKind::Video)),
        );
        assert_eq!(project.timelines[timeline].tracks.len(), 3);
        assert_eq!(project.timelines[timeline].tracks[2].kind, TrackKind::Video);

        history.undo(&mut project);
        assert_eq!(project.timelines[timeline].tracks.len(), 2);
    }

    #[test]
    fn go_to_jumps_back_and_forth_through_the_history() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        for _ in 0..3 {
            history.do_command(
                &mut project,
                Box::new(command::AddTrack::new(timeline, TrackKind::Video)),
            );
        }
        history.do_command(
            &mut project,
            Box::new(command::SetTrackFlag::new(timeline, 0, TrackFlag::Muted, true)),
        );

        history.go_to(&mut project, 1);
        assert_eq!(project.timelines[timeline].tracks.len(), 3);
        assert!(!project.timelines[timeline].tracks[0].muted);
        assert_eq!(history.position(), 1);
        assert_eq!(history.labels().count(), 4, "i passi annullati restano in elenco");

        history.go_to(&mut project, 4);
        assert_eq!(project.timelines[timeline].tracks.len(), 5);
        assert!(project.timelines[timeline].tracks[0].muted);
        assert_eq!(history.labels().last(), Some(CommandLabel::MuteTrack));

        history.go_to(&mut project, 0);
        assert_eq!(project.timelines[timeline].tracks.len(), 2);
        history.go_to(&mut project, 99);
        assert_eq!(history.position(), 4);
    }

    #[test]
    fn group_takes_the_explicit_label_over_its_first_command() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let mark = history.begin_group();
        history.do_command(
            &mut project,
            Box::new(command::AddTrack::new(timeline, TrackKind::Video)),
        );
        history.do_command(
            &mut project,
            Box::new(command::SetTrackFlag::new(timeline, 2, TrackFlag::Locked, true)),
        );
        history.end_group_as(mark, CommandLabel::InsertClips);
        assert_eq!(history.labels().collect::<Vec<_>>(), [CommandLabel::InsertClips]);
    }

    #[test]
    fn remove_track_deletes_its_clips_too_and_undo_restores_everything() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 10);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::RemoveTrack::new(timeline, 0)),
        );
        assert_eq!(project.timelines[timeline].tracks.len(), 1);
        assert_eq!(project.timelines[timeline].tracks[0].kind, TrackKind::Audio);

        history.undo(&mut project);
        assert_eq!(project.timelines[timeline].tracks.len(), 2);
        assert_eq!(project.timelines[timeline].tracks[0].kind, TrackKind::Video);
        assert_eq!(project.timelines[timeline].tracks[0].clips[0].id, a_id);
    }

    #[test]
    fn move_clip_between_tracks_and_undo_restores() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 10);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::MoveClips::new(timeline, vec![(a_id, 0, 1, 25)])),
        );

        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 0);
        assert_eq!(tl.tracks[1].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 25);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 0);
        assert_eq!(tl.tracks[0].clips[0].timeline_start, 0);
    }

    #[test]
    fn trim_start_moves_timeline_start_and_source_in_together_keeping_the_end_fixed() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 10, 20); // [10, 30), source [0, 20)
        let a_id = a.id;
        let original_end = a.timeline_end();
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        // Trimma il bordo sinistro da 10 a 15: source_in passa da 0 a 5.
        history.do_command(
            &mut project,
            Box::new(command::TrimClip::new(
                timeline,
                0,
                a_id,
                TrimEdge::Start,
                15,
            )),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_in(), 5);
        assert_eq!(clip.timeline_start, 15, "si sposta della stessa quantità");
        assert_eq!(
            clip.timeline_end(),
            original_end,
            "la fine sulla timeline resta ferma"
        );

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_in(), 0);
        assert_eq!(clip.timeline_start, 10);
    }

    #[test]
    fn trim_end_changes_source_out_leaving_the_start_fixed() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 10, 20); // [10, 30), source [0, 20)
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::TrimClip::new(timeline, 0, a_id, TrimEdge::End, 25)),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_out(), 15);
        assert_eq!(
            clip.timeline_start, 10,
            "l'inizio sulla timeline resta fermo"
        );
        assert_eq!(clip.timeline_end(), 25);

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_out(), 20);
        assert_eq!(clip.timeline_end(), 30);
    }

    #[test]
    fn split_clip_creates_two_clips_and_undo_merges_back() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::SplitClip::new(timeline, 0, a_id, 8)),
        );

        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 2);
        let first = &tl.tracks[0].clips[0];
        let second = &tl.tracks[0].clips[1];
        assert_eq!(first.timeline_start, 0);
        assert_eq!(first.source_out(), 8);
        assert_eq!(second.timeline_start, 8);
        assert_eq!(second.source_in(), 8);
        assert_eq!(second.source_out(), 20);
        assert_ne!(first.id, second.id);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].source_out(), 20);
    }

    #[test]
    fn splitting_either_side_of_a_crossing_removes_it_and_undo_restores_it() {
        let make_transition = || Transition {
            kind: TransitionKind::Push,
            duration: 4,
            direction: PushDirection::Right,
            ease: Ease::None,
            curve: 0.0,
        };

        for split_the_right_clip in [false, true] {
            let (mut project, timeline) = make_project_with_two_tracks();
            let mut history = History::default();

            let a = make_clip(&mut project, 0, 20);
            let a_id = a.id;
            let b = make_clip(&mut project, 20, 20);
            let b_id = b.id;
            history.do_command(&mut project, Box::new(command::InsertClip { timeline, track_index: 0, clip: a }));
            history.do_command(&mut project, Box::new(command::InsertClip { timeline, track_index: 0, clip: b }));
            history.do_command(
                &mut project,
                Box::new(command::SetCrossTransition::new(
                    timeline,
                    0,
                    a_id,
                    Some(CrossTransition { left_clip: a_id, right_clip: b_id, transition: make_transition() }),
                )),
            );
            assert_eq!(project.timelines[timeline].tracks[0].crossings.len(), 1);

            let (split_clip_id, split_at) = if split_the_right_clip { (b_id, 28) } else { (a_id, 8) };
            history.do_command(&mut project, Box::new(command::SplitClip::new(timeline, 0, split_clip_id, split_at)));

            assert!(
                project.timelines[timeline].tracks[0].crossings.is_empty(),
                "split_the_right_clip={split_the_right_clip}: la crossing tra le due clip originali doveva sparire, non restare orfana"
            );

            history.undo(&mut project);
            assert_eq!(
                project.timelines[timeline].tracks[0].crossings,
                vec![CrossTransition { left_clip: a_id, right_clip: b_id, transition: make_transition() }],
                "split_the_right_clip={split_the_right_clip}: l'undo dello split deve far tornare la crossing"
            );
        }
    }

    #[test]
    fn deleting_a_clip_removes_its_crossings_and_undo_restores_them() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        let b = make_clip(&mut project, 20, 20);
        let b_id = b.id;
        history.do_command(&mut project, Box::new(command::InsertClip { timeline, track_index: 0, clip: a }));
        history.do_command(&mut project, Box::new(command::InsertClip { timeline, track_index: 0, clip: b }));
        let transition = Transition {
            kind: TransitionKind::Push,
            duration: 4,
            direction: PushDirection::Right,
            ease: Ease::None,
            curve: 0.0,
        };
        history.do_command(
            &mut project,
            Box::new(command::SetCrossTransition::new(
                timeline,
                0,
                a_id,
                Some(CrossTransition { left_clip: a_id, right_clip: b_id, transition: transition.clone() }),
            )),
        );
        assert_eq!(project.timelines[timeline].tracks[0].crossings.len(), 1);

        history.do_command(&mut project, Box::new(command::LiftDelete::new(timeline, 0, a_id)));
        assert!(
            project.timelines[timeline].tracks[0].crossings.is_empty(),
            "cancellare a_id deve togliere la crossing, non lasciarla puntare a un ClipId inesistente"
        );

        history.undo(&mut project);
        assert_eq!(project.timelines[timeline].tracks[0].clips.len(), 2);
        assert_eq!(
            project.timelines[timeline].tracks[0].crossings,
            vec![CrossTransition { left_clip: a_id, right_clip: b_id, transition }],
        );
    }

    #[test]
    fn moving_a_clip_to_another_track_removes_its_crossings_and_undo_restores_them() {
        let mut project = Project::default();
        let timeline = project.timelines.insert(Timeline {
            name: "Timeline 1".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Video)],
        });
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        let b = make_clip(&mut project, 20, 20);
        let b_id = b.id;
        history.do_command(&mut project, Box::new(command::InsertClip { timeline, track_index: 0, clip: a }));
        history.do_command(&mut project, Box::new(command::InsertClip { timeline, track_index: 0, clip: b }));
        let transition = Transition {
            kind: TransitionKind::Push,
            duration: 4,
            direction: PushDirection::Right,
            ease: Ease::None,
            curve: 0.0,
        };
        history.do_command(
            &mut project,
            Box::new(command::SetCrossTransition::new(
                timeline,
                0,
                a_id,
                Some(CrossTransition { left_clip: a_id, right_clip: b_id, transition: transition.clone() }),
            )),
        );

        history.do_command(&mut project, Box::new(command::MoveClips::new(timeline, vec![(a_id, 0, 1, 0)])));
        assert!(
            project.timelines[timeline].tracks[0].crossings.is_empty(),
            "spostare a_id su un'altra track deve togliere la crossing dalla track di partenza"
        );

        history.undo(&mut project);
        assert_eq!(
            project.timelines[timeline].tracks[0].crossings,
            vec![CrossTransition { left_clip: a_id, right_clip: b_id, transition }],
        );
    }

    fn split_clip_with_gain_keyframes(keyframes: &[(FrameIdx, f32)], split_at: FrameIdx) -> (Project, History, TimelineId) {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let mut a = make_clip(&mut project, 0, 20);
        for (f, v) in keyframes {
            a.effects.gain_db.upsert(*f, *v, Interpolation::Linear);
        }
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::SplitClip::new(timeline, 0, a_id, split_at)),
        );
        (project, history, timeline)
    }

    #[test]
    fn split_after_the_last_keyframe_leaves_the_right_half_without_keyframes() {
        let (project, _, timeline) = split_clip_with_gain_keyframes(&[(2, 0.0), (5, -6.0)], 12);
        let clips = &project.timelines[timeline].tracks[0].clips;
        assert_eq!(clips[0].effects.gain_db.keyframes().len(), 2);
        let right = &clips[1].effects.gain_db;
        assert!(right.is_constant());
        assert_eq!(right.value_at(12), -6.0, "tiene il valore che aveva al taglio");
    }

    #[test]
    fn split_mid_interpolation_keeps_the_values_on_both_sides() {
        let (project, _, timeline) = split_clip_with_gain_keyframes(&[(0, 0.0), (10, -10.0)], 4);
        let clips = &project.timelines[timeline].tracks[0].clips;
        let (left, right) = (&clips[0].effects.gain_db, &clips[1].effects.gain_db);
        assert!(left.keyframes().iter().all(|(f, _, _)| *f < 4));
        assert!(right.keyframes().iter().all(|(f, _, _)| *f >= 4));
        for f in 0..4 {
            assert_eq!(left.value_at(f), -(f as f32));
        }
        for f in 4..20 {
            assert_eq!(right.value_at(f), -(f.min(10) as f32));
        }
    }

    #[test]
    fn undo_split_restores_the_keyframes_of_the_left_half() {
        let (mut project, mut history, timeline) =
            split_clip_with_gain_keyframes(&[(2, 0.0), (15, -6.0)], 8);
        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.gain_db.keyframes().len(), 2);
        assert_eq!(clip.effects.gain_db.value_at(15), -6.0);
    }

    const RATES: [Rational; 4] = [
        Rational::new(1, 1),
        Rational::new(1001, 1000),
        Rational::new(6, 5),
        Rational::new(5, 6),
    ];

    fn span(clip: &Clip) -> (FrameIdx, FrameIdx, FrameIdx) {
        (clip.timeline_start, clip.source_offset, clip.timeline_len)
    }

    /// Anche su una clip conformata il taglio cade esattamente dove
    /// richiesto, e ogni frame di timeline mostra lo stesso frame sorgente
    /// di prima: la metà destra non perde la fase.
    #[test]
    fn split_clip_cuts_exactly_and_preserves_every_frame_at_any_rate() {
        for rate in RATES {
            let (mut project, timeline) = make_project_with_two_tracks();
            let mut history = History::default();
            let a = Clip::from_source_range(
                project.alloc_clip_id(),
                ClipSource::SolidColor,
                17,
                317,
                40,
                rate,
            );
            let original = a.clone();
            history.do_command(
                &mut project,
                Box::new(command::InsertClip {
                    timeline,
                    track_index: 0,
                    clip: a,
                }),
            );

            for split_at in original.timeline_start + 1..original.timeline_end() {
                history.do_command(
                    &mut project,
                    Box::new(command::SplitClip::new(timeline, 0, original.id, split_at)),
                );
                let clips = &project.timelines[timeline].tracks[0].clips;
                assert_eq!(clips.len(), 2);
                assert_eq!(clips[0].timeline_end(), split_at);
                assert_eq!(clips[1].timeline_start, split_at);
                assert_eq!(clips[1].timeline_end(), original.timeline_end());
                for t in original.timeline_start..original.timeline_end() {
                    let half = if t < split_at { &clips[0] } else { &clips[1] };
                    assert_eq!(
                        half.source_frame_at(t),
                        original.source_frame_at(t),
                        "rate {rate:?}, split a {split_at}, t {t}"
                    );
                }

                history.undo(&mut project);
                let clips = &project.timelines[timeline].tracks[0].clips;
                assert_eq!(clips.len(), 1);
                assert_eq!(span(&clips[0]), span(&original));
            }
        }
    }

    /// Stesso principio dello split per i due bordi di un trim.
    #[test]
    fn trim_cuts_exactly_and_preserves_every_remaining_frame_at_any_rate() {
        for rate in RATES {
            for edge in [TrimEdge::Start, TrimEdge::End] {
                let (mut project, timeline) = make_project_with_two_tracks();
                let mut history = History::default();
                let a = Clip::from_source_range(
                    project.alloc_clip_id(),
                    ClipSource::SolidColor,
                    17,
                    317,
                    40,
                    rate,
                );
                let original = a.clone();
                history.do_command(
                    &mut project,
                    Box::new(command::InsertClip {
                        timeline,
                        track_index: 0,
                        clip: a,
                    }),
                );

                for edge_at in original.timeline_start + 1..original.timeline_end() {
                    history.do_command(
                        &mut project,
                        Box::new(command::TrimClip::new(timeline, 0, original.id, edge, edge_at)),
                    );
                    let clip = &project.timelines[timeline].tracks[0].clips[0];
                    let expected = match edge {
                        TrimEdge::Start => (edge_at, original.timeline_end()),
                        TrimEdge::End => (original.timeline_start, edge_at),
                    };
                    assert_eq!((clip.timeline_start, clip.timeline_end()), expected);
                    for t in clip.timeline_start..clip.timeline_end() {
                        assert_eq!(
                            clip.source_frame_at(t),
                            original.source_frame_at(t),
                            "rate {rate:?}, {edge:?} a {edge_at}, t {t}"
                        );
                    }

                    history.undo(&mut project);
                    let clip = &project.timelines[timeline].tracks[0].clips[0];
                    assert_eq!(span(clip), span(&original));
                }
            }
        }
    }

    #[test]
    fn trim_start_on_a_conformed_clip_keeps_the_timeline_end_fixed() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let rate = Rational::conform_rate(Rational::new(60, 1), Rational::new(60000, 1001));
        let a = Clip::from_source_range(
            project.alloc_clip_id(),
            ClipSource::SolidColor,
            0,
            6000,
            1000,
            rate,
        );
        let a_id = a.id;
        let original_end = a.timeline_end();
        let new_start = a.timeline_frame_at(3000);
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::TrimClip::new(timeline, 0, a_id, TrimEdge::Start, new_start)),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_in(), 3000);
        assert_eq!(clip.timeline_end(), original_end);
        assert_eq!(clip.timeline_len, 3003, "3000 frame a 59,94 su 60 fps");

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!((clip.source_in(), clip.timeline_start), (0, 1000));
    }

    #[test]
    fn split_clip_outside_body_is_a_no_op() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        // split_at fuori dal corpo della clip (>= end): nessun effetto.
        history.do_command(
            &mut project,
            Box::new(command::SplitClip::new(timeline, 0, a_id, 20)),
        );

        assert_eq!(project.timelines[timeline].tracks[0].clips.len(), 1);
    }

    #[test]
    fn set_clip_transform_and_gain_undo_restore_defaults() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 10);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::set_clip_transform_param(
                timeline,
                0,
                a_id,
                TransformParam::ZoomX,
                2.0,
            )),
        );
        history.do_command(
            &mut project,
            Box::new(command::set_clip_gain(timeline, 0, a_id, -6.0)),
        );

        let zoom_x = |p: &Project| {
            p.timelines[timeline].tracks[0].clips[0]
                .effects
                .transform
                .track(TransformParam::ZoomX)
                .default
        };
        assert_eq!(zoom_x(&project), 2.0);
        assert_eq!(
            project.timelines[timeline].tracks[0].clips[0]
                .effects
                .gain_db
                .default,
            -6.0
        );

        history.undo(&mut project);
        assert_eq!(
            project.timelines[timeline].tracks[0].clips[0]
                .effects
                .gain_db
                .default,
            0.0
        );
        assert_eq!(zoom_x(&project), 2.0);

        history.undo(&mut project);
        assert_eq!(zoom_x(&project), 1.0);
    }

    /// Il reset di una sezione del pannello: i parametri tornano al
    /// default, keyframe compresi, e l'undo li rimette com'erano.
    #[test]
    fn reset_transform_params_clears_keyframes_and_undo_restores_them() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let clip = make_clip(&mut project, 0, 20);
        let a_id = clip.id;
        let mut history = History::default();
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::UpsertKeyframe::new(
                timeline,
                0,
                a_id,
                5,
                command::KeyframeValue::TransformParam(TransformParam::ZoomX, 3.0),
                Interpolation::Linear,
            )),
        );
        history.do_command(
            &mut project,
            Box::new(command::set_clip_flip(timeline, 0, a_id, [true, false])),
        );
        history.do_command(
            &mut project,
            Box::new(command::ResetTransformParams::new(
                timeline,
                0,
                a_id,
                vec![TransformParam::ZoomX],
                true,
            )),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert!(clip.effects.transform.is_constant(), "keyframe azzerati");
        assert_eq!(clip.effects.transform.value_at(5).zoom, [1.0, 1.0]);
        assert_eq!(clip.effects.transform.flip, [false, false]);

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.transform.value_at(5).zoom, [3.0, 1.0]);
        assert_eq!(clip.effects.transform.flip, [true, false]);
    }

    #[test]
    fn upsert_keyframe_gain_then_undo_removes_it_again() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::UpsertKeyframe::new(
                timeline,
                0,
                a_id,
                5,
                command::KeyframeValue::Gain(-6.0),
                Interpolation::Linear,
            )),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(
            clip.effects.gain_db.keyframe_at(5),
            Some((-6.0, Interpolation::Linear))
        );
        assert_eq!(clip.effects.gain_db.value_at(5), -6.0);

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert!(clip.effects.gain_db.is_constant());
    }

    #[test]
    fn upsert_keyframe_replacing_existing_one_undoes_to_old_value() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::UpsertKeyframe::new(
                timeline,
                0,
                a_id,
                5,
                command::KeyframeValue::Gain(-6.0),
                Interpolation::Linear,
            )),
        );
        history.do_command(
            &mut project,
            Box::new(command::UpsertKeyframe::new(
                timeline,
                0,
                a_id,
                5,
                command::KeyframeValue::Gain(3.0),
                Interpolation::Hold,
            )),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(
            clip.effects.gain_db.keyframe_at(5),
            Some((3.0, Interpolation::Hold))
        );

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(
            clip.effects.gain_db.keyframe_at(5),
            Some((-6.0, Interpolation::Linear)),
            "l'undo deve ripristinare il keyframe precedente, non rimuoverlo"
        );
    }

    /// Clip con due keyframe di gain, pronta per i comandi di gruppo.
    fn clip_with_two_gain_keyframes() -> (Project, History, TimelineId, ClipId) {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 0, clip: a }),
        );
        for (frame, value) in [(5, -6.0), (10, 3.0)] {
            history.do_command(
                &mut project,
                Box::new(command::UpsertKeyframe::new(
                    timeline,
                    0,
                    a_id,
                    frame,
                    command::KeyframeValue::Gain(value),
                    Interpolation::Linear,
                )),
            );
        }
        (project, history, timeline, a_id)
    }

    #[test]
    fn moving_keyframes_shifts_them_and_undo_puts_them_back() {
        let (mut project, mut history, timeline, clip_id) = clip_with_two_gain_keyframes();
        history.do_command(
            &mut project,
            Box::new(command::MoveKeyframes::new(
                timeline,
                0,
                clip_id,
                vec![(command::KeyframeTarget::Gain, 5), (command::KeyframeTarget::Gain, 10)],
                4,
            )),
        );

        let gain = &project.timelines[timeline].tracks[0].clips[0].effects.gain_db;
        assert_eq!(gain.keyframe_at(5), None);
        assert_eq!(gain.keyframe_at(9), Some((-6.0, Interpolation::Linear)));
        assert_eq!(gain.keyframe_at(14), Some((3.0, Interpolation::Linear)));

        history.undo(&mut project);
        let gain = &project.timelines[timeline].tracks[0].clips[0].effects.gain_db;
        assert_eq!(gain.keyframe_at(5), Some((-6.0, Interpolation::Linear)));
        assert_eq!(gain.keyframe_at(10), Some((3.0, Interpolation::Linear)));
        assert_eq!(gain.keyframes().len(), 2);
    }

    #[test]
    fn moving_a_keyframe_onto_another_restores_it_on_undo() {
        let (mut project, mut history, timeline, clip_id) = clip_with_two_gain_keyframes();
        history.do_command(
            &mut project,
            Box::new(command::MoveKeyframes::new(
                timeline,
                0,
                clip_id,
                vec![(command::KeyframeTarget::Gain, 5)],
                5,
            )),
        );

        let gain = &project.timelines[timeline].tracks[0].clips[0].effects.gain_db;
        assert_eq!(gain.keyframes().len(), 1, "quello di arrivo è stato sovrascritto");
        assert_eq!(gain.keyframe_at(10), Some((-6.0, Interpolation::Linear)));

        history.undo(&mut project);
        let gain = &project.timelines[timeline].tracks[0].clips[0].effects.gain_db;
        assert_eq!(gain.keyframe_at(5), Some((-6.0, Interpolation::Linear)));
        assert_eq!(gain.keyframe_at(10), Some((3.0, Interpolation::Linear)));
    }

    #[test]
    fn setting_interpolation_only_touches_the_picked_keyframes() {
        let (mut project, mut history, timeline, clip_id) = clip_with_two_gain_keyframes();
        history.do_command(
            &mut project,
            Box::new(command::SetKeyframeInterpolation::new(
                timeline,
                0,
                clip_id,
                vec![(command::KeyframeTarget::Gain, 5)],
                Interpolation::EaseIn,
            )),
        );

        let gain = &project.timelines[timeline].tracks[0].clips[0].effects.gain_db;
        assert_eq!(gain.keyframe_at(5), Some((-6.0, Interpolation::EaseIn)));
        assert_eq!(gain.keyframe_at(10), Some((3.0, Interpolation::Linear)));

        history.undo(&mut project);
        let gain = &project.timelines[timeline].tracks[0].clips[0].effects.gain_db;
        assert_eq!(gain.keyframe_at(5), Some((-6.0, Interpolation::Linear)));
    }

    #[test]
    fn remove_keyframe_then_undo_reinserts_it() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 20);
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::UpsertKeyframe::new(
                timeline,
                0,
                a_id,
                5,
                command::KeyframeValue::TransformParam(TransformParam::ZoomX, 2.0),
                Interpolation::Linear,
            )),
        );

        history.do_command(
            &mut project,
            Box::new(command::RemoveKeyframe::new(
                timeline,
                0,
                a_id,
                command::KeyframeTarget::TransformParam(TransformParam::ZoomX),
                5,
            )),
        );
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert!(clip.effects.transform.is_constant());

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(
            clip.effects
                .transform
                .track(TransformParam::ZoomX)
                .keyframe_at(5)
                .unwrap()
                .0,
            2.0
        );
    }

    fn white() -> Rgba {
        Rgba {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        }
    }

    #[test]
    fn set_clip_color_initializes_then_updates_then_undo_removes_entirely() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let mut a = make_clip(&mut project, 0, 10);
        a.source = ClipSource::SolidColor;
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );
        assert!(
            project.timelines[timeline].tracks[0].clips[0]
                .effects
                .color
                .is_none()
        );

        let red = Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        history.do_command(
            &mut project,
            Box::new(command::SetClipColor::new(timeline, 0, a_id, red)),
        );
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.color.as_ref().unwrap().default.r, 1.0);

        history.do_command(
            &mut project,
            Box::new(command::SetClipColor::new(timeline, 0, a_id, white())),
        );
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.color.as_ref().unwrap().default.r, 1.0);
        assert_eq!(clip.effects.color.as_ref().unwrap().default.g, 1.0);

        history.undo(&mut project); // torna a rosso
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.color.as_ref().unwrap().default.g, 0.0);

        history.undo(&mut project); // torna a "nessun colore"
        assert!(
            project.timelines[timeline].tracks[0].clips[0]
                .effects
                .color
                .is_none()
        );
    }

    #[test]
    fn upsert_and_remove_color_keyframe_round_trip() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let mut a = make_clip(&mut project, 0, 20);
        a.source = ClipSource::SolidColor;
        let a_id = a.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: a,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::SetClipColor::new(
                timeline,
                0,
                a_id,
                Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
            )),
        );

        history.do_command(
            &mut project,
            Box::new(command::UpsertKeyframe::new(
                timeline,
                0,
                a_id,
                10,
                command::KeyframeValue::Color(white()),
                Interpolation::Linear,
            )),
        );
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(
            clip.effects
                .color
                .as_ref()
                .unwrap()
                .keyframe_at(10)
                .unwrap()
                .0
                .r,
            1.0
        );

        history.do_command(
            &mut project,
            Box::new(command::RemoveKeyframe::new(
                timeline,
                0,
                a_id,
                command::KeyframeTarget::Color,
                10,
            )),
        );
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert!(clip.effects.color.as_ref().unwrap().is_constant());

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(
            clip.effects
                .color
                .as_ref()
                .unwrap()
                .keyframe_at(10)
                .unwrap()
                .0
                .r,
            1.0
        );
    }

    #[test]
    fn move_clips_moves_both_atomically_and_undo_restores_both() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let video = make_clip(&mut project, 0, 10);
        let video_id = video.id;
        let audio = make_clip(&mut project, 0, 10);
        let audio_id = audio.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: video,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 1,
                clip: audio,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::MoveClips::new(
                timeline,
                vec![(video_id, 0, 0, 40), (audio_id, 1, 1, 40)],
            )),
        );
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].timeline_start, 40);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 40);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].timeline_start, 0);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 0);
    }

    #[test]
    fn unlink_clip_dissolves_the_whole_group_and_undo_restores_it() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let mut video = make_clip(&mut project, 0, 10);
        let mut audio = make_clip(&mut project, 0, 10);
        let group = project.alloc_link_group_id();
        video.linked_group = Some(group);
        audio.linked_group = Some(group);
        let (video_id, audio_id) = (video.id, audio.id);

        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: video,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 1,
                clip: audio,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::UnlinkClip::new(timeline, 0, video_id)),
        );
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].linked_group, None);
        assert_eq!(tl.tracks[1].clips[0].linked_group, None);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].linked_group, Some(group));
        assert_eq!(tl.tracks[1].clips[0].linked_group, Some(group));
        assert_eq!(video_id, tl.tracks[0].clips[0].id);
        assert_eq!(audio_id, tl.tracks[1].clips[0].id);
    }

    #[test]
    fn unlink_clip_in_a_group_of_three_dissolves_all_three_not_just_the_clicked_one() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let mut a = make_clip(&mut project, 0, 10);
        let mut b = make_clip(&mut project, 0, 10);
        let mut c = make_clip(&mut project, 0, 10);
        let group = project.alloc_link_group_id();
        a.linked_group = Some(group);
        b.linked_group = Some(group);
        c.linked_group = Some(group);
        let (a_id, b_id, c_id) = (a.id, b.id, c.id);

        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 0, clip: a }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 0, clip: b }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 1, clip: c }),
        );

        // "Scollega" invocato su *a* soltanto: deve sciogliere l'intero
        // gruppo (bug segnalato: lasciava b e c ancora collegate tra loro).
        history.do_command(
            &mut project,
            Box::new(command::UnlinkClip::new(timeline, 0, a_id)),
        );
        let tl = &project.timelines[timeline];
        let find = |id: ClipId| tl.tracks.iter().flat_map(|t| &t.clips).find(|c| c.id == id).unwrap();
        assert_eq!(find(a_id).linked_group, None);
        assert_eq!(find(b_id).linked_group, None, "l'intero gruppo si scioglie, non solo a");
        assert_eq!(find(c_id).linked_group, None);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        let find = |id: ClipId| tl.tracks.iter().flat_map(|t| &t.clips).find(|c| c.id == id).unwrap();
        assert_eq!(find(a_id).linked_group, Some(group));
        assert_eq!(find(b_id).linked_group, Some(group));
        assert_eq!(find(c_id).linked_group, Some(group));
    }

    #[test]
    fn link_clips_groups_an_arbitrary_number_and_undo_restores_previous() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let video = make_clip(&mut project, 0, 10);
        let video_id = video.id;
        let audio_a = make_clip(&mut project, 0, 10);
        let audio_a_id = audio_a.id;
        let audio_b = make_clip(&mut project, 0, 10);
        let audio_b_id = audio_b.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 0, clip: video }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 1, clip: audio_a }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 1, clip: audio_b }),
        );

        history.do_command(
            &mut project,
            Box::new(command::LinkClips::new(
                timeline,
                vec![(0, video_id), (1, audio_a_id), (1, audio_b_id)],
            )),
        );
        let tl = &project.timelines[timeline];
        let find = |id: ClipId| tl.tracks.iter().flat_map(|t| &t.clips).find(|c| c.id == id).unwrap();
        let group = find(video_id).linked_group.expect("collegata");
        assert_eq!(find(audio_a_id).linked_group, Some(group));
        assert_eq!(find(audio_b_id).linked_group, Some(group));

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        let find = |id: ClipId| tl.tracks.iter().flat_map(|t| &t.clips).find(|c| c.id == id).unwrap();
        assert_eq!(find(video_id).linked_group, None);
        assert_eq!(find(audio_a_id).linked_group, None);
        assert_eq!(find(audio_b_id).linked_group, None);
    }

    #[test]
    fn split_clip_keeps_the_group_on_the_left_half_and_starts_the_right_half_unlinked() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let mut video = make_clip(&mut project, 0, 20);
        let group = project.alloc_link_group_id();
        video.linked_group = Some(group);
        let video_id = video.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: video,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::SplitClip::new(timeline, 0, video_id, 8)),
        );
        let tl = &project.timelines[timeline];
        assert_eq!(
            tl.tracks[0].clips[0].linked_group,
            Some(group),
            "la metà sinistra è la stessa clip di prima, accorciata: resta nel gruppo"
        );
        assert_eq!(
            tl.tracks[0].clips[1].linked_group, None,
            "la metà destra è una clip nuova, parte scollegata"
        );

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].linked_group, Some(group));
    }

    #[test]
    fn composite_command_applies_and_undoes_all_as_one_step() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let video = make_clip(&mut project, 0, 20);
        let video_id = video.id;
        let audio = make_clip(&mut project, 0, 20);
        let audio_id = audio.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: video,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 1,
                clip: audio,
            }),
        );

        // "Taglia tutto al frame 8": due SplitClip in un solo passo di history.
        history.do_command(
            &mut project,
            Box::new(command::CompositeCommand::new(CommandLabel::SplitClips, vec![
                Box::new(command::SplitClip::new(timeline, 0, video_id, 8)),
                Box::new(command::SplitClip::new(timeline, 1, audio_id, 8)),
            ])),
        );

        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips.len(), 2);

        // Un solo undo riporta indietro entrambi i tagli.
        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
    }

    #[test]
    fn ripple_gap_leaves_locked_tracks_where_they_are() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        for track_index in 0..2 {
            let clip = make_clip(&mut project, 20, 10);
            history.do_command(
                &mut project,
                Box::new(command::InsertClip { timeline, track_index, clip }),
            );
        }
        history.do_command(
            &mut project,
            Box::new(command::SetTrackFlag::new(timeline, 1, TrackFlag::Locked, true)),
        );

        history.do_command(&mut project, Box::new(command::RippleDeleteGap::new(timeline, 0, 20)));
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].timeline_start, 0);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 20, "track bloccata");

        history.undo(&mut project);
        history.undo(&mut project);
        assert!(!project.timelines[timeline].tracks[1].locked);
    }

    #[test]
    fn disabling_clips_is_undoable_and_hides_them_from_compositing() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let clip = make_clip(&mut project, 0, 10);
        let id = clip.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 0, clip }),
        );

        history.do_command(
            &mut project,
            Box::new(command::SetClipsDisabled::new(timeline, vec![(0, id)], true)),
        );
        assert!(project.timelines[timeline].active_video_clips_at(5).is_empty());

        history.undo(&mut project);
        assert_eq!(project.timelines[timeline].active_video_clips_at(5).len(), 1);

        history.do_command(
            &mut project,
            Box::new(command::SetTrackFlag::new(timeline, 0, TrackFlag::Muted, true)),
        );
        assert!(
            project.timelines[timeline].active_video_clips_at(5).is_empty(),
            "track video disattivata"
        );
    }

    #[test]
    fn set_clip_fade_is_undoable_and_clamped_to_clip_length() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let clip = make_clip(&mut project, 0, 10);
        let id = clip.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 0, clip }),
        );

        history.do_command(
            &mut project,
            Box::new(command::SetClipFade::new(timeline, 0, id, command::FadeEdge::In, 4)),
        );
        history.do_command(
            &mut project,
            Box::new(command::SetClipFade::new(timeline, 0, id, command::FadeEdge::Out, 999)),
        );
        fn find<'p>(p: &'p Project, timeline: TimelineId, id: ClipId) -> &'p Clip {
            p.timelines[timeline].clip(0, id).unwrap()
        }
        assert_eq!(find(&project, timeline, id).fade_in, 4);
        assert_eq!(find(&project, timeline, id).fade_out, 10, "oltre la durata della clip si clampa");

        history.undo(&mut project);
        assert_eq!(find(&project, timeline, id).fade_out, 0);
        history.undo(&mut project);
        assert_eq!(find(&project, timeline, id).fade_in, 0);
    }

    #[test]
    fn fade_multiplier_ramps_in_then_plateaus_then_ramps_out() {
        let mut clip = Clip::from_source_range(
            ClipId(0),
            ClipSource::Media(MediaId::default()),
            0,
            100,
            0,
            Rational::one(),
        );
        clip.fade_in = 20;
        clip.fade_out = 20;
        assert_eq!(clip.fade_multiplier_at(0), 0.0);
        assert!((clip.fade_multiplier_at(10) - 0.5).abs() < 1e-6);
        assert_eq!(clip.fade_multiplier_at(20), 1.0);
        assert_eq!(clip.fade_multiplier_at(50), 1.0);
        assert!((clip.fade_multiplier_at(90) - 0.5).abs() < 1e-6);
        assert_eq!(clip.fade_multiplier_at(100), 0.0);
    }

    #[test]
    fn set_clip_transition_is_undoable() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();
        let clip = make_clip(&mut project, 0, 10);
        let id = clip.id;
        history.do_command(
            &mut project,
            Box::new(command::InsertClip { timeline, track_index: 0, clip }),
        );
        let transition = Transition {
            kind: TransitionKind::Push,
            duration: 5,
            direction: PushDirection::Right,
            ease: Ease::None,
            curve: 0.5,
        };
        history.do_command(
            &mut project,
            Box::new(set_clip_transition(timeline, 0, id, FadeEdge::In, Some(transition.clone()))),
        );
        fn find<'p>(p: &'p Project, timeline: TimelineId, id: ClipId) -> &'p Clip {
            p.timelines[timeline].clip(0, id).unwrap()
        }
        assert_eq!(find(&project, timeline, id).effects.transition_in, Some(transition));
        assert!(find(&project, timeline, id).effects.transition_out.is_none());

        history.undo(&mut project);
        assert!(find(&project, timeline, id).effects.transition_in.is_none());
    }

    #[test]
    fn transition_offset_slides_in_from_the_push_direction_then_settles() {
        let mut clip = Clip::from_source_range(
            ClipId(0),
            ClipSource::Media(MediaId::default()),
            0,
            100,
            0,
            Rational::one(),
        );
        clip.effects.transition_in = Some(Transition {
            kind: TransitionKind::Push,
            duration: 20,
            direction: PushDirection::Right,
            ease: Ease::None,
            curve: 0.0,
        });
        let frame_size = (1920.0, 1080.0);
        let zoom = [1.0, 1.0];
        // All'inizio della clip: fuori schermo dal lato opposto a quello
        // d'arrivo ("Right" è il verso con cui il contenuto raggiunge il
        // centro).
        assert_eq!(clip.transition_offset_at(0, frame_size, zoom), [-1920.0, 0.0]);
        let mid = clip.transition_offset_at(10, frame_size, zoom);
        assert!((mid[0] - (-960.0)).abs() < 1.0);
        // Finita la transizione: a posto, nessun offset residuo.
        assert_eq!(clip.transition_offset_at(20, frame_size, zoom), [0.0, 0.0]);
        assert_eq!(clip.transition_offset_at(50, frame_size, zoom), [0.0, 0.0]);
    }

    #[test]
    fn cross_transition_window_straddles_the_cut_and_progresses_from_zero_to_one() {
        let left = Clip::from_source_range(ClipId(1), ClipSource::Media(MediaId::default()), 0, 100, 0, Rational::one());
        // Adiacente: comincia esattamente dove finisce `left` (100).
        let right = Clip::from_source_range(ClipId(2), ClipSource::Media(MediaId::default()), 0, 100, 100, Rational::one());
        let crossing = CrossTransition {
            left_clip: left.id,
            right_clip: right.id,
            transition: Transition {
                kind: TransitionKind::Push,
                duration: 20,
                direction: PushDirection::Right,
                ease: Ease::None,
                curve: 0.0,
            },
        };
        // Finestra simmetrica sul taglio: 10 frame prima, 10 dopo.
        assert_eq!(crossing.window(&left, &right), 90..110);
        assert_eq!(crossing.eased_progress_at(90, &left, &right), 0.0);
        assert!((crossing.eased_progress_at(100, &left, &right) - 0.5).abs() < 1e-6);
        assert_eq!(crossing.eased_progress_at(110, &left, &right), 1.0);

        let frame_size = (1920.0, 1080.0);
        // A progresso 0: sinistra del tutto a posto, destra del tutto fuori
        // (dal lato opposto a "Right", da cui arriva).
        let zoom = [1.0, 1.0];
        let (left_off, right_off) = crossing.offsets(0.0, frame_size, zoom, zoom);
        assert_eq!(left_off, [0.0, 0.0]);
        assert_eq!(right_off, [-1920.0, 0.0]);
        // A progresso 1: l'opposto.
        let (left_off, right_off) = crossing.offsets(1.0, frame_size, zoom, zoom);
        assert_eq!(left_off, [1920.0, 0.0]);
        assert_eq!(right_off, [0.0, 0.0]);
    }
}

#[cfg(test)]
mod compound_clip_tests {
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

    fn insert_clip(project: &mut Project, timeline: TimelineId, track_index: usize, start: FrameIdx, len: FrameIdx) -> ClipId {
        let clip = Clip::from_source_range(project.alloc_clip_id(), ClipSource::SolidColor, 0, len, start, Rational::one());
        let id = clip.id;
        project.timelines[timeline].tracks[track_index].insert_sorted(clip);
        id
    }

    /// Inserisce nel pool una compound clip che referenzia `plan`, come farebbe
    /// il chiamante reale (main.rs) prima di girare `compound_clip_commands`.
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
        assert_eq!(plan.len, 40); // fino a 50 (fine della clip audio), da 10
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
        assert_eq!(plan.video_track, Some(0), "la track più in basso fra quelle coinvolte");
        assert_eq!(plan.audio_track, None);
        assert_eq!(plan.nested_timeline.tracks.len(), 2, "una track annidata per ogni track video coinvolta");
    }

    #[test]
    fn deleting_a_compound_clip_frees_its_name_and_its_nested_timeline() {
        let (mut project, timeline) = project_with_tracks(&[TrackKind::Video]);
        let id = insert_clip(&mut project, timeline, 0, 0, 20);
        let plan = plan_compound_clip(&project, timeline, &[(0, id)]).unwrap();
        let media = insert_compound_media(&mut project, &plan);
        assert_eq!(project.media_pool[media].path.to_str(), Some("Compound Clip 1"));

        let mut history = History::default();
        history.do_command(&mut project, Box::new(command::RemoveMedia::new(media)));
        assert_eq!(project.timelines.len(), 1, "la timeline annidata sparisce col media");

        let again = insert_compound_media(&mut project, &plan);
        assert_eq!(project.media_pool[again].path.to_str(), Some("Compound Clip 1"));
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
        let nested = project.media_pool.values().next().unwrap().compound.unwrap();
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
            Box::new(CompositeCommand::new(CommandLabel::MakeCompoundClip, commands)),
        );

        let video_track = &project.timelines[timeline].tracks[0];
        assert_eq!(video_track.clips.len(), 1);
        assert!(matches!(video_track.clips[0].source, ClipSource::Media(id) if id == media_id));
        assert_eq!(video_track.clips[0].timeline_start, 10);
        assert_eq!(video_track.clips[0].timeline_len, 40);

        let audio_track = &project.timelines[timeline].tracks[1];
        assert_eq!(audio_track.clips.len(), 1);
        assert!(matches!(audio_track.clips[0].source, ClipSource::Media(id) if id == media_id));
        let group = video_track.clips[0].linked_group.expect("video collegato all'audio");
        assert_eq!(audio_track.clips[0].linked_group, Some(group));

        // Il pool e la timeline annidata restano fuori dalla history.
        assert_eq!(project.media_pool.len(), 1);
        assert_eq!(project.timelines.len(), 2);

        history.undo(&mut project);
        assert_eq!(project.timelines[timeline].tracks[0].clips.len(), 1);
        assert_eq!(project.timelines[timeline].tracks[0].clips[0].id, video_id);
        assert_eq!(project.timelines[timeline].tracks[0].clips[0].timeline_start, 10);
        assert_eq!(project.timelines[timeline].tracks[1].clips[0].id, audio_id);
        // Undo non de-importa la compound clip dal pool, come un import.
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

        assert!(project.would_create_a_cycle(media, timeline), "T dentro se stessa");
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
        // B contiene già una clip che referenzia A: importare B dentro A
        // chiuderebbe il ciclo A -> B -> A.
        project.timelines[b].tracks.push(Track {
            kind: TrackKind::Video,
            clips: vec![Clip::from_source_range(ClipId(1), ClipSource::Media(media_a), 0, 10, 0, Rational::one())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        });

        assert!(project.would_create_a_cycle(media_b, a), "A -> B -> A");
        assert!(!project.would_create_a_cycle(media_a, b), "B -> A non ancora un ciclo su B");
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
}
