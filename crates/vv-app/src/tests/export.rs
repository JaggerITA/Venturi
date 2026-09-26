use super::*;
use vv_core::ClipSource;
use vv_core::{Clip, Keyframed, Rgba, Track, TrackKind};

fn solid_color_clip(id: u64, start: FrameIdx, len: FrameIdx, color: Rgba) -> Clip {
    let mut clip = Clip::from_source_range(
        ClipId(id),
        ClipSource::SolidColor,
        0,
        len,
        start,
        vv_core::Rational::one(),
    );
    clip.effects.color = Some(Keyframed::constant(color));
    clip
}

fn timeline_with(tracks: Vec<Track>) -> Timeline {
    Timeline {
        name: "t".into(),
        fps: vv_core::Rational::new(25, 1),
        resolution: (4, 2),
        tracks,
    }
}

// BT.709 limited range, like `Compositor::render_layers_i420`.
const BLACK_I420: [u8; 3] = [16, 128, 128];
const RED_I420: [u8; 3] = [63, 102, 240];
const BLUE_I420: [u8; 3] = [32, 240, 118];

fn solid_i420(width: usize, height: usize, [y, u, v]: [u8; 3]) -> Vec<u8> {
    let chroma = width.div_ceil(2) * height.div_ceil(2);
    let mut data = vec![y; width * height];
    data.extend(std::iter::repeat_n(u, chroma));
    data.extend(std::iter::repeat_n(v, chroma));
    data
}

fn red() -> Rgba {
    Rgba {
        r: 1.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    }
}

fn blue() -> Rgba {
    Rgba {
        r: 0.0,
        g: 0.0,
        b: 1.0,
        a: 1.0,
    }
}

#[test]
fn render_video_frame_returns_black_in_a_gap() {
    let project = Project::default();
    let tl = timeline_with(vec![Track {
        kind: TrackKind::Video,
        clips: vec![solid_color_clip(1, 10, 5, red())],
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
    }]);
    let compositor = vv_render::Compositor::new_headless();
    let mut active = StreamingFrameProvider::default();
    let frame = render_video_frame(&project, &tl, &compositor, &mut active, 0, (2, 2)).unwrap();
    assert_eq!(frame, solid_i420(2, 2, BLACK_I420));
}

#[test]
fn render_video_frame_reads_solid_color_at_the_clips_source_frame() {
    let project = Project::default();
    let tl = timeline_with(vec![Track {
        kind: TrackKind::Video,
        clips: vec![solid_color_clip(1, 10, 5, red())],
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
    }]);
    let compositor = vv_render::Compositor::new_headless();
    let mut active = StreamingFrameProvider::default();
    let frame = render_video_frame(&project, &tl, &compositor, &mut active, 12, (2, 2)).unwrap();
    assert_eq!(frame, solid_i420(2, 2, RED_I420));
}

/// A video clip inside the nested timeline of a compound clip must
/// show up in the exported frame, at the position of the compound clip
/// in the outer timeline — not before/after, and not at the one it
/// would have in its nested timeline.
#[test]
fn render_video_frame_recurses_into_a_compound_clips_nested_timeline() {
    let mut project = Project::default();
    let nested_id = project.timelines.insert(Timeline {
        name: "Nested".into(),
        fps: vv_core::Rational::new(25, 1),
        resolution: (2, 2),
        tracks: vec![Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(1, 0, 10, red())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }],
    });
    let compound_media = project.media_pool.insert(vv_core::MediaItem {
        path: "Compound Clip 1".into(),
        meta: vv_core::MediaMeta {
            duration_frames: 10,
            fps: vv_core::Rational::new(25, 1),
            width: 2,
            height: 2,
            has_video: true,
            has_audio: false,
            sample_rate: 0,
            channels: 0,
            audio_streams: 0,
        },
        content_hash: 1,
        compound: Some(nested_id),
    });
    let compound_clip = Clip::from_source_range(
        ClipId(2),
        ClipSource::Media(compound_media),
        0,
        10,
        5,
        vv_core::Rational::one(),
    );
    let tl = timeline_with(vec![Track {
        kind: TrackKind::Video,
        clips: vec![compound_clip],
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
    }]);
    let compositor = vv_render::Compositor::new_headless();
    let mut provider = StreamingFrameProvider::default();

    let before = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (2, 2)).unwrap();
    assert_eq!(
        before,
        solid_i420(2, 2, BLACK_I420),
        "before the compound clip: empty"
    );

    let during = render_video_frame(&project, &tl, &compositor, &mut provider, 7, (2, 2)).unwrap();
    assert_eq!(
        during,
        solid_i420(2, 2, RED_I420),
        "inside: the content of the nested timeline"
    );
}

/// Bug reported by the user: a compound clip is a timeline like any
/// other, so where its nested timeline has nothing to show it must
/// stay transparent and let the track below show through —
/// not cover it with black.
#[test]
fn render_video_frame_lets_the_track_below_show_through_the_compound_clips_empty_area() {
    let mut project = Project::default();
    let mut nested_clip = solid_color_clip(1, 0, 10, red());
    // Cuts away the right half (crop in timeline pixels, nested 4x2).
    nested_clip.effects.transform = vv_core::TransformTracks::constant(vv_core::Transform {
        crop: [0.0, 0.0, 2.0, 0.0],
        ..Default::default()
    });
    let nested_id = project.timelines.insert(Timeline {
        name: "Nested".into(),
        fps: vv_core::Rational::new(25, 1),
        resolution: (4, 2),
        tracks: vec![Track {
            kind: TrackKind::Video,
            clips: vec![nested_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }],
    });
    let compound_media = project.media_pool.insert(vv_core::MediaItem {
        path: "Compound Clip 1".into(),
        meta: vv_core::MediaMeta {
            duration_frames: 10,
            fps: vv_core::Rational::new(25, 1),
            width: 4,
            height: 2,
            has_video: true,
            has_audio: false,
            sample_rate: 0,
            channels: 0,
            audio_streams: 0,
        },
        content_hash: 1,
        compound: Some(nested_id),
    });
    let compound_clip = Clip::from_source_range(
        ClipId(2),
        ClipSource::Media(compound_media),
        0,
        10,
        0,
        vv_core::Rational::one(),
    );
    let tl = timeline_with(vec![
        Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(3, 0, 10, blue())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
        Track {
            kind: TrackKind::Video,
            clips: vec![compound_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
    ]);
    let compositor = vv_render::Compositor::new_headless();
    let mut provider = StreamingFrameProvider::default();

    let frame = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (4, 2)).unwrap();
    // Y plane, one byte per pixel: left covered by the red of the
    // compound clip, right uncovered (the blue below must show).
    assert_eq!(frame[0], RED_I420[0], "left: the red of the compound clip");
    assert_eq!(
        frame[3], BLUE_I420[0],
        "right: the blue of the track below, not black"
    );
}

/// A PNG with transparency imported into the pool must let the track
/// below show through where it is transparent, not cover it: its alpha must
/// be preserved from decode all the way to compositing.
#[test]
fn render_video_frame_lets_the_track_below_show_through_a_transparent_png() {
    let dir = std::env::temp_dir().join("vv-app-export-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("half_transparent.png");
    // Left half opaque red, right half fully transparent.
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=4x2:d=1",
            "-vf",
            "format=rgba,geq=r='255':g='0':b='0':a='if(lt(X,2),255,0)'",
            "-frames:v",
            "1",
            "-update",
            "1",
        ],
        &path,
    );

    let mut project = Project::default();
    let meta = vv_media::probe::probe_image(&path).expect("PNG probe failed");
    let png_media = project.media_pool.insert(vv_core::MediaItem {
        path: path.clone(),
        meta,
        content_hash: 1,
        compound: None,
    });
    let png_clip = Clip::from_source_range(
        ClipId(2),
        ClipSource::Media(png_media),
        0,
        10,
        0,
        vv_core::Rational::one(),
    );
    let tl = timeline_with(vec![
        Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(3, 0, 10, blue())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
        Track {
            kind: TrackKind::Video,
            clips: vec![png_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
    ]);
    let compositor = vv_render::Compositor::new_headless();
    let mut provider = StreamingFrameProvider::default();

    let frame = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (4, 2)).unwrap();
    assert!(
        (frame[0] as i16 - RED_I420[0] as i16).abs() <= 4,
        "left: the opaque red of the PNG, not the blue ({})",
        frame[0]
    );
    assert!(
        (frame[3] as i16 - BLUE_I420[0] as i16).abs() <= 4,
        "right: transparent, the blue below must show ({})",
        frame[3]
    );
}

#[test]
fn render_video_frame_applies_the_transform_to_a_solid_color_clip() {
    let project = Project::default();
    let mut clip = solid_color_clip(1, 0, 5, red());
    // Crop in timeline pixels (4x2): away with the right half.
    clip.effects.transform = vv_core::TransformTracks::constant(vv_core::Transform {
        crop: [0.0, 0.0, 2.0, 0.0],
        ..Default::default()
    });
    let tl = timeline_with(vec![Track {
        kind: TrackKind::Video,
        clips: vec![clip],
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
    }]);
    let compositor = vv_render::Compositor::new_headless();
    let mut active = StreamingFrameProvider::default();
    let frame = render_video_frame(&project, &tl, &compositor, &mut active, 0, (4, 2)).unwrap();
    // First row of the Y plane.
    assert_eq!(frame[0], RED_I420[0]);
    assert_eq!(frame[3], BLACK_I420[0]);
}

/// A media missing from the pool is an `Err`, not a black frame.
#[test]
fn render_video_frame_fails_loudly_when_the_clip_references_a_missing_media() {
    let missing_media_id = {
        let mut other_project = Project::default();
        other_project.media_pool.insert(vv_core::MediaItem {
            path: "dummy.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 0,
                fps: vv_core::Rational::new(25, 1),
                width: 0,
                height: 0,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        })
    };
    let project = Project::default();
    let tl = timeline_with(vec![Track {
        kind: TrackKind::Video,
        clips: vec![Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(missing_media_id),
            0,
            10,
            0,
            vv_core::Rational::one(),
        )],
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
    }]);
    let compositor = vv_render::Compositor::new_headless();
    let mut provider = StreamingFrameProvider::default();
    let err = render_video_frame(&project, &tl, &compositor, &mut provider, 0, (2, 2))
        .expect_err("a media missing from the pool must fail, not produce a black frame");
    assert!(err.contains("media not found"), "err={err}");
}

/// Where the top track has no clip, the one below shows.
#[test]
fn render_video_frame_prefers_the_topmost_video_track() {
    let project = Project::default();
    let tl = timeline_with(vec![
        Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(1, 0, 30, red())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
        Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(2, 10, 10, blue())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
    ]);
    let compositor = vv_render::Compositor::new_headless();
    let mut provider = StreamingFrameProvider::default();

    let below = render_video_frame(&project, &tl, &compositor, &mut provider, 5, (2, 2)).unwrap();
    assert_eq!(
        below,
        solid_i420(2, 2, RED_I420),
        "below the top track: the bottom one shows"
    );

    let above = render_video_frame(&project, &tl, &compositor, &mut provider, 15, (2, 2)).unwrap();
    assert_eq!(
        above,
        solid_i420(2, 2, BLUE_I420),
        "the top track has a clip here: it wins"
    );
}

#[test]
fn mix_audio_track_is_silence_when_no_audio_track_has_clips() {
    let project = Project::default();
    let tl = timeline_with(vec![
        Track {
            kind: TrackKind::Video,
            clips: vec![solid_color_clip(1, 0, 25, red())],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
        Track {
            kind: TrackKind::Audio,
            clips: vec![],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        },
    ]);
    let mixed = mix_audio_track(&project, &tl, 0..25).unwrap();
    assert_eq!(
        mixed.len(),
        (PROJECT_SAMPLE_RATE as usize) * PROJECT_CHANNELS as usize
    );
    assert!(mixed.iter().all(|&s| s == 0.0));
}

/// A real audio file inside the nested timeline of a compound clip
/// must reach the export mix, at the position of the compound
/// clip in the outer timeline — not at the one it would have in its
/// nested timeline.
#[test]
fn mix_audio_track_recurses_into_a_compound_clips_nested_timeline() {
    let dir = std::env::temp_dir().join("vv-app-export-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("compound_audio_source.wav");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
        ],
        &path,
    );

    let mut project = Project::default();
    let real_media = project.media_pool.insert(vv_core::MediaItem {
        path: path.clone(),
        meta: vv_core::MediaMeta {
            duration_frames: 25,
            fps: vv_core::Rational::new(25, 1),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate: 48_000,
            channels: 1,
            audio_streams: 1,
        },
        content_hash: 1,
        compound: None,
    });
    let real_clip = Clip::from_source_range(
        ClipId(100),
        ClipSource::Media(real_media),
        0,
        25,
        0,
        vv_core::Rational::one(),
    );
    let nested_id = project.timelines.insert(Timeline {
        name: "Nested".into(),
        fps: vv_core::Rational::new(25, 1),
        resolution: (1, 1),
        tracks: vec![Track {
            kind: TrackKind::Audio,
            clips: vec![real_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }],
    });
    let compound_media = project.media_pool.insert(vv_core::MediaItem {
        path: "Compound Clip 1".into(),
        meta: vv_core::MediaMeta {
            duration_frames: 25,
            fps: vv_core::Rational::new(25, 1),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate: 48_000,
            channels: 1,
            audio_streams: 1,
        },
        content_hash: 2,
        compound: Some(nested_id),
    });
    // At 25 (1s after the start): silence before, sine wave during.
    let compound_clip = Clip::from_source_range(
        ClipId(1),
        ClipSource::Media(compound_media),
        0,
        25,
        25,
        vv_core::Rational::one(),
    );
    let tl = timeline_with(vec![Track {
        kind: TrackKind::Audio,
        clips: vec![compound_clip],
        muted: false,
        solo: false,
        locked: false,
        crossings: Vec::new(),
    }]);

    let mixed = mix_audio_track(&project, &tl, 0..50).unwrap();
    let per_second = PROJECT_SAMPLE_RATE as usize * PROJECT_CHANNELS as usize;
    assert!(
        mixed[..per_second].iter().all(|&s| s == 0.0),
        "silence before the compound clip"
    );
    assert!(
        mixed[per_second..].iter().any(|&s| s.abs() > 0.01),
        "the nested timeline content reaches the mix at the compound clip position"
    );
}

/// End-to-end: builds a real timeline (via `VenturiApp`, not
/// `Clip`/`Track` by hand) with a real video+audio file generated by
/// the ffmpeg CLI (same pattern as the tests in `main.rs`), exports, and
/// checks the result by reading it back with `vv_media` — the only way to
/// verify the export in this environment, without a display.
#[test]
fn export_timeline_produces_a_playable_file_matching_the_timeline() {
    let dir = std::env::temp_dir().join("vv-app-export-test");
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("source.mp4");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=25:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ],
        &source_path,
    );

    let mut app = crate::VenturiApp::default();
    app.import_media(source_path);
    let timeline_id = app
        .timeline_id
        .expect("import should have created the timeline");
    let media_id = app
        .project
        .media_pool
        .iter()
        .find(|(_, item)| item.compound.is_none())
        .map(|(id, _)| id)
        .expect("imported media expected in the pool");
    app.add_media_to_timeline(media_id);

    let output_path = dir.join("out.mp4");
    let progress = Mutex::new(ExportProgress::default());
    let cancel = AtomicBool::new(false);
    let total = app.project.timelines[timeline_id].total_frames();
    let settings = ExportSettings::new(output_path.clone());
    export_timeline(
        &app.project,
        timeline_id,
        &settings,
        0..total,
        &progress,
        &cancel,
    )
    .expect("export failed");

    assert!(progress.lock().unwrap().done);

    let meta = vv_media::probe(&output_path).unwrap();
    assert_eq!(meta.width, 64);
    assert_eq!(meta.height, 48);
    assert!(meta.has_audio);

    let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
    let mut count = 0;
    while decoder.next_frame().unwrap().is_some() {
        count += 1;
    }
    // ~25fps for 1s, the same tolerance already used for the B-frame
    // encoder reordering seen in the `vv_media::encode` tests.
    assert!((24..=25).contains(&count), "count={count}");

    let audio = vv_media::decode_audio_track(&output_path, 0)
        .unwrap()
        .expect("audio expected in the export");
    let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
    assert!(peak > 0.1, "peak={peak}, expected a non-silent signal");
}

/// Regression for a user report of audio running ahead of the
/// video on export (perceived with mpv, 2-3 frames): a black->white
/// flash (`ClipSource::SolidColor`, no video encode/decode
/// involved in its position) and a beep (a real audio clip) that
/// start at the same timeline frame, at a fractional NTSC fps
/// (30000/1001) on purpose so that `PROJECT_SAMPLE_RATE / fps` is not an
/// integer (at 24fps it is exactly 2000, which would hide a
/// rounding bug) — the real case isolated from the report
/// was already at an integer fps, and here it comes out perfectly in
/// sync anyway: if this test breaks, the bug is in the
/// frame<->sample rounding of `export.rs`/`encode.rs`/`vv_audio::mixer`, not
/// in the original cause of the report (probably on the
/// player side, not in this export).
#[test]
fn export_keeps_video_and_audio_frame_accurate_at_a_fractional_ntsc_fps() {
    let dir = std::env::temp_dir().join("vv-app-diag-avsync-frac");
    std::fs::create_dir_all(&dir).unwrap();
    let beep_path = dir.join("beep.wav");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000:duration=1",
        ],
        &beep_path,
    );

    let mut project = Project::default();
    let beep_media = project.media_pool.insert(vv_core::MediaItem {
        path: beep_path,
        meta: vv_core::MediaMeta {
            duration_frames: 30,
            fps: vv_core::Rational::new(30000, 1001),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate: 48_000,
            channels: 1,
            audio_streams: 1,
        },
        content_hash: 1,
        compound: None,
    });
    const FLASH_FRAME: FrameIdx = 20;
    let beep_clip = Clip::from_source_range(
        ClipId(1),
        ClipSource::Media(beep_media),
        0,
        25,
        FLASH_FRAME,
        vv_core::Rational::one(),
    );
    let tl = Timeline {
        name: "diag".into(),
        fps: vv_core::Rational::new(30000, 1001),
        resolution: (64, 48),
        tracks: vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![
                    solid_color_clip(10, 0, FLASH_FRAME, black()),
                    solid_color_clip(11, FLASH_FRAME, 15, white()),
                ],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
            Track {
                kind: TrackKind::Audio,
                clips: vec![beep_clip],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
        ],
    };
    let timeline_id = project.timelines.insert(tl);

    let output_path = dir.join("out.mp4");
    let progress = Mutex::new(ExportProgress::default());
    let cancel = AtomicBool::new(false);
    let total = project.timelines[timeline_id].total_frames();
    let settings = ExportSettings::new(output_path.clone());
    export_timeline(
        &project,
        timeline_id,
        &settings,
        0..total,
        &progress,
        &cancel,
    )
    .expect("export failed");

    let fps = 30000.0_f64 / 1001.0;

    let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
    let mut white_frame_idx = None;
    let mut idx = 0i64;
    while let Some((_, frame)) = decoder.next_frame().unwrap() {
        let avg_y = frame.y.iter().map(|&b| b as u64).sum::<u64>() / frame.y.len() as u64;
        if avg_y > 128 && white_frame_idx.is_none() {
            white_frame_idx = Some(idx);
        }
        idx += 1;
    }
    let white_frame_idx = white_frame_idx.expect("no white frame found in the export");

    let audio = vv_media::decode_audio_track(&output_path, 0)
        .unwrap()
        .expect("audio expected");
    let mut onset_sample = None;
    for (i, &s) in audio.samples.iter().enumerate() {
        if s.abs() > 0.05 {
            onset_sample = Some(i as u64 / audio.channels as u64);
            break;
        }
    }
    let onset_sample = onset_sample.expect("no audio onset found in the export");
    let onset_secs = onset_sample as f64 / audio.sample_rate as f64;
    let onset_frame = (onset_secs * fps).floor() as i64;

    assert_eq!(
        white_frame_idx, FLASH_FRAME,
        "the video flash is not at the expected frame"
    );
    assert_eq!(
        onset_frame,
        FLASH_FRAME,
        "the audio onset ({onset_secs:.6}s) falls in frame {onset_frame} instead of the video flash frame {FLASH_FRAME}: {} frame offset",
        FLASH_FRAME - onset_frame
    );
}

fn black() -> Rgba {
    Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    }
}

fn white() -> Rgba {
    Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    }
}

/// End-to-end regression for exporting an image (see
/// `ActiveClipDecoder::open_for`): the default clip (5s = 125
/// frames at 25fps) covers well past the single real frame
/// an image has — before the dedicated support the export would
/// have errored out (or stopped) as soon as it went past the first
/// requested position.
#[test]
fn export_timeline_covers_a_stretched_image_clip_past_its_only_real_frame() {
    let dir = std::env::temp_dir().join("vv-app-export-image-test");
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("still.png");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=yellow:size=64x48:rate=1:duration=1",
            "-frames:v",
            "1",
            "-update",
            "1",
        ],
        &source_path,
    );

    let mut app = crate::VenturiApp::default();
    app.import_media(source_path);
    let timeline_id = app
        .timeline_id
        .expect("import should have created the timeline");
    let media_id = app
        .project
        .media_pool
        .iter()
        .find(|(_, item)| item.compound.is_none())
        .map(|(id, _)| id)
        .expect("imported media expected in the pool");
    assert!(app.project.media_pool[media_id].meta.is_image());
    app.add_media_to_timeline(media_id);

    let output_path = dir.join("out.mp4");
    let progress = Mutex::new(ExportProgress::default());
    let cancel = AtomicBool::new(false);
    let total = app.project.timelines[timeline_id].total_frames();
    assert_eq!(total, 5 * 25, "5 s by default at 25 fps");
    let settings = ExportSettings::new(output_path.clone());
    export_timeline(
        &app.project,
        timeline_id,
        &settings,
        0..total,
        &progress,
        &cancel,
    )
    .expect("export failed");

    assert!(progress.lock().unwrap().done);

    let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
    let mut count = 0;
    while let Some((_, frame)) = decoder.next_frame().unwrap() {
        // Yellow over the whole frame, for the whole export: if
        // the image "ended" halfway, black would appear here (or an
        // error would already have interrupted the export above).
        assert!(
            frame.y[0] > 150,
            "expected the image frame still, not black"
        );
        count += 1;
    }
    assert!((total - 1..=total).contains(&count), "count={count}");
}

/// The bug case, end-to-end: a clip at 23.976 fps appended onto a
/// timeline at 25 (created from the first media, at 25). Conformed, the
/// second clip occupies its real time on the timeline, so the exported
/// file lasts as long as the two clips together and the audio of the second
/// reaches the very end instead of finishing before the video.
#[test]
fn export_conforms_a_clip_whose_fps_differs_from_the_timeline() {
    let dir = std::env::temp_dir().join("vv-app-export-conform-test");
    std::fs::create_dir_all(&dir).unwrap();

    let mute_25 = dir.join("mute25.mp4");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=25:duration=1",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=sample_rate=48000:channel_layout=stereo",
            "-t",
            "1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ],
        &mute_25,
    );

    // 24000/1001 = 23.976 fps: 48 source frames for 2 real s.
    let sine_23976 = dir.join("sine23976.mp4");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=24000/1001:duration=2",
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
        &sine_23976,
    );

    let mut app = crate::VenturiApp::default();
    app.import_media(mute_25);
    let timeline_id = app
        .timeline_id
        .expect("import should have created the timeline");
    assert_eq!(
        app.project.timelines[timeline_id].fps,
        vv_core::Rational::new(25, 1)
    );
    let first = app
        .project
        .media_pool
        .iter()
        .find(|(_, item)| item.compound.is_none())
        .map(|(id, _)| id)
        .unwrap();
    app.add_media_to_timeline(first);

    app.import_media(sine_23976);
    let second = app
        .project
        .media_pool
        .iter()
        .filter(|(_, item)| item.compound.is_none())
        .map(|(id, _)| id)
        .find(|id| *id != first)
        .expect("second media expected in the pool");
    let second_start = app.project.timelines[timeline_id].total_frames();
    app.add_media_to_timeline(second);

    let conformed = app.project.timelines[timeline_id]
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter())
        .find(|c| c.timeline_start == second_start)
        .expect("conformed clip expected");
    assert_ne!(conformed.rate, vv_core::Rational::one());
    let total = app.project.timelines[timeline_id].total_frames();
    // 1 s at 25 fps + 2 s conformed to 25 fps, within one frame of
    // rounding on the duration reported by ffmpeg.
    assert!((74..=76).contains(&total), "total={total}");

    let output_path = dir.join("out.mp4");
    let progress = Mutex::new(ExportProgress::default());
    export_timeline(
        &app.project,
        timeline_id,
        &ExportSettings::new(output_path.clone()),
        0..total,
        &progress,
        &AtomicBool::new(false),
    )
    .expect("export failed");

    let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
    let mut count = 0;
    while decoder.next_frame().unwrap().is_some() {
        count += 1;
    }
    assert!((total - 1..=total).contains(&count), "count={count}");

    let audio = vv_media::decode_audio_track(&output_path, 0)
        .unwrap()
        .expect("audio expected in the export");
    let frames = audio.samples.len() / audio.channels as usize;
    let secs = frames as f64 / audio.sample_rate as f64;
    let expected_secs = total as f64 / 25.0;
    assert!(
        (secs - expected_secs).abs() < 0.1,
        "audio {secs}s versus {expected_secs}s of video"
    );

    // The audio of the second clip covers its stretch to the end:
    // if the video were mapped 1:1 onto the source frames, the timeline
    // would end earlier and the tail of the sine would be cut off.
    let peak_in = |from_secs: f64, to_secs: f64| {
        let ch = audio.channels as usize;
        let from = (from_secs * audio.sample_rate as f64) as usize * ch;
        let to = ((to_secs * audio.sample_rate as f64) as usize * ch).min(audio.samples.len());
        audio.samples[from.min(to)..to]
            .iter()
            .fold(0.0_f32, |m, s| m.max(s.abs()))
    };
    assert!(peak_in(0.1, 0.9) < 0.05, "the first clip is muted");
    assert!(peak_in(1.1, 1.9) > 0.1, "the second clip plays");
    assert!(
        peak_in(expected_secs - 0.2, expected_secs) > 0.1,
        "the sine must reach the end of the timeline"
    );
}

#[test]
fn preferred_settings_pick_the_faster_encoders_only_when_available() {
    let settings = ExportSettings::preferred(PathBuf::from("out.mp4"));
    let nvenc = vv_media::VideoCodec::Nvenc.is_available();
    assert_eq!(settings.video.codec == vv_media::VideoCodec::Nvenc, nvenc);
    assert_eq!(settings.video.preset, settings.video.codec.default_preset());
    let fdk = vv_media::AudioCodec::FdkAac.is_available();
    let audio = settings.audio.expect("audio included by default");
    assert_eq!(audio.codec == vv_media::AudioCodec::FdkAac, fdk);
}

#[test]
fn export_timeline_scales_the_output_and_can_drop_the_audio() {
    let dir = std::env::temp_dir().join("vv-app-export-scale-test");
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("source.mp4");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=25:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ],
        &source_path,
    );

    let mut app = crate::VenturiApp::default();
    app.import_media(source_path);
    let media_id = app
        .project
        .media_pool
        .iter()
        .find(|(_, item)| item.compound.is_none())
        .map(|(id, _)| id)
        .unwrap();
    app.add_media_to_timeline(media_id);
    let timeline_id = app.timeline_id.unwrap();
    let total = app.project.timelines[timeline_id].total_frames();

    let mut settings = ExportSettings::new(dir.join("out.mp4"));
    settings.scale_percent = 50;
    settings.audio = None;
    export_timeline(
        &app.project,
        timeline_id,
        &settings,
        0..total,
        &Mutex::new(ExportProgress::default()),
        &AtomicBool::new(false),
    )
    .expect("export failed");

    let meta = vv_media::probe(&settings.output_path).unwrap();
    assert_eq!((meta.width, meta.height), (32, 24));
    assert!(!meta.has_audio);
}

#[test]
fn export_timeline_writes_only_the_in_out_range() {
    let dir = std::env::temp_dir().join("vv-app-export-range-test");
    std::fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("source.mp4");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=25:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ],
        &source_path,
    );

    let mut app = crate::VenturiApp::default();
    app.import_media(source_path);
    let media_id = app
        .project
        .media_pool
        .iter()
        .find(|(_, item)| item.compound.is_none())
        .map(|(id, _)| id)
        .unwrap();
    app.add_media_to_timeline(media_id);
    let timeline_id = app.timeline_id.unwrap();

    let output_path = dir.join("out.mp4");
    let progress = Mutex::new(ExportProgress::default());
    export_timeline(
        &app.project,
        timeline_id,
        &ExportSettings::new(output_path.clone()),
        5..15,
        &progress,
        &AtomicBool::new(false),
    )
    .expect("export failed");
    assert_eq!(progress.lock().unwrap().total_frames, 10);

    let mut decoder = vv_media::Decoder::open(&output_path).unwrap();
    let mut count = 0;
    while decoder.next_frame().unwrap().is_some() {
        count += 1;
    }
    assert!((9..=10).contains(&count), "count={count}");
}

/// 2 s of a 440 Hz sine at 200%: 1 s on the timeline, at 440 Hz with pitch
/// correction and at 880 Hz without.
#[test]
fn mix_audio_track_plays_a_faster_clip_with_or_without_its_pitch() {
    let dir = std::env::temp_dir().join("vv-app-export-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("speed_source.wav");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2",
        ],
        &path,
    );
    let mut project = Project::default();
    let media = project.media_pool.insert(vv_core::MediaItem {
        path,
        meta: vv_core::MediaMeta {
            duration_frames: 50,
            fps: vv_core::Rational::new(25, 1),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate: 48_000,
            channels: 1,
            audio_streams: 1,
        },
        content_hash: 1,
        compound: None,
    });
    let frequency = |pitch_correction: bool| {
        let mut clip = Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(media),
            0,
            50,
            0,
            vv_core::Rational::one(),
        );
        clip.speed = vv_core::Rational::new(2, 1);
        clip.pitch_correction = pitch_correction;
        clip.conform(vv_core::Rational::one());
        clip.timeline_len = 25;
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Audio,
            clips: vec![clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);
        let mixed = mix_audio_track(&project, &tl, 0..25).unwrap();
        // First channel, the middle half second: away from the stretch's edges.
        let mono: Vec<f32> = mixed
            .iter()
            .step_by(PROJECT_CHANNELS as usize)
            .copied()
            .collect();
        let window = &mono[12_000..36_000];
        let crossings = window
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f64 / 2.0 / 0.5
    };
    let corrected = frequency(true);
    let varispeed = frequency(false);
    assert!(
        (corrected - 440.0).abs() < 20.0,
        "pitch kept: {corrected} Hz"
    );
    assert!(
        (varispeed - 880.0).abs() < 20.0,
        "pitch follows: {varispeed} Hz"
    );
}
