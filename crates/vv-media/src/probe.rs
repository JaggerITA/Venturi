//! Reading the metadata of a media.

use ffmpeg_next as ffmpeg;
use std::path::Path;
use std::sync::Once;
use vv_core::{FrameIdx, MediaMeta, Rational};

static INIT: Once = Once::new();

pub(crate) fn ensure_init() {
    INIT.call_once(|| {
        ffmpeg::init().expect("cannot initialize ffmpeg");
    });
}

/// Nominal fps of an audio-only media: it has no frames of its own, but
/// `source_in`/`source_out` and the gain keyframes are counted in source
/// frames all the same.
pub const AUDIO_ONLY_FPS: Rational = Rational::new(30, 1);

/// Nominal fps of an image: only a consistent unit for `source_in`/`source_out`
/// is needed.
pub const IMAGE_FPS: Rational = Rational::new(25, 1);

/// Above this the declared fps is not a frame rate but the container's
/// tick rate: VFR recordings (Android screen capture, some phone cameras)
/// carry `r_frame_rate = 90000`, and trusting it would inflate
/// `duration_frames` by five orders of magnitude.
const MAX_PLAUSIBLE_FPS: f64 = 1000.0;

/// Fallback when neither declared rate is usable.
const UNKNOWN_FPS: Rational = Rational::new(30, 1);

/// Ceiling for the rate measured on a VFR stream: beyond this the cost of
/// conforming it to CFR outweighs the smoothness gained.
const MAX_MEASURED_FPS: i32 = 120;

/// The rate at which the media is decoded and indexed. `r_frame_rate` is
/// the finest tick the stream can express, not its real rate: phone screen
/// captures declare 90000 there, so it is only believed when plausible.
pub(crate) fn media_fps(path: &Path, video: &ffmpeg::format::stream::Stream) -> Rational {
    let rate = video.rate();
    if let Some(fps) = plausible_fps(rate.numerator(), rate.denominator()) {
        return fps;
    }
    // A VFR stream gets conformed to CFR, and the average rate would drop
    // the bursts: a clip averaging 4 fps because the screen is still can
    // still hold 60 fps of real motion. The peak is what makes those
    // stretches play smoothly.
    measured_peak_fps(path)
        .or_else(|| {
            let avg = video.avg_frame_rate();
            plausible_fps(avg.numerator(), avg.denominator())
        })
        .unwrap_or(UNKNOWN_FPS)
}

fn plausible_fps(num: i32, den: i32) -> Option<Rational> {
    (num > 0 && den > 0 && f64::from(num) / f64::from(den) <= MAX_PLAUSIBLE_FPS)
        .then(|| Rational::new(num, den))
}

/// Frames in the densest one-second window, demuxing the whole file without
/// decoding. Memoized: `Decoder::open` needs the same value as `probe` and
/// is called again on every seek far from the playhead.
fn measured_peak_fps(path: &Path) -> Option<Rational> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<(std::path::PathBuf, u64, i64), Option<Rational>>>> =
        OnceLock::new();
    let meta = std::fs::metadata(path).ok()?;
    let key = (
        path.to_path_buf(),
        meta.len(),
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64),
    );
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&key) {
        return *hit;
    }
    let measured = peak_fps_of_window(&packet_times(path)?);
    cache.lock().unwrap().insert(key, measured);
    measured
}

fn packet_times(path: &Path) -> Option<Vec<f64>> {
    let mut input = ffmpeg::format::input(&path).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;
    let stream_index = stream.index();
    let time_base = seconds_per_tick(stream.time_base());
    let mut times: Vec<f64> = input
        .packets()
        .filter(|(stream, _)| stream.index() == stream_index)
        .filter_map(|(_, packet)| packet.pts().map(|pts| pts as f64 * time_base))
        .collect();
    times.sort_by(f64::total_cmp);
    Some(times)
}

fn peak_fps_of_window(times: &[f64]) -> Option<Rational> {
    if times.len() < 2 {
        return None;
    }
    let mut start = 0usize;
    let mut peak = 0usize;
    for end in 0..times.len() {
        while times[end] - times[start] >= 1.0 {
            start += 1;
        }
        peak = peak.max(end - start + 1);
    }
    if peak < 2 {
        return None;
    }
    Some(Rational::new(
        snapped_to_a_common_rate(peak as i32).min(MAX_MEASURED_FPS),
        1,
    ))
}

/// The count in the densest window is off by a frame or two — pts jitter at
/// one end of the window, a dropped frame at the other. A capture device
/// targets a standard rate, so the nearby one is the real answer.
fn snapped_to_a_common_rate(measured: i32) -> i32 {
    const COMMON: [i32; 8] = [24, 25, 30, 48, 50, 60, 90, 120];
    COMMON
        .into_iter()
        .filter(|&rate| (measured - rate).abs() * 10 <= rate)
        // On a tie the higher rate wins: conforming too low drops real
        // frames, too high only repeats them.
        .min_by_key(|&rate| ((measured - rate).abs(), -rate))
        .unwrap_or(measured)
}

pub fn probe(path: &Path) -> Result<MediaMeta, crate::MediaError> {
    ensure_init();
    let input = ffmpeg::format::input(&path)?;

    let video = match input.streams().best(ffmpeg::media::Type::Video) {
        Some(video) => {
            let fps = media_fps(path, &video);
            let decoder = ffmpeg::codec::context::Context::from_parameters(video.parameters())?
                .decoder()
                .video()?;
            Some((fps, decoder.width(), decoder.height()))
        }
        None => None,
    };

    let audio = input.streams().best(ffmpeg::media::Type::Audio);
    let (has_audio, sample_rate, channels) = match audio {
        Some(audio) => {
            let audio_decoder =
                ffmpeg::codec::context::Context::from_parameters(audio.parameters())?
                    .decoder()
                    .audio()?;
            (true, audio_decoder.rate(), audio_decoder.channels())
        }
        None => (false, 0, 0),
    };
    if video.is_none() && !has_audio {
        return Err(crate::MediaError::NoStream(path.display().to_string()));
    }

    let audio_streams = input
        .streams()
        .filter(|s| s.parameters().medium() == ffmpeg::media::Type::Audio)
        .count() as u16;
    let (fps, width, height) = video.unwrap_or((AUDIO_ONLY_FPS, 0, 0));
    let duration_secs = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    let nominal_duration_frames = (duration_secs * fps.as_f64()).round() as i64;
    // The container often declares more frames than the decoder actually
    // produces (drop frames, imprecise CFR index...): whoever asks for
    // frame `duration_frames - 1` as the "last frame" (e.g. the freeze in
    // `extrapolated_frame_for`) would get `None` forever if we blindly
    // trusted the arithmetic.
    let duration_frames = if video.is_some() {
        verify_last_decodable_frame(path, fps, nominal_duration_frames)
    } else {
        nominal_duration_frames
    };
    Ok(MediaMeta {
        duration_frames,
        fps,
        width,
        height,
        has_video: video.is_some(),
        has_audio,
        sample_rate,
        channels,
        audio_streams,
    })
}

/// Corrects `nominal` (duration*fps) to the true count of frames the stream
/// produces. Two passes, both on the tail of the file only: a demux (no
/// decoding) that finds the pts of the last video packet and of the last
/// keyframe, then decoding of the last GOP alone to confirm that that frame
/// really comes out of the decoder. It never raises `nominal`: if something
/// fails the arithmetic value is kept and the excess, if any, is ignored —
/// this fix corrects only the overestimate, it does not introduce a new
/// one.
fn verify_last_decodable_frame(path: &Path, fps: Rational, nominal: i64) -> i64 {
    let idx_of = |secs: f64| (secs * fps.as_f64()).round() as FrameIdx;
    let Some((last_packet_secs, last_key_secs)) = scan_tail(path, fps, nominal) else {
        return nominal;
    };
    match decode_from(path, last_key_secs) {
        Some(secs) => (idx_of(secs) + 1).min(nominal),
        // The final GOP does not decode: stop at the demux, which is
        // closer to the truth than the arithmetic anyway.
        None => (idx_of(last_packet_secs) + 1).min(nominal),
    }
}

/// `(pts of the last video packet, pts of the last keyframe)` in seconds,
/// demuxing from the tail of the stream. If the seek lands past the real
/// end (which is the case we are fixing) it re-reads from the start:
/// without decoding it costs a few ms even on long files.
fn scan_tail(path: &Path, fps: Rational, nominal: i64) -> Option<(f64, f64)> {
    const SAFETY_MARGIN_SECS: f64 = 15.0;
    let nominal_secs = nominal as f64 / fps.as_f64();
    for seek_secs in [(nominal_secs - SAFETY_MARGIN_SECS).max(0.0), 0.0] {
        let mut input = ffmpeg::format::input(&path).ok()?;
        let stream = input.streams().best(ffmpeg::media::Type::Video)?;
        let stream_index = stream.index();
        let time_base = seconds_per_tick(stream.time_base());
        let ts = (seek_secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
        if input.seek(ts, ..ts).is_err() {
            continue;
        }
        let mut last = None;
        let mut last_key = 0.0f64;
        for (stream, packet) in input.packets() {
            if stream.index() != stream_index {
                continue;
            }
            let Some(secs) = packet.pts().map(|pts| pts as f64 * time_base) else {
                continue;
            };
            last = Some(last.map_or(secs, |max: f64| max.max(secs)));
            if packet.is_key() {
                last_key = last_key.max(secs);
            }
        }
        if let Some(last) = last {
            return Some((last, last_key));
        }
        if seek_secs == 0.0 {
            break;
        }
    }
    None
}

/// Pts of the last frame the decoder produces starting from the keyframe at
/// `from_secs`, in seconds. No scaler: only the timestamp is needed, not
/// the pixels.
fn decode_from(path: &Path, from_secs: f64) -> Option<f64> {
    let mut input = ffmpeg::format::input(&path).ok()?;
    let stream = input.streams().best(ffmpeg::media::Type::Video)?;
    let stream_index = stream.index();
    let time_base = seconds_per_tick(stream.time_base());
    let mut context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .ok()?
        .decoder();
    context.set_threading(ffmpeg::threading::Config {
        kind: ffmpeg::threading::Type::Frame,
        count: 0,
        ..Default::default()
    });
    let mut decoder = context.video().ok()?;
    let ts = (from_secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
    input.seek(ts, ..ts).ok()?;

    let mut frame = ffmpeg::frame::Video::empty();
    let mut last: Option<f64> = None;
    let mut drain = |decoder: &mut ffmpeg::decoder::Video, last: &mut Option<f64>| {
        while decoder.receive_frame(&mut frame).is_ok() {
            let Some(secs) = frame.pts().map(|pts| pts as f64 * time_base) else {
                continue;
            };
            *last = Some(last.map_or(secs, |max: f64| max.max(secs)));
        }
    };
    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_ok() {
            drain(&mut decoder, &mut last);
        }
    }
    if decoder.send_eof().is_ok() {
        drain(&mut decoder, &mut last);
    }
    last
}

fn seconds_per_tick(time_base: ffmpeg::Rational) -> f64 {
    time_base.numerator() as f64 / time_base.denominator() as f64
}

/// Like `probe` but for an image: `duration_frames` is the sentinel
/// `vv_core::IMAGE_DURATION_FRAMES`.
pub fn probe_image(path: &Path) -> Result<MediaMeta, crate::MediaError> {
    ensure_init();
    let input = ffmpeg::format::input(&path)?;
    let video = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
    let decoder = ffmpeg::codec::context::Context::from_parameters(video.parameters())?
        .decoder()
        .video()?;
    Ok(MediaMeta {
        duration_frames: vv_core::IMAGE_DURATION_FRAMES,
        fps: IMAGE_FPS,
        width: decoder.width(),
        height: decoder.height(),
        has_video: true,
        has_audio: false,
        sample_rate: 0,
        channels: 0,
        audio_streams: 0,
    })
}

/// Info on an audio stream of the container, for multi-track import
/// (see `audio_streams`).
pub struct AudioStreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
}

/// All the audio streams in container order (not ffmpeg's "best"): this is
/// the index of `Clip::audio_stream_index`. A multi-audio file is imported
/// with one clip per stream.
pub fn audio_streams(path: &Path) -> Result<Vec<AudioStreamInfo>, crate::MediaError> {
    ensure_init();
    let input = ffmpeg::format::input(&path)?;
    let mut out = Vec::new();
    for stream in input.streams() {
        if stream.parameters().medium() != ffmpeg::media::Type::Audio {
            continue;
        }
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        out.push(AudioStreamInfo {
            sample_rate: decoder.rate(),
            channels: decoder.channels(),
        });
    }
    Ok(out)
}

/// Fingerprint (canonical path + size + mtime, FNV-1a) used as the
/// `content_hash` for the derived caches. It does not read the bytes: a file
/// overwritten with the same size and mtime would use a stale cache, a
/// tradeoff accepted for instant imports. Stable across restarts, unlike
/// `DefaultHasher`.
pub fn content_fingerprint(path: &Path) -> std::io::Result<u64> {
    let metadata = std::fs::metadata(path)?;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
        }
    };
    feed(canonical.to_string_lossy().as_bytes());
    feed(&metadata.len().to_le_bytes());
    feed(&mtime_secs.to_le_bytes());
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Android screen capture declares `r_frame_rate = 90000`, the
    /// container tick rate: believing it inflated `duration_frames` by five
    /// orders of magnitude and froze the preview.
    #[test]
    fn a_tick_rate_masquerading_as_r_frame_rate_is_not_plausible() {
        assert_eq!(plausible_fps(90000, 1), None);
        assert_eq!(plausible_fps(0, 0), None);
        assert_eq!(plausible_fps(60, 1), Some(Rational::new(60, 1)));
        assert_eq!(plausible_fps(30000, 1001), Some(Rational::new(30000, 1001)));
    }

    /// A clip that is still for most of its length but holds real motion in
    /// bursts: the average (~4 fps here) would make those stretches play in
    /// slideshow, the peak keeps them smooth.
    #[test]
    fn the_measured_rate_is_the_peak_not_the_average() {
        let mut times: Vec<f64> = (0..60).map(|i| f64::from(i) / 60.0).collect();
        times.extend((1..10).map(f64::from));
        assert_eq!(peak_fps_of_window(&times), Some(Rational::new(60, 1)));
    }

    #[test]
    fn a_rate_measured_off_by_jitter_snaps_to_the_standard_one() {
        assert_eq!(snapped_to_a_common_rate(61), 60);
        assert_eq!(snapped_to_a_common_rate(55), 60);
        assert_eq!(snapped_to_a_common_rate(31), 30);
        // Too far from any standard rate: it is taken as it is.
        assert_eq!(snapped_to_a_common_rate(40), 40);
    }

    #[test]
    fn the_measured_rate_is_capped() {
        let times: Vec<f64> = (0..500).map(|i| f64::from(i) / 500.0).collect();
        assert_eq!(
            peak_fps_of_window(&times),
            Some(Rational::new(MAX_MEASURED_FPS, 1))
        );
    }

    #[test]
    fn too_few_packets_to_measure_a_rate() {
        assert_eq!(peak_fps_of_window(&[]), None);
        assert_eq!(peak_fps_of_window(&[0.0]), None);
    }

    /// Reproduces the bug scenario: a container declaring more frames than
    /// the decoder actually produces (simulated here by passing
    /// `verify_last_decodable_frame` a `nominal` inflated past the true
    /// count, instead of a real broken container). It must correct it to
    /// the true last decodable frame, not trust the given value.
    #[test]
    fn verify_last_decodable_frame_corrects_an_inflated_nominal() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dieci_frame.mp4");
        crate::test_support::ffmpeg(
            &["-f", "lavfi", "-i", "testsrc=size=64x64:rate=10:duration=1", "-c:v", "libx264", "-pix_fmt", "yuv420p"],
            &path,
        );

        let fps = Rational::new(10, 1);
        let real = verify_last_decodable_frame(&path, fps, 10);
        let corrected = verify_last_decodable_frame(&path, fps, 10_000);

        assert_eq!(corrected, real, "an inflated nominal count must converge to the real last decodable frame");
        assert!(corrected <= 15, "10 real frames at 10fps: the corrected count must not stay near the inflated nominal ({corrected})");
    }

    /// Real case of the overestimate: the audio lasts longer than the video, so
    /// the container duration (the maximum across the streams) multiplied by
    /// the fps promises many more frames than the video has.
    #[test]
    fn probe_ignores_frames_promised_by_an_audio_track_longer_than_the_video() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audio_piu_lungo.mp4");
        crate::test_support::ffmpeg(
            &[
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=1",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=4",
                "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
            ],
            &path,
        );

        let meta = probe(&path).expect("probe failed");
        assert!(
            (meta.duration_frames - 25).abs() <= 2,
            "1 s of video at 25 fps, not the frames promised by 4 s of audio: {}",
            meta.duration_frames
        );
    }

    /// Generates a real x264 file (with AAC audio) via the ffmpeg CLI and checks
    /// that the probe reads coherent metadata. This is the primary use case
    /// stated by the user (compressed x264), not a mock.
    #[test]
    fn probe_reads_x264_metadata() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.mp4");

        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x360:rate=25:duration=2",
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

        let meta = probe(&path).expect("probe failed");
        assert_eq!(meta.width, 640);
        assert_eq!(meta.height, 360);
        assert_eq!(meta.fps, Rational::new(25, 1));
        assert!(meta.has_audio);
        assert_eq!(meta.sample_rate, 48000);
        assert_eq!(meta.audio_streams, 1);
        // ~2s at 25fps: tolerates a few rounding frames on the container.
        assert!((meta.duration_frames - 50).abs() <= 2);
    }

    #[test]
    fn probe_accepts_an_audio_only_file() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=2",
            ],
            &path,
        );

        let meta = probe(&path).expect("probe failed");
        assert!(!meta.has_video);
        assert!(meta.has_audio);
        assert_eq!((meta.width, meta.height), (0, 0));
        assert_eq!(meta.fps, AUDIO_ONLY_FPS);
        assert_eq!(meta.sample_rate, 44_100);
        assert_eq!(meta.duration_frames, 60, "2 s at nominal fps");
    }

    #[test]
    fn probe_image_reads_dimensions_and_reports_the_image_sentinel() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("still.png");
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=640x360:rate=1:duration=1",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &path,
        );

        let meta = probe_image(&path).expect("probe_image failed");
        assert_eq!((meta.width, meta.height), (640, 360));
        assert_eq!(meta.fps, IMAGE_FPS);
        assert!(meta.has_video);
        assert!(!meta.has_audio);
        assert_eq!(meta.duration_frames, vv_core::IMAGE_DURATION_FRAMES);
        assert!(meta.is_image());
    }

    #[test]
    fn probe_rejects_a_file_without_audio_or_video() {
        let dir = std::env::temp_dir().join("vv-media-probe-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("subtitles.srt");
        std::fs::write(&path, "1\n00:00:00,000 --> 00:00:01,000\nhello\n").unwrap();
        assert!(probe(&path).is_err());
    }

    #[test]
    fn content_fingerprint_is_stable_for_the_same_unchanged_file() {
        let dir = std::env::temp_dir().join("vv-media-fingerprint-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stable.bin");
        std::fs::write(&path, "test content").unwrap();

        let a = content_fingerprint(&path).unwrap();
        let b = content_fingerprint(&path).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn content_fingerprint_differs_for_different_sized_files() {
        let dir = std::env::temp_dir().join("vv-media-fingerprint-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("a.bin");
        let path_b = dir.join("b.bin");
        std::fs::write(&path_a, "short content").unwrap();
        std::fs::write(&path_b, "content much longer than the short one").unwrap();

        assert_ne!(
            content_fingerprint(&path_a).unwrap(),
            content_fingerprint(&path_b).unwrap()
        );
    }

    #[test]
    fn content_fingerprint_changes_when_the_file_is_rewritten_with_different_content() {
        let dir = std::env::temp_dir().join("vv-media-fingerprint-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rewritten.bin");

        std::fs::write(&path, "first version").unwrap();
        let before = content_fingerprint(&path).unwrap();

        // A different size guarantees the fingerprint changes even
        // if the filesystem has an mtime resolution too coarse
        // to record the write as "later" than the previous one
        // within the duration of the test.
        std::fs::write(&path, "second version, longer than the first").unwrap();
        let after = content_fingerprint(&path).unwrap();

        assert_ne!(before, after);
    }
}
