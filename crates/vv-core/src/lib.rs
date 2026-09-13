pub mod command;
pub mod model;

pub use command::{
    Command, History, InsertClip, LiftDelete, MoveClip, RippleDeleteAllTracks, SplitClip,
};
pub use model::*;

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
        }
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
}
