use super::*;
use crate::dispatch::tests::{clip_file, create, error, import, ok, test_dir};
use crate::{ToolCall, dispatch};

/// A session with an empty timeline (V1, A1); returns its id.
fn with_timeline(session: &mut Session) -> String {
    ok(session, ToolCall::CreateTimeline(create("T")))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn solid(session: &mut Session, timeline: &str, at: i64, duration: i64) -> String {
    let result = ok(
        session,
        ToolCall::AddSolidColor(AddSolidColorArgs {
            timeline_id: timeline.into(),
            at,
            duration: Some(duration),
            track: None,
            color: None,
        }),
    );
    result["clips"][0]["id"].as_str().unwrap().to_owned()
}

fn tracks(session: &mut Session, timeline: &str) -> Value {
    ok(
        session,
        ToolCall::GetTimeline(TimelineArgs {
            timeline_id: timeline.into(),
        }),
    )["tracks"]
        .clone()
}

/// `(start, end)` of the clips of a track.
fn spans(session: &mut Session, timeline: &str, track: usize) -> Vec<(i64, i64)> {
    tracks(session, timeline)[track]["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["start"].as_i64().unwrap(), c["end"].as_i64().unwrap()))
        .collect()
}

fn lock(session: &mut Session, timeline: &str, track: &str) {
    ok(
        session,
        ToolCall::SetTrack(SetTrackArgs {
            timeline_id: timeline.into(),
            track: track.into(),
            muted: None,
            solo: None,
            locked: Some(true),
        }),
    );
}

fn insert(timeline: &str, media: &str) -> InsertClipArgs {
    InsertClipArgs {
        timeline_id: timeline.into(),
        media_id: media.into(),
        at: 0,
        source_in: None,
        source_out: None,
        video_track: None,
        audio_track: None,
        video: true,
        audio: true,
    }
}

#[test]
fn every_edit_is_one_undo_step_and_a_failed_one_leaves_none() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    let before = session.history.position();
    // A generator on a new track: two commands, one step.
    ok(
        &mut session,
        ToolCall::AddTitle(AddTitleArgs {
            timeline_id: timeline.clone(),
            text: "Hi".into(),
            at: 0,
            duration: None,
            track: None,
            size: Some(80.0),
            color: Some([1.0, 0.0, 0.0, 1.0]),
            position: None,
        }),
    );
    assert_eq!(session.history.position(), before + 1);

    let bad = ToolCall::Split(SplitArgs {
        timeline_id: timeline.clone(),
        frame: 1000,
        clip_ids: None,
    });
    assert_eq!(
        error(&mut session, bad),
        "no clip of an unlocked track crosses frame 1000"
    );
    assert_eq!(session.history.position(), before + 1);
    ok(&mut session, ToolCall::Undo);
    assert!(spans(&mut session, &timeline, 0).is_empty());
}

#[test]
fn insert_clip_validates_then_links_video_and_audio() {
    let dir = test_dir("insert-clip");
    let a = clip_file(&dir, "a.mp4");
    let mut session = Session::default();
    let media = import(&mut session, &[&a])["media"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let timeline = with_timeline(&mut session);

    let mut args = insert(&timeline, &media);
    args.source_out = Some(1000);
    assert!(error(&mut session, ToolCall::InsertClip(args)).contains("past the end"));
    let mut args = insert(&timeline, &media);
    args.video_track = Some("A1".into());
    assert_eq!(
        error(&mut session, ToolCall::InsertClip(args)),
        "track A1 is not a video track"
    );

    let mut args = insert(&timeline, &media);
    args.at = 10;
    args.source_in = Some(5);
    args.source_out = Some(15);
    let clips = ok(&mut session, ToolCall::InsertClip(args))["clips"].clone();
    let clips = clips.as_array().unwrap();
    assert_eq!(clips.len(), 2);
    assert_eq!(clips[0]["track"], "V1");
    assert_eq!(clips[1]["track"], "A1");
    assert_eq!(clips[0]["link_group"], clips[1]["link_group"]);
    assert!(!clips[0]["link_group"].is_null());
    assert_eq!(clips[0]["source_in"], 5);
    assert_eq!(clips[0]["source_out"], 15);
}

#[test]
fn insert_clip_audio_only_creates_its_track_when_there_is_none() {
    let dir = test_dir("insert-audio");
    let a = clip_file(&dir, "a.mp4");
    let mut session = Session::default();
    let media = import(&mut session, &[&a])["media"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let timeline = with_timeline(&mut session);
    lock(&mut session, &timeline, "A1");

    let mut args = insert(&timeline, &media);
    args.video = false;
    let clips = ok(&mut session, ToolCall::InsertClip(args))["clips"].clone();
    assert_eq!(clips[0]["track"], "A2");
}

#[test]
fn locked_tracks_refuse_edits() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    let clip = solid(&mut session, &timeline, 0, 50);
    lock(&mut session, &timeline, "V1");
    let delete = ToolCall::DeleteClips(DeleteClipsArgs {
        timeline_id: timeline.clone(),
        clip_ids: vec![clip],
        ripple: false,
    });
    assert_eq!(error(&mut session, delete), "track V1 is locked");
    let title = ToolCall::AddTitle(AddTitleArgs {
        timeline_id: timeline.clone(),
        text: "x".into(),
        at: 0,
        duration: None,
        track: Some("V1".into()),
        size: None,
        color: None,
        position: None,
    });
    assert_eq!(error(&mut session, title), "track V1 is locked");
}

#[test]
fn split_reports_both_halves() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    let clip = solid(&mut session, &timeline, 0, 50);

    let result = ok(
        &mut session,
        ToolCall::Split(SplitArgs {
            timeline_id: timeline.clone(),
            frame: 20,
            clip_ids: Some(vec![clip.clone()]),
        }),
    );

    assert_eq!(result["split"][0]["left"], json!(clip));
    assert_eq!(result["split"][0]["track"], "V1");
    assert_eq!(spans(&mut session, &timeline, 0), [(0, 20), (20, 50)]);
}

#[test]
fn delete_ranges_rejects_tracks_with_ripple_and_lifts_on_named_tracks() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    solid(&mut session, &timeline, 0, 100);
    ok(
        &mut session,
        ToolCall::AddTrack(AddTrackArgs {
            timeline_id: timeline.clone(),
            kind: TrackKindArg::Video,
        }),
    );
    let ranges = |ripple, tracks: Option<Vec<String>>| {
        ToolCall::DeleteRanges(DeleteRangesArgs {
            timeline_id: timeline.clone(),
            ranges: vec![[10, 20]],
            ripple,
            tracks,
        })
    };
    assert!(error(&mut session, ranges(true, Some(vec!["V1".into()]))).contains("ripple"));
    assert_eq!(
        error(
            &mut session,
            ToolCall::DeleteRanges(DeleteRangesArgs {
                timeline_id: timeline.clone(),
                ranges: vec![[20, 10]],
                ripple: false,
                tracks: None,
            })
        ),
        "invalid range [20, 10)"
    );

    ok(&mut session, ranges(false, Some(vec!["V2".into()])));
    assert_eq!(spans(&mut session, &timeline, 0), [(0, 100)]);
    ok(&mut session, ranges(true, None));
    assert_eq!(spans(&mut session, &timeline, 0), [(0, 10), (10, 90)]);
}

#[test]
fn move_and_trim_validate_their_targets() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    let a = solid(&mut session, &timeline, 0, 50);
    let b = solid(&mut session, &timeline, 50, 50);

    let wrong_kind = ToolCall::MoveClips(MoveClipsArgs {
        timeline_id: timeline.clone(),
        moves: vec![ClipMove {
            clip_id: a.clone(),
            start: 0,
            track: Some("A1".into()),
        }],
    });
    assert_eq!(
        error(&mut session, wrong_kind),
        "track A1 is not a video track"
    );
    // Moving `a` over `b` overwrites the start of `b`.
    ok(
        &mut session,
        ToolCall::MoveClips(MoveClipsArgs {
            timeline_id: timeline.clone(),
            moves: vec![ClipMove {
                clip_id: a.clone(),
                start: 30,
                track: None,
            }],
        }),
    );
    assert_eq!(spans(&mut session, &timeline, 0), [(30, 80), (80, 100)]);

    let trim = |clip: &str, edge, frame| {
        ToolCall::TrimClip(TrimClipArgs {
            timeline_id: timeline.clone(),
            clip_id: clip.into(),
            edge,
            frame,
        })
    };
    assert_eq!(
        error(&mut session, trim(&b, EdgeArg::Start, 100)),
        "the start edge can go from frame 50 to frame 99"
    );
    ok(&mut session, trim(&b, EdgeArg::End, 120));
    assert_eq!(spans(&mut session, &timeline, 0), [(30, 80), (80, 120)]);
}

#[test]
fn set_clip_properties_checks_ranges_and_applies_everything() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    let clip = solid(&mut session, &timeline, 0, 50);
    let props = |opacity, fade_in| SetClipPropertiesArgs {
        timeline_id: timeline.clone(),
        clip_ids: vec![clip.clone()],
        opacity,
        position: Some([10.0, -5.0]),
        scale: None,
        rotation: None,
        gain_db: None,
        disabled: Some(true),
        fade_in,
        fade_out: None,
        color: Some([0.0, 0.0, 1.0, 1.0]),
    };
    assert_eq!(
        error(
            &mut session,
            ToolCall::SetClipProperties(props(Some(150.0), None))
        ),
        "opacity goes from 0 to 100"
    );
    assert!(
        error(
            &mut session,
            ToolCall::SetClipProperties(props(None, Some(60)))
        )
        .starts_with("fade_in of clip")
    );

    let before = session.history.position();
    ok(
        &mut session,
        ToolCall::SetClipProperties(props(Some(40.0), Some(10))),
    );
    assert_eq!(session.history.position(), before + 1);
    let detail = ok(
        &mut session,
        ToolCall::GetClip(ClipArgs {
            timeline_id: timeline.clone(),
            clip_id: clip.clone(),
        }),
    );
    assert_eq!(detail["disabled"], true);
    assert_eq!(detail["fade_in"], 10);
    assert_eq!(detail["effects"]["color"]["default"]["b"], 1.0);
}

#[test]
fn links_and_markers() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    let a = solid(&mut session, &timeline, 0, 50);
    let b = solid(&mut session, &timeline, 50, 50);
    let clips = ToolCall::LinkClips(ClipsArgs {
        timeline_id: timeline.clone(),
        clip_ids: vec![a.clone(), b.clone()],
    });
    let linked = ok(&mut session, clips);
    assert!(!linked["clips"][0]["link_group"].is_null());
    let unlink = || {
        ToolCall::UnlinkClips(ClipsArgs {
            timeline_id: timeline.clone(),
            clip_ids: vec![a.clone()],
        })
    };
    let unlinked = ok(&mut session, unlink());
    assert!(unlinked["clips"][0]["link_group"].is_null());
    assert_eq!(
        error(&mut session, unlink()),
        "none of these clips is linked"
    );

    let marker = ok(
        &mut session,
        ToolCall::AddMarker(AddMarkerArgs {
            timeline_id: timeline.clone(),
            at: 12,
            duration: None,
            note: Some("check".into()),
        }),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let edited = ok(
        &mut session,
        ToolCall::EditMarker(EditMarkerArgs {
            timeline_id: timeline.clone(),
            marker_id: marker.clone(),
            at: Some(20),
            duration: Some(5),
            note: None,
        }),
    );
    assert_eq!(
        edited,
        json!({ "id": marker, "start": 20, "duration": 5, "note": "check" })
    );
    ok(
        &mut session,
        ToolCall::DeleteMarker(MarkerArgs {
            timeline_id: timeline.clone(),
            marker_id: marker.clone(),
        }),
    );
    let markers = ok(
        &mut session,
        ToolCall::GetTimeline(TimelineArgs {
            timeline_id: timeline.clone(),
        }),
    )["markers"]
        .clone();
    assert_eq!(markers, json!([]));
}

#[test]
fn unknown_ids_are_reported() {
    let mut session = Session::default();
    let timeline = with_timeline(&mut session);
    let call = ToolCall::GetClip(ClipArgs {
        timeline_id: timeline.clone(),
        clip_id: "99".into(),
    });
    assert_eq!(error(&mut session, call), "no clip \"99\" in this timeline");
    let call = ToolCall::GetTimeline(TimelineArgs {
        timeline_id: "7".into(),
    });
    assert_eq!(error(&mut session, call), "unknown timeline id \"7\"");
    assert!(matches!(
        dispatch(&mut session, ToolCall::Undo),
        crate::Dispatch::Handled(Err(_))
    ));
}
