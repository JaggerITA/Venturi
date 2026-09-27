use super::*;
use crate::{Rational, Timeline, Track};

fn project() -> (Project, TimelineId) {
    let mut project = Project::default();
    let timeline = project.timelines.insert(Timeline {
        name: "T".into(),
        fps: Rational::new(25, 1),
        resolution: (64, 48),
        tracks: vec![Track::new(TrackKind::Video)],
        markers: Vec::new(),
    });
    (project, timeline)
}

fn add_track(timeline: TimelineId) -> Box<dyn Command> {
    Box::new(AddTrack::new(timeline, TrackKind::Audio))
}

fn tracks(project: &Project, timeline: TimelineId) -> usize {
    project.timelines[timeline].tracks.len()
}

#[test]
fn joined_commands_are_one_step() {
    let (mut project, tl) = project();
    let mut history = History::default();
    let label = CommandLabel::RemoveMedia;

    let step = history.join(&mut project, None, label, add_track(tl));
    let step = history.join(&mut project, Some(step), label, add_track(tl));
    history.join(&mut project, Some(step), label, add_track(tl));

    assert_eq!(history.position(), 1);
    assert_eq!(history.labels().collect::<Vec<_>>(), vec![label]);
    history.undo(&mut project);
    assert_eq!(tracks(&project, tl), 1);
    history.redo(&mut project);
    assert_eq!(tracks(&project, tl), 4);
}

#[test]
fn each_join_is_a_change() {
    let (mut project, tl) = project();
    let mut history = History::default();

    let step = history.join(&mut project, None, CommandLabel::AddTrack, add_track(tl));
    let generation = history.generation();
    history.join(
        &mut project,
        Some(step),
        CommandLabel::AddTrack,
        add_track(tl),
    );

    assert!(history.generation() > generation);
}

#[test]
fn a_command_in_between_starts_a_new_step() {
    let (mut project, tl) = project();
    let mut history = History::default();

    let step = history.join(&mut project, None, CommandLabel::RemoveMedia, add_track(tl));
    history.do_command(&mut project, add_track(tl));
    history.join(
        &mut project,
        Some(step),
        CommandLabel::RemoveMedia,
        add_track(tl),
    );

    assert_eq!(history.position(), 3);
}

#[test]
fn an_undo_in_between_starts_a_new_step_and_drops_the_redo() {
    let (mut project, tl) = project();
    let mut history = History::default();

    let step = history.join(&mut project, None, CommandLabel::RemoveMedia, add_track(tl));
    history.undo(&mut project);
    history.join(
        &mut project,
        Some(step),
        CommandLabel::RemoveMedia,
        add_track(tl),
    );

    assert_eq!(history.position(), 1);
    assert_eq!(history.labels().count(), 1);
    assert_eq!(tracks(&project, tl), 2);
}

#[test]
fn a_group_closed_around_the_step_ends_it() {
    let (mut project, tl) = project();
    let mut history = History::default();

    let mark = history.begin_group();
    history.do_command(&mut project, add_track(tl));
    let step = history.join(&mut project, None, CommandLabel::RemoveMedia, add_track(tl));
    history.end_group_as(mark, CommandLabel::Gain);
    history.join(
        &mut project,
        Some(step),
        CommandLabel::RemoveMedia,
        add_track(tl),
    );

    assert_eq!(
        history.labels().collect::<Vec<_>>(),
        vec![CommandLabel::Gain, CommandLabel::RemoveMedia]
    );
    history.undo(&mut project);
    assert_eq!(tracks(&project, tl), 3);
}

#[test]
fn only_the_latest_handle_joins() {
    let (mut project, tl) = project();
    let mut history = History::default();

    let first = history.join(&mut project, None, CommandLabel::RemoveMedia, add_track(tl));
    history.join(
        &mut project,
        Some(first),
        CommandLabel::RemoveMedia,
        add_track(tl),
    );
    history.join(
        &mut project,
        Some(first),
        CommandLabel::RemoveMedia,
        add_track(tl),
    );

    assert_eq!(history.position(), 2);
}

#[test]
fn a_joinable_step_never_absorbs_a_plain_composite() {
    let (mut project, tl) = project();
    let mut history = History::default();

    let step = history.join(&mut project, None, CommandLabel::RemoveMedia, add_track(tl));
    history.undo(&mut project);
    history.do_command(
        &mut project,
        Box::new(CompositeCommand::new(
            CommandLabel::Gain,
            vec![add_track(tl)],
        )),
    );
    history.join(
        &mut project,
        Some(step),
        CommandLabel::RemoveMedia,
        add_track(tl),
    );

    assert_eq!(history.position(), 2);
}

#[test]
fn an_explicit_label_renames_a_group_of_one_step() {
    let (mut project, tl) = project();
    let mut history = History::default();

    let mark = history.begin_group();
    history.do_command(&mut project, add_track(tl));
    history.end_group_as(mark, CommandLabel::InsertClips);

    assert_eq!(
        history.labels().collect::<Vec<_>>(),
        vec![CommandLabel::InsertClips]
    );
    history.undo(&mut project);
    assert_eq!(tracks(&project, tl), 1);
}
