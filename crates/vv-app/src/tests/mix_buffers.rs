use super::*;
use vv_core::Project;

fn wait_done(cache: &mut MixBufferCache) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cache.has_pending() {
        assert!(
            std::time::Instant::now() < deadline,
            "decode never finished"
        );
        std::thread::yield_now();
    }
}

#[test]
fn compound_mixdown_waits_for_its_real_media_then_caches_it() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("compound_source.wav");
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
            file: Default::default(),
        },
        content_hash: 1,
        compound: None,
    });
    let real_clip = vv_core::Clip::from_source_range(
        vv_core::ClipId(1),
        vv_core::ClipSource::Media(real_media),
        0,
        25,
        0,
        vv_core::Rational::one(),
    );
    let nested_id = project.timelines.insert(vv_core::Timeline {
        name: "Nested".into(),
        fps: vv_core::Rational::new(25, 1),
        resolution: (1, 1),
        tracks: vec![vv_core::Track {
            kind: vv_core::TrackKind::Audio,
            clips: vec![real_clip],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }],
        markers: Vec::new(),
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
            file: Default::default(),
        },
        content_hash: 2,
        compound: Some(nested_id),
    });

    let mut cache = MixBufferCache::spawn(48_000, 1);
    let mixdown = |cache: &mut MixBufferCache| {
        vv_audio::mixer::compound_mixdown(&project, compound_media, 48_000, 1, cache)
    };
    assert!(
        matches!(mixdown(&mut cache), vv_audio::ClipAudio::Pending),
        "the real media is not decoded yet: the compound clip is not ready"
    );

    wait_done(&mut cache);
    let vv_audio::ClipAudio::Ready(buffer) = mixdown(&mut cache) else {
        panic!("the decode is over: the mixdown must be complete");
    };
    assert!(
        buffer.iter().any(|&s| s.abs() > 0.01),
        "the sine wave must reach the mixdown"
    );

    // Same content_hash: the cached buffer, not a new mix.
    let vv_audio::ClipAudio::Ready(cached) = mixdown(&mut cache) else {
        panic!("a complete mixdown stays ready");
    };
    assert!(Arc::ptr_eq(&buffer, &cached));
}

#[test]
fn buffers_are_decoded_in_background_per_path_and_stream() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("two_streams.mkv");
    // Stream 0 mono 44.1 kHz of 1s, stream 1 stereo 48 kHz of 0.5s.
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=0.5",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-ac:1",
            "2",
            "-c:a",
            "pcm_f32le",
        ],
        &path,
    );

    let mut cache = MixBufferCache::spawn(48_000, 2);
    assert!(cache.get_or_request(&path, 0).is_none());
    assert!(cache.get_or_request(&path, 1).is_none());
    wait_done(&mut cache);

    let s0 = cache.get_or_request(&path, 0).expect("stream 0 pronto");
    let s1 = cache.get_or_request(&path, 1).expect("stream 1 pronto");
    let frames = |b: &Vec<f32>| b.len() / 2;
    assert!(
        (frames(&s0) as i64 - 48_000).abs() < 100,
        "s0={}",
        frames(&s0)
    );
    assert!(
        (frames(&s1) as i64 - 24_000).abs() < 100,
        "s1={}",
        frames(&s1)
    );
    assert!(
        cache.get_or_request(&path, 5).is_none(),
        "nonexistent stream"
    );
}

#[test]
fn a_long_track_is_published_partially_before_decoding_ends() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("long.wav");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=20",
        ],
        &path,
    );

    let mut cache = MixBufferCache::spawn(48_000, 2);
    cache.get_or_request(&path, 0);
    let mut lengths = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cache.has_pending() {
        assert!(
            std::time::Instant::now() < deadline,
            "decode never finished"
        );
        if let Some(buffer) = cache.get_or_request(&path, 0)
            && lengths.last() != Some(&buffer.len())
        {
            lengths.push(buffer.len());
        }
        std::thread::yield_now();
    }
    let full = cache.get_or_request(&path, 0).unwrap().len();
    assert_eq!(full, 20 * 48_000 * 2);
    assert!(
        lengths.first().is_some_and(|&first| first < full),
        "expected at least one partial publication: {lengths:?}"
    );
}

#[test]
fn a_priority_request_jumps_the_queue() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let paths: Vec<PathBuf> = ["queue_a.wav", "queue_b.wav", "queue_c.wav"]
        .iter()
        .map(|name| {
            let path = dir.join(name);
            vv_media::test_support::ffmpeg(
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000:duration=30",
                ],
                &path,
            );
            path
        })
        .collect();

    let mut cache = MixBufferCache::spawn(48_000, 2);
    cache.get_or_request(&paths[0], 0);
    cache.get_or_request(&paths[1], 0);
    cache.get_or_request_first(&paths[2], 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        assert!(std::time::Instant::now() < deadline, "decode never arrived");
        cache.poll();
        if cache.get_or_request(&paths[2], 0).is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        cache.get_or_request(&paths[1], 0).is_none(),
        "the priority request should have jumped ahead of the queued one"
    );
}

#[test]
fn every_stream_of_a_file_starts_playing_before_any_of_them_is_fully_decoded() {
    let dir = std::env::temp_dir().join("vv-app-mix-buffers-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("three_streams_long.mkv");
    vv_media::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=120",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=48000:duration=120",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=120",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-c:a",
            "aac",
        ],
        &path,
    );

    let mut cache = MixBufferCache::spawn(48_000, 2);
    for stream in 0..3 {
        cache.get_or_request_first(&path, stream);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        assert!(std::time::Instant::now() < deadline, "decode never arrived");
        cache.poll();
        if (0..3).all(|stream| cache.get_or_request(&path, stream).is_some()) {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(
        cache.in_progress.len(),
        3,
        "no stream should have finished yet"
    );
    let full = 120 * 48_000 * 2;
    assert!((0..3).all(|stream| cache.get_or_request(&path, stream).unwrap().len() < full));
}

#[test]
fn a_stretch_is_computed_in_background_and_dropped_once_unused() {
    let mut cache = MixBufferCache::spawn(48_000, 1);
    let sine: Arc<Vec<f32>> = Arc::new(
        (0..96_000)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
            .collect(),
    );
    let ask = |cache: &mut MixBufferCache| cache.stretched(&sine, 0..48_000, 2.0, 48_000, 1);
    assert!(matches!(ask(&mut cache), ClipAudio::Pending));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let stretched = loop {
        cache.poll();
        match ask(&mut cache) {
            ClipAudio::Ready(samples) => break samples,
            ClipAudio::Pending => {}
            _ => panic!("the stretch failed"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stretch never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let secs = stretched.len() as f64 / 48_000.0;
    assert!((secs - 0.5).abs() < 0.05, "1 s at 2x lasts {secs} s");

    cache.sweep_stretched();
    assert!(matches!(ask(&mut cache), ClipAudio::Ready(_)), "still used");
    cache.sweep_stretched();
    cache.sweep_stretched();
    assert!(
        matches!(ask(&mut cache), ClipAudio::Pending),
        "dropped when unused"
    );
}
