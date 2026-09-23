use super::*;

/// Stereo PCM in MKV has an "unknown" channel layout: it used to fail
/// with "Input changed".
#[test]
fn decode_audio_track_handles_unspecified_channel_layout() {
    let dir = std::env::temp_dir().join("vv-media-audio-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pcm_unknown_layout.mkv");

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
        ],
        &path,
    );

    let audio = decode_audio_track(&path, 0).unwrap().expect("audio expected");
    assert_eq!(audio.channels, 2);
    assert_eq!(audio.samples.len(), 48_000 * 2);
}

#[test]
fn decode_audio_track_reads_correct_length_and_is_not_silent() {
    let dir = std::env::temp_dir().join("vv-media-audio-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sample.mp4");

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-c:a",
            "aac",
        ],
        &path,
    );

    let audio = decode_audio_track(&path, 0).unwrap().expect("audio expected");
    assert_eq!(audio.sample_rate, 48000);
    assert_eq!(audio.channels, 1);

    let total_frames = audio.samples.len() / audio.channels as usize;
    // ~1s at 48kHz: the AAC encoder adds some priming/padding.
    assert!(
        (45_000..=52_000).contains(&total_frames),
        "total_frames={total_frames}"
    );

    // A 440Hz sine is not silent: the peak must be well above 0.
    let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
    assert!(peak > 0.1, "peak={peak}, expected a non-silent signal");
}

#[test]
fn streaming_decode_resamples_continuously_in_many_chunks() {
    let dir = std::env::temp_dir().join("vv-media-audio-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("streaming_44k.wav");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=2",
        ],
        &path,
    );

    let mut chunks = 0;
    let mut samples = Vec::new();
    let formats = decode_audio_streams_streaming(&path, &[0], Some(48_000), |_, channels, chunk| {
        assert_eq!(channels, 1);
        chunks += 1;
        samples.extend_from_slice(chunk);
        ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(formats, vec![Some((48_000, 1))]);
    assert!(chunks > 1, "chunks={chunks}");
    assert!((samples.len() as i64 - 96_000).abs() < 100, "len={}", samples.len());
    // A continuous sine: no jumps between samples near the chunk boundaries.
    let max_step = samples.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
    assert!(max_step < 0.1, "max_step={max_step}");
}

#[test]
fn streaming_decode_stops_when_the_callback_breaks() {
    let dir = std::env::temp_dir().join("vv-media-audio-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("streaming_break.wav");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=2",
        ],
        &path,
    );

    let mut chunks = 0;
    decode_audio_streams_streaming(&path, &[0], None, |_, _, _| {
        chunks += 1;
        ControlFlow::Break(())
    })
    .unwrap();
    assert_eq!(chunks, 1);
}

#[test]
fn streaming_decode_delivers_every_requested_stream_in_one_pass() {
    let dir = std::env::temp_dir().join("vv-media-audio-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("streaming_three.mkv");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=660:sample_rate=44100:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=3",
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

    let mut order = Vec::new();
    let mut lengths = [0usize; 3];
    let formats = decode_audio_streams_streaming(&path, &[2, 0, 5, 1], Some(48_000), |slot, _, chunk| {
        order.push(slot);
        let i = [0, 1, usize::MAX, 2][slot];
        lengths[i] += chunk.len();
        ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(formats, vec![Some((48_000, 1)), Some((48_000, 1)), None, Some((48_000, 1))]);
    for len in lengths {
        assert!((len as i64 - 144_000).abs() < 3_000, "{lengths:?}");
    }
    // Interleaved: the first chunk of each stream arrives well before the end.
    let first_of = |slot| order.iter().position(|&s| s == slot).unwrap();
    assert!([0, 1, 3].iter().all(|&s| first_of(s) < order.len() / 4), "{order:?}");
}

/// A file with *two* audio streams (real case: stereo mix + separate
/// 5.1) must be able to decode one or the other based on
/// `stream_index`, not always "the best one" according to ffmpeg — here
/// they are told apart by sample_rate (44100 vs 48000) to check it
/// without spectral analysis.
#[test]
fn decode_audio_track_selects_the_requested_stream_index_not_just_the_best() {
    let dir = std::env::temp_dir().join("vv-media-audio-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("two_streams.mp4");

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=1",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-c:a",
            "aac",
        ],
        &path,
    );

    let first = decode_audio_track(&path, 0).unwrap().expect("stream 0 expected");
    assert_eq!(first.sample_rate, 44100);

    let second = decode_audio_track(&path, 1).unwrap().expect("stream 1 expected");
    assert_eq!(second.sample_rate, 48000);

    assert!(
        decode_audio_track(&path, 2).unwrap().is_none(),
        "no audio stream at index 2"
    );
}
