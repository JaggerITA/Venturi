use super::*;
use crate::export::{ExportError, ExportSettings};
use crate::relink_job::RelinkRequest;
use crate::{RelinkEnd, SessionEvent, Waker};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use vv_core::edit::{self, MediaInsert, RangeDelete, TargetTracks};
use vv_core::{FrameIdx, MediaId, Rational, Timeline, Track, TrackKind};

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vv-session-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 2 s of 64x48 video at 25 fps with a sine track.
fn clip_file(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=25:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ],
        &path,
    );
    path
}

/// Ticks until no job is left, returning every event.
fn run_jobs(session: &mut Session) -> Vec<SessionEvent> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut events = Vec::new();
    loop {
        events.extend(session.tick());
        if !session.has_running_jobs() {
            return events;
        }
        assert!(std::time::Instant::now() < deadline, "jobs never finished");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn add_timeline(session: &mut Session) -> TimelineId {
    let id = session.project.timelines.insert(Timeline {
        name: "Timeline 1".into(),
        fps: Rational::new(25, 1),
        resolution: (64, 48),
        tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        markers: Vec::new(),
    });
    session.project.insert_timeline_item(id, None);
    id
}

fn insert_whole(session: &mut Session, timeline: TimelineId, media: MediaId, start: FrameIdx) {
    let len = session.project.media_pool[media].meta.duration_frames;
    edit::insert_media(
        &mut session.project,
        &mut session.history,
        timeline,
        MediaInsert {
            media_id: media,
            source_in: 0,
            source_out: len,
            video: true,
            audio: true,
        },
        start,
        TargetTracks {
            video: Some(0),
            extra_audio: None,
        },
    )
    .unwrap();
}

#[test]
fn import_adds_the_media_in_order_and_reports_the_failures() {
    let dir = test_dir("import");
    let a = clip_file(&dir, "a.mp4");
    let b = clip_file(&dir, "b.mp4");
    let wakes = Arc::new(AtomicUsize::new(0));
    let mut session = Session::default();
    session.set_waker(Waker::new({
        let wakes = wakes.clone();
        move || {
            wakes.fetch_add(1, Ordering::Relaxed);
        }
    }));

    let job = session.import_media(vec![a.clone(), dir.join("missing.mp4"), b, a]);
    let events = run_jobs(&mut session);

    assert!(matches!(events[0], SessionEvent::ImportStarted { job: j } if j == job));
    let added: Vec<MediaId> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::MediaAdded { media_id, .. } => Some(*media_id),
            _ => None,
        })
        .collect();
    let names: Vec<String> = added
        .iter()
        .map(|&id| crate::file_label(&session.project.media_pool[id].path))
        .collect();
    assert_eq!(names, ["a.mp4", "b.mp4"]);
    let Some(SessionEvent::ImportFinished {
        imported, errors, ..
    }) = events.last()
    else {
        panic!("the import must finish last");
    };
    assert_eq!(imported, &added);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].starts_with("missing.mp4: "), "{errors:?}");
    assert!(session.has_unsaved_changes());
    assert!(wakes.load(Ordering::Relaxed) > 0);
}

#[test]
fn importing_media_already_in_the_pool_finishes_without_starting() {
    let dir = test_dir("reimport");
    let a = clip_file(&dir, "a.mp4");
    let mut session = Session::default();
    session.import_media(vec![a.clone()]);
    run_jobs(&mut session);

    let job = session.import_media(vec![a]);
    let events = run_jobs(&mut session);

    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        SessionEvent::ImportFinished { job: j, imported, errors }
            if *j == job && imported.is_empty() && errors.is_empty()
    ));
}

#[test]
fn saving_and_opening_follow_the_unsaved_state() {
    let dir = test_dir("save");
    let mut session = Session::default();
    assert!(!session.has_unsaved_changes());
    let timeline = add_timeline(&mut session);
    session.mark_unsaved();
    edit::insert_generator(
        &mut session.project,
        &mut session.history,
        timeline,
        edit::Generator::Text,
        0,
        0,
        None,
    );

    let path = dir.join("p.vvproj");
    session.save_to(&path).unwrap();
    assert!(!session.has_unsaved_changes());
    assert_eq!(session.path(), Some(path.as_path()));

    let mut reopened = Session::default();
    reopened.open(&path).unwrap();
    assert!(!reopened.has_unsaved_changes());
    assert_eq!(reopened.path(), Some(path.as_path()));
    assert_eq!(reopened.project.timelines.len(), 1);
    assert!(reopened.open(&dir.join("nope.vvproj")).is_err());
    assert_eq!(
        reopened.path(),
        Some(path.as_path()),
        "a failed open keeps the project"
    );

    reopened.new_project();
    assert!(reopened.project.timelines.is_empty());
    assert_eq!(reopened.path(), None);
}

#[test]
fn import_edit_and_export_without_a_ui() {
    let dir = test_dir("end-to-end");
    let a = clip_file(&dir, "a.mp4");
    let mut session = Session::default();
    session.import_media(vec![a]);
    let media = match run_jobs(&mut session).pop() {
        Some(SessionEvent::ImportFinished { imported, .. }) => imported[0],
        _ => panic!("import expected"),
    };
    let timeline = add_timeline(&mut session);
    insert_whole(&mut session, timeline, media, 0);
    let mark = session.history.begin_group();
    edit::delete_ranges(
        &mut session.project,
        &mut session.history,
        timeline,
        &[(10, 20), (30, 40)],
        &RangeDelete::Ripple,
    );
    session.history.end_group(mark);
    session.sync_timeline_media(timeline);
    let total = session.project.timelines[timeline].total_frames();
    assert_eq!(total, 30);
    let item = session
        .project
        .media_pool
        .values()
        .find(|item| item.compound == Some(timeline))
        .unwrap();
    assert_eq!(item.meta.duration_frames, 30);

    let output = dir.join("out.mp4");
    let (job, progress) = session
        .export(timeline, ExportSettings::new(output.clone()), 0..total)
        .unwrap();
    let events = run_jobs(&mut session);

    assert!(matches!(
        events.as_slice(),
        [SessionEvent::ExportFinished { job: j, result: Ok(()) }] if *j == job
    ));
    assert!(progress.lock().unwrap().done);
    let frames = vv_media::probe(&output).unwrap().duration_frames;
    assert!((29..=30).contains(&frames), "frames={frames}");
}

#[test]
fn a_cancelled_export_reports_it() {
    let dir = test_dir("export-cancel");
    let mut session = Session::default();
    let timeline = add_timeline(&mut session);
    edit::insert_generator(
        &mut session.project,
        &mut session.history,
        timeline,
        edit::Generator::SolidColor,
        0,
        0,
        None,
    );
    let (job, progress) = session
        .export(timeline, ExportSettings::new(dir.join("out.mp4")), 0..125)
        .unwrap();
    assert!(
        session
            .export(timeline, ExportSettings::new(dir.join("other.mp4")), 0..125)
            .is_none(),
        "one export at a time"
    );
    assert!(session.cancel_export(job));
    let events = run_jobs(&mut session);

    // A very fast machine may finish before seeing the flag.
    match events.as_slice() {
        [SessionEvent::ExportFinished { result: Err(e), .. }] => {
            assert_eq!(*e, ExportError::Cancelled);
            assert_eq!(progress.lock().unwrap().error, Some(ExportError::Cancelled));
        }
        [SessionEvent::ExportFinished { result: Ok(()), .. }] => {}
        _ => panic!("one ExportFinished expected"),
    }
}

#[test]
fn relink_by_name_is_one_undo_step() {
    let dir = test_dir("relink");
    let moved = dir.join("moved");
    std::fs::create_dir_all(&moved).unwrap();
    let a = clip_file(&dir, "a.mp4");
    let mut session = Session::default();
    session.import_media(vec![a.clone()]);
    run_jobs(&mut session);
    let media = session.media_with_path(&a).unwrap();
    std::fs::rename(&a, moved.join("a.mp4")).unwrap();

    let job = session
        .relink(RelinkRequest::ByName {
            base_dir: dir.clone(),
            targets: vec![(media, a.clone())],
        })
        .unwrap();
    let events = run_jobs(&mut session);

    let [
        SessionEvent::RelinkFinished {
            job: j,
            end:
                RelinkEnd::Done {
                    relinked,
                    not_found,
                },
        },
    ] = events.as_slice()
    else {
        panic!("one finished relink expected");
    };
    assert_eq!(*j, job);
    assert_eq!(relinked, &[media]);
    assert!(not_found.is_none());
    assert_eq!(session.project.media_pool[media].path, moved.join("a.mp4"));
    session.history.undo(&mut session.project);
    assert_eq!(session.project.media_pool[media].path, a);
}

#[test]
fn otio_import_asks_before_reusing_media_with_the_same_name() {
    let dir = test_dir("otio");
    let a = clip_file(&dir, "a.mp4");
    let mut source = Session::default();
    source.import_media(vec![a]);
    let media = match run_jobs(&mut source).pop() {
        Some(SessionEvent::ImportFinished { imported, .. }) => imported[0],
        _ => panic!("import expected"),
    };
    let timeline = add_timeline(&mut source);
    insert_whole(&mut source, timeline, media, 0);
    let otio = dir.join("edit.otio");
    vv_core::export_otio(&source.project, timeline, &otio, None).unwrap();

    let mut fresh = Session::default();
    fresh.import_otio(&otio).unwrap();
    let events = run_jobs(&mut fresh);
    let [SessionEvent::OtioImported { result, .. }] = events.as_slice() else {
        panic!("no media to share: imported right away");
    };
    assert_eq!(result.timelines.len(), 1);
    assert_eq!(fresh.project.timelines[result.timelines[0]].name, "edit");
    assert!(result.folder.is_some());

    let pool_len = source.project.media_pool.len();
    source.import_otio(&otio).unwrap();
    assert!(
        source.import_otio(&otio).is_none(),
        "one OTIO import at a time"
    );
    let events = run_jobs(&mut source);
    assert!(matches!(
        events.as_slice(),
        [SessionEvent::OtioNeedsDecision { .. }]
    ));
    assert_eq!(source.otio_awaiting_decision(), Some(("edit", 1)));
    let result = source.finish_otio_import(Some(true)).unwrap();
    assert!(result.added_media.len() == 1, "only the timeline item");
    assert_eq!(source.project.media_pool.len(), pool_len + 1);
    assert!(source.otio_awaiting_decision().is_none());
}
