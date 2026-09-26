use super::*;

fn decode_first_frame(path: &Path) -> Result<Arc<FrameYuv420>, crate::MediaError> {
    let mut decoder = Decoder::open(path)?;
    decoder
        .next_frame()?
        .map(|(_, frame)| frame)
        .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))
}

#[test]
fn guess_matrix_uses_the_signaled_space_when_present() {
    assert_eq!(guess_matrix(color::Space::BT709, 240), ColorMatrix::Bt709);
    assert_eq!(
        guess_matrix(color::Space::BT2020NCL, 240),
        ColorMatrix::Bt2020
    );
    assert_eq!(
        guess_matrix(color::Space::SMPTE170M, 1080),
        ColorMatrix::Bt601,
        "an explicitly signalled SD matrix wins over the resolution heuristic"
    );
}

#[test]
fn guess_matrix_falls_back_to_a_resolution_heuristic_when_unspecified() {
    assert_eq!(
        guess_matrix(color::Space::Unspecified, 240),
        ColorMatrix::Bt601,
        "SD unsignalled: BT.601"
    );
    assert_eq!(
        guess_matrix(color::Space::Unspecified, 1080),
        ColorMatrix::Bt709,
        "HD unsignalled: BT.709"
    );
}

#[test]
fn guess_matrix_never_guesses_bt2020_from_the_heuristic() {
    // BT.2020 is too specific to be guessed: even at UHD resolutions,
    // without an explicit signal it stays on BT.709, never
    // BT.2020.
    assert_eq!(
        guess_matrix(color::Space::Unspecified, 2160),
        ColorMatrix::Bt709
    );
}

fn make_test_clip(name: &str, duration_secs: u32) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("vv-media-decode-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
            "-c:v",
            "libx264",
            "-g",
            "10", // short GOP: more keyframes to test the seek
            "-pix_fmt",
            "yuv420p",
        ],
        &path,
    );
    path
}

/// Phone screen capture: frames only when the picture changes, so the
/// pts leave long holes. 30 fps declared, 6 frames in 3.1 s.
fn make_vfr_test_clip(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("vv-media-decode-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=30:duration=4",
            "-vf",
            "select='eq(n,0)+eq(n,10)+eq(n,50)+eq(n,90)+eq(n,91)+eq(n,92)'",
            "-fps_mode",
            "passthrough",
            "-c:v",
            "libx264",
            "-g",
            "1",
            "-pix_fmt",
            "yuv420p",
        ],
        &path,
    );
    path
}

/// The cache demands every index of a range: on a VFR source the holes
/// between one pts and the next were never filled and the preview
/// waited forever for frames the stream does not contain.
#[test]
fn a_vfr_stream_is_decoded_as_a_contiguous_cfr_sequence() {
    let path = make_vfr_test_clip("vfr.mp4");
    let mut decoder = Decoder::open(&path).unwrap();
    let mut indices = Vec::new();
    while let Some((idx, _)) = decoder.next_frame().unwrap() {
        indices.push(idx);
    }
    assert_eq!(
        indices,
        (0..=92).collect::<Vec<_>>(),
        "expected the full CFR sequence from the 6 real frames"
    );
}

/// The holes are filled by sharing the held frame: a long still
/// stretch must not cost one full copy per CFR slot.
#[test]
fn the_frames_filling_a_hole_share_one_allocation() {
    let path = make_vfr_test_clip("vfr_shared.mp4");
    let mut decoder = Decoder::open(&path).unwrap();
    let mut frames = Vec::new();
    while let Some((_, frame)) = decoder.next_frame().unwrap() {
        frames.push(frame);
    }
    let distinct = frames
        .iter()
        .map(|f| Arc::as_ptr(f))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        distinct.len(),
        6,
        "expected the 6 allocations of the real frames"
    );
}

#[test]
fn a_vfr_stream_stays_contiguous_after_a_seek() {
    let path = make_vfr_test_clip("vfr_seek.mp4");
    let mut decoder = Decoder::open(&path).unwrap();
    decoder.seek_to_time(2.0).unwrap();
    let mut indices = Vec::new();
    while let Some((idx, _)) = decoder.next_frame().unwrap() {
        indices.push(idx);
    }
    assert!(
        indices.first().is_some_and(|&first| first <= 60),
        "the seek must land at 2 s or earlier: {:?}",
        indices.first()
    );
    assert!(
        indices.windows(2).all(|w| w[1] == w[0] + 1),
        "non-contiguous indices after the seek: {indices:?}"
    );
    assert_eq!(indices.last(), Some(&92));
}

fn make_test_clip_with_gop_and_bframes(
    name: &str,
    duration_secs: u32,
    gop: u32,
    bframes: u32,
) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("vv-media-decode-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
            "-c:v",
            "libx264",
            "-g",
            &gop.to_string(),
            "-keyint_min",
            &gop.to_string(),
            "-bf",
            &bframes.to_string(),
            "-pix_fmt",
            "yuv420p",
        ],
        &path,
    );
    path
}

#[test]
#[ignore = "manual measurement, not a correctness assertion"]
fn bench_decode_forward_through_a_large_gop() {
    let dir = std::env::temp_dir().join("vv-media-decode-bench");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("large_gop.mp4");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=1920x1080:rate=25:duration=12",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-g",
            "250",
            "-keyint_min",
            "250",
            "-pix_fmt",
            "yuv420p",
        ],
        &path,
    );

    let mut decoder = Decoder::open(&path).expect("open failed");
    decoder.seek_to_time(0.0).expect("seek failed");
    let start = std::time::Instant::now();
    let mut count = 0;
    while count < 249 {
        match decoder.next_frame().expect("decode failed") {
            Some(_) => count += 1,
            None => break,
        }
    }
    eprintln!(
        "decoded {count} frames (1920x1080) in {:?} ({:.1} fps)",
        start.elapsed(),
        count as f64 / start.elapsed().as_secs_f64()
    );
}

#[test]
fn decode_first_frame_of_x264_reads_correct_dimensions_and_pixels() {
    let path = make_test_clip("sample.mp4", 1);

    let frame = decode_first_frame(&path).expect("decode failed");
    assert_eq!(frame.width, 320);
    assert_eq!(frame.height, 240);
    assert_eq!(frame.y.len(), 320 * 240, "dense Y plane, 1 byte/pixel");
    // 4:2:0: chroma planes at half resolution (rounded up,
    // exact here because 320x240 is already even).
    assert_eq!(frame.u_width, 160);
    assert_eq!(frame.u_height, 120);
    assert_eq!(frame.u.len(), 160 * 120);
    assert_eq!(frame.v.len(), 160 * 120);

    // The testsrc pattern is never uniform: if we find more than one
    // distinct value in the Y plane, the stride/format are correct.
    let distinct: std::collections::HashSet<u8> = frame.y.iter().step_by(37).copied().collect();
    assert!(
        distinct.len() > 5,
        "the decoded pixels look degenerate: {distinct:?}"
    );
}

fn make_test_image(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("vv-media-decode-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);

    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=1:duration=1",
            "-frames:v",
            "1",
            "-update",
            "1",
        ],
        &path,
    );
    path
}

/// A PNG with transparency must carry it all the way to the decoded frame:
/// scaled to YUV420P the alpha would disappear and the transparent areas
/// would end up opaque, in the color left underneath in the file.
#[test]
fn a_transparent_png_keeps_its_alpha_channel() {
    let dir = std::env::temp_dir().join("vv-media-decode-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("half_transparent.png");
    // Left half opaque, right half fully transparent.
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=16x8:d=1",
            "-vf",
            "format=rgba,geq=r='255':g='0':b='0':a='if(lt(X,8),255,0)'",
            "-frames:v",
            "1",
            "-update",
            "1",
        ],
        &path,
    );

    let mut decoder = Decoder::open_image(&path).expect("image open failed");
    let (_, frame) = decoder.next_frame().unwrap().expect("frame expected");

    let alpha = frame
        .alpha
        .as_ref()
        .expect("a PNG with transparency must carry the alpha plane");
    assert_eq!(
        alpha.len(),
        (frame.width * frame.height) as usize,
        "alpha not subsampled"
    );
    let at = |x: u32, y: u32| alpha[(y * frame.width + x) as usize];
    assert_eq!(at(2, 4), 255, "left: opaque");
    assert_eq!(at(13, 4), 0, "right: transparent, not opaque");
}

/// An RGB frame (a PNG) always declares `color_range = JPEG`, but that
/// is the range of the RGB: the YUV sws derives from it is limited range, and
/// believing it full would wash out every imported image.
#[test]
fn a_png_is_reported_as_limited_range_because_that_is_what_the_scaler_produces() {
    let path = make_test_image("range.png");
    let mut decoder = Decoder::open_image(&path).expect("image open failed");
    let (_, frame) = decoder.next_frame().unwrap().expect("frame expected");

    assert!(!frame.full_range);
    assert!(
        frame.y.iter().all(|&y| (16..=235).contains(&y)),
        "values outside the limited range: not the declared range"
    );
}

/// A video without an alpha channel must not pay for a fourth, fully
/// opaque coverage plane: it costs cache memory and one texture upload
/// per frame.
#[test]
fn a_video_without_alpha_carries_no_alpha_plane() {
    let path = make_test_clip("no-alpha.mp4", 1);
    let mut decoder = Decoder::open(&path).expect("open failed");
    let (_, frame) = decoder.next_frame().unwrap().expect("frame expected");

    assert!(frame.alpha.is_none());
}

#[test]
fn open_image_decodes_correct_dimensions_and_pixels() {
    let path = make_test_image("still.png");
    let decoder = Decoder::open_image(&path).expect("image open failed");
    assert_eq!(decoder.width(), 320);
    assert_eq!(decoder.height(), 240);
}

/// An image has no "next frame": any requested `source_frame` (via
/// `seek_to_time`) or a sequential call to `next_frame` without a seek
/// must always return the same content, never `None` as a real video
/// would after the single available frame.
#[test]
fn open_image_returns_the_same_frame_for_any_requested_position() {
    let path = make_test_image("still_repeat.png");
    let mut decoder = Decoder::open_image(&path).expect("image open failed");

    decoder.seek_to_time(0.0).unwrap();
    let (idx0, frame0) = decoder.next_frame().unwrap().expect("frame expected");
    assert_eq!(idx0, 0);

    decoder.seek_to_time(120.0).unwrap();
    let (idx_far, frame_far) = decoder
        .next_frame()
        .unwrap()
        .expect("frame expected even far ahead in time");
    assert_eq!(
        idx_far,
        (120.0 * crate::probe::IMAGE_FPS.as_f64()).round() as FrameIdx
    );
    assert_eq!(
        frame_far.y, frame0.y,
        "the very same frame, whatever the position"
    );

    // Without a seek in between, next_frame keeps returning
    // something (never None) instead of behaving like a real EOF.
    let (idx_next, frame_next) = decoder
        .next_frame()
        .unwrap()
        .expect("never EOF for an image");
    assert!(idx_next > idx_far);
    assert_eq!(frame_next.y, frame0.y);
}

/// JPEGs decode into `yuvj420p`: the full range must be preserved, not
/// compressed to limited by the scaler.
#[test]
fn full_range_jpeg_keeps_its_luma_range() {
    let dir = std::env::temp_dir().join("vv-media-decode-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("white.jpg");
    crate::test_support::ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "color=white:size=64x64",
            "-frames:v",
            "1",
            "-update",
            "1",
        ],
        &path,
    );
    let mut decoder = Decoder::open_image(&path).expect("image open failed");
    let (_, frame) = decoder.next_frame().unwrap().expect("frame expected");
    assert!(frame.full_range);
    assert!(
        frame.y.iter().all(|&y| y >= 250),
        "compressed luma: {}",
        frame.y[0]
    );
}

#[test]
fn decode_first_frame_defaults_to_mpeg_limited_range_and_a_resolution_based_matrix() {
    // make_test_clip does not explicitly signal matrix/range (common
    // for generated/consumer content): 320x240 is below the 720p
    // threshold, it must fall back on BT.601 + limited range.
    let path = make_test_clip("colorspace.mp4", 1);
    let frame = decode_first_frame(&path).expect("decode failed");
    assert_eq!(frame.matrix, ColorMatrix::Bt601);
    assert!(
        !frame.full_range,
        "the default range for video must be limited (MPEG), not full"
    );
}

#[test]
fn decoder_sequential_frames_have_increasing_index() {
    let path = make_test_clip("sequential.mp4", 1);
    let mut decoder = Decoder::open(&path).unwrap();

    let mut last_idx = -1;
    let mut count = 0;
    while let Some((idx, _)) = decoder.next_frame().unwrap() {
        assert!(idx > last_idx, "frame indices must increase");
        last_idx = idx;
        count += 1;
    }
    // ~25fps for 1s: a few frames of tolerance on the last GOP.
    assert!((20..=30).contains(&count), "count={count}");
}

#[test]
fn decoder_seek_lands_near_target_and_decodes_forward() {
    let path = make_test_clip("seek.mp4", 3);
    let mut decoder = Decoder::open(&path).unwrap();

    decoder.seek_to_time(1.5).unwrap();
    let (idx, _) = decoder
        .next_frame()
        .unwrap()
        .expect("there should have been a frame after the seek");

    // The seek lands on the keyframe <= target: with GOP=10 at 25fps the
    // distance from frame 1.5s*25=37 does not exceed one GOP.
    assert!(idx <= 37, "idx={idx} should be <= the target");
    assert!(idx >= 37 - 10, "idx={idx} too far from the target");
}

/// Regression for a real bug, confirmed by the user on a
/// 1080p60fps file with B-frames: `avformat_seek_file`, even constraining
/// `max_ts` to the target (tried and verified ineffective: the mov/mp4
/// demuxer ignores it), can still land on a keyframe *after* the
/// target instead of on the preceding one, when the target falls a
/// frame or two from a keyframe boundary — the concrete case was
/// `target=749` landing on `idx=751` instead of on a much earlier
/// preceding keyframe, leaving the frames in between uncovered
/// forever (see `seek_to_time`, which now corrects itself by retrying
/// further back until it really lands `<=` the target). Not reproduced
/// by the synthetic content below (the real file that triggered the bug
/// had a B-frame structure this `testsrc` does not replicate), but the
/// contract (`idx <= target`) must be respected all the same, near
/// *every* keyframe boundary, not only far from them as in the test
/// above.
#[test]
fn decoder_seek_lands_at_or_before_target_near_every_keyframe_boundary() {
    let path = make_test_clip_with_gop_and_bframes("seek_boundaries.mp4", 4, 25, 3);
    // Keyframes at 0,25,50,75 (fps=25, g=25): a target 1/2/3 frames
    // before each one is the edge case that triggered the bug.
    for keyframe in [25, 50, 75] {
        for offset in [1, 2, 3] {
            let target = keyframe - offset;
            let mut decoder = Decoder::open(&path).unwrap();
            decoder.seek_to_time(target as f64 / 25.0).unwrap();
            let (idx, _) = decoder.next_frame().unwrap().expect("frame expected");
            assert!(
                idx <= target,
                "keyframe={keyframe} offset={offset} target={target}: \
                     landed on idx={idx}, past the target"
            );
        }
    }
}
