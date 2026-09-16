pub mod command;
pub mod model;
pub mod persistence;

pub use command::{
    AddTrack, Command, CompositeCommand, History, InsertClip, KeyframeTarget, KeyframeValue,
    LiftDelete, LinkClips, MoveClip, MoveClips, RemoveKeyframe, RemoveTrack, RippleDeleteAllTracks,
    RippleDeleteGap, SetClipColor, SetClipGain, SetClipTransform, SplitClip, TrimClip, TrimEdge,
    UnlinkClip, UpsertKeyframe,
};
pub use model::*;
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
        Clip {
            id: project.alloc_clip_id(),
            source: ClipSource::SolidColor,
            source_in: 0,
            source_out: len,
            timeline_start: start,
            effects: EffectStack::default(),
            linked: None,
            audio_stream_index: 0,
        }
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
    fn ripple_delete_shifts_all_tracks_and_undo_restores() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let a = make_clip(&mut project, 0, 10);
        let b = make_clip(&mut project, 10, 10);
        let a_audio = make_clip(&mut project, 0, 10);
        let b_audio = make_clip(&mut project, 10, 10);
        let b_id = b.id;

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
            Box::new(command::InsertClip {
                timeline,
                track_index: 0,
                clip: b,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 1,
                clip: a_audio,
            }),
        );
        history.do_command(
            &mut project,
            Box::new(command::InsertClip {
                timeline,
                track_index: 1,
                clip: b_audio,
            }),
        );

        history.do_command(
            &mut project,
            Box::new(command::RippleDeleteAllTracks::new(timeline, 0, b_id)),
        );

        // La clip "b" video è sparita, e la clip audio corrispondente
        // (che parte allo stesso tempo) si è spostata a 0 anche se sta su
        // un'altra track: questo è il comportamento "ripple all tracks".
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 0);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 0);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 0);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 10);
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
            Box::new(command::MoveClip::new(timeline, a_id, 0, 1, 25)),
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

        // Trimma il bordo sinistro: source_in passa da 0 a 5.
        history.do_command(
            &mut project,
            Box::new(command::TrimClip::new(
                timeline,
                0,
                a_id,
                TrimEdge::Start,
                5,
            )),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_in, 5);
        assert_eq!(clip.timeline_start, 15, "si sposta della stessa quantità");
        assert_eq!(
            clip.timeline_end(),
            original_end,
            "la fine sulla timeline resta ferma"
        );

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_in, 0);
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
            Box::new(command::TrimClip::new(timeline, 0, a_id, TrimEdge::End, 15)),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_out, 15);
        assert_eq!(
            clip.timeline_start, 10,
            "l'inizio sulla timeline resta fermo"
        );
        assert_eq!(clip.timeline_end(), 25);

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.source_out, 20);
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
        assert_eq!(first.source_out, 8);
        assert_eq!(second.timeline_start, 8);
        assert_eq!(second.source_in, 8);
        assert_eq!(second.source_out, 20);
        assert_ne!(first.id, second.id);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].source_out, 20);
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

        let new_transform = Transform {
            crop: [0.1, 0.1, 0.9, 0.9],
            zoom: 2.0,
            position: [0.1, -0.1],
        };
        history.do_command(
            &mut project,
            Box::new(command::SetClipTransform::new(
                timeline,
                0,
                a_id,
                new_transform,
            )),
        );
        history.do_command(
            &mut project,
            Box::new(command::SetClipGain::new(timeline, 0, a_id, -6.0)),
        );

        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.transform.default.zoom, 2.0);
        assert_eq!(clip.effects.gain_db.default, -6.0);

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.gain_db.default, 0.0);
        assert_eq!(clip.effects.transform.default.zoom, 2.0);

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.transform.default.zoom, 1.0);
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
                command::KeyframeValue::Transform(Transform {
                    zoom: 2.0,
                    ..Transform::default()
                }),
                Interpolation::Linear,
            )),
        );

        history.do_command(
            &mut project,
            Box::new(command::RemoveKeyframe::new(
                timeline,
                0,
                a_id,
                command::KeyframeTarget::Transform,
                5,
            )),
        );
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert!(clip.effects.transform.is_constant());

        history.undo(&mut project);
        let clip = &project.timelines[timeline].tracks[0].clips[0];
        assert_eq!(clip.effects.transform.keyframe_at(5).unwrap().0.zoom, 2.0);
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
    fn unlink_clip_clears_both_sides_and_undo_restores_both() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let mut video = make_clip(&mut project, 0, 10);
        let mut audio = make_clip(&mut project, 0, 10);
        video.linked = Some(audio.id);
        audio.linked = Some(video.id);
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
        assert_eq!(tl.tracks[0].clips[0].linked, None);
        assert_eq!(tl.tracks[1].clips[0].linked, None);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].linked, Some(audio_id));
        assert_eq!(tl.tracks[1].clips[0].linked, Some(video_id));
    }

    #[test]
    fn link_clips_sets_both_sides_and_undo_restores_previous() {
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
            Box::new(command::LinkClips::new(
                timeline,
                (0, video_id),
                (1, audio_id),
            )),
        );
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].linked, Some(audio_id));
        assert_eq!(tl.tracks[1].clips[0].linked, Some(video_id));

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].linked, None);
        assert_eq!(tl.tracks[1].clips[0].linked, None);
    }

    #[test]
    fn split_clip_clears_link_on_both_halves_and_undo_restores_it() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let mut video = make_clip(&mut project, 0, 20);
        video.linked = Some(ClipId(999)); // gemella fittizia, non serve che esista per questo test
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
        assert_eq!(tl.tracks[0].clips[0].linked, None);
        assert_eq!(tl.tracks[0].clips[1].linked, None);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips[0].linked, Some(ClipId(999)));
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
            Box::new(command::CompositeCommand::new(vec![
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
    fn ripple_delete_with_also_remove_removes_linked_clip_without_double_shift() {
        let (mut project, timeline) = make_project_with_two_tracks();
        let mut history = History::default();

        let video_a = make_clip(&mut project, 0, 10);
        let video_b = make_clip(&mut project, 10, 10); // da rimuovere
        let video_c = make_clip(&mut project, 20, 10);
        let (video_a_id, video_b_id, video_c_id) = (video_a.id, video_b.id, video_c.id);

        let audio_a = make_clip(&mut project, 0, 10);
        let audio_b = make_clip(&mut project, 10, 10); // gemella di video_b, anche lei da rimuovere
        let audio_c = make_clip(&mut project, 20, 10);
        let audio_b_id = audio_b.id;

        for (track_index, clip) in [
            (0, video_a),
            (0, video_b),
            (0, video_c),
            (1, audio_a),
            (1, audio_b),
            (1, audio_c),
        ] {
            history.do_command(
                &mut project,
                Box::new(command::InsertClip {
                    timeline,
                    track_index,
                    clip,
                }),
            );
        }

        history.do_command(
            &mut project,
            Box::new(
                command::RippleDeleteAllTracks::new(timeline, 0, video_b_id)
                    .with_also_remove(vec![(1, audio_b_id)]),
            ),
        );

        let tl = &project.timelines[timeline];
        // Entrambe le clip "b" sono sparite, non solo shiftate.
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips.len(), 2);
        // Lo shift è di UNA sola lunghezza di gap (10), non doppio (20):
        // altrimenti "c" finirebbe a 10 invece che a 20-10=10... la prova
        // vera è che "c" atterra esattamente dove stava "b" (gap chiuso
        // una volta sola), su entrambe le track.
        assert_eq!(tl.tracks[0].clips[0].id, video_a_id);
        assert_eq!(tl.tracks[0].clips[1].id, video_c_id);
        assert_eq!(tl.tracks[0].clips[1].timeline_start, 10);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 10);

        history.undo(&mut project);
        let tl = &project.timelines[timeline];
        assert_eq!(tl.tracks[0].clips.len(), 3);
        assert_eq!(tl.tracks[1].clips.len(), 3);
        assert_eq!(tl.tracks[0].clips[2].timeline_start, 20);
        assert_eq!(tl.tracks[1].clips[2].timeline_start, 20);
    }
}
