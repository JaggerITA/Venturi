//! Decoding a media: `next_frame` in sequence or after `seek_to_time`.
//! Every pixel format becomes dense 8-bit YUV420P — YUVA420P if the
//! source has an alpha channel (PNG, WebM with alpha, ProRes 4444), so
//! transparency makes it all the way to compositing; the conversion to RGB
//! is done by the shader. Matrix and range are read from the original frame
//! (scaling does not change them), with a resolution heuristic if missing.

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::color;
use ffmpeg_next as ffmpeg;
use std::path::Path;
use std::sync::Arc;
use vv_core::FrameIdx;

pub use vv_core::ColorMatrix;

/// 8-bit YUV420P frame, dense planes (no row padding).
#[derive(Clone)]
pub struct FrameYuv420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    /// 4:2:0 subsampled U/V planes: `plane_width(1)` x `plane_height(1)`
    /// dimensions of the scaled frame (ffmpeg rounds up on odd
    /// dimensions, not a plain `width/2`).
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub u_width: u32,
    pub u_height: u32,
    pub matrix: ColorMatrix,
    /// `true` = full range (0-255), `false` = limited (16-235/240), the norm.
    pub full_range: bool,
    /// Per-pixel coverage (`width`x`height`, not subsampled, *straight*
    /// non-premultiplied alpha): `Some` when the source format has an alpha
    /// channel (`format_has_alpha`), `None` when it is opaque. A PNG with
    /// transparency carries it all the way here, and without it its
    /// transparent areas would show the color the file left underneath —
    /// often black — instead of letting the clip below show through.
    pub alpha: Option<Vec<u8>>,
}

impl FrameYuv420 {
    /// Bytes used by the planes.
    pub fn byte_len(&self) -> usize {
        self.y.len() + self.u.len() + self.v.len() + self.alpha.as_ref().map_or(0, Vec::len)
    }
}

/// Bytes of an 8-bit YUV420 `width`x`height` frame, to estimate cache
/// budgets before having decoded it.
pub fn yuv420_frame_bytes(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

/// Matrix when the source does not signal it: BT.601 below 720 rows,
/// BT.709 above, like ffmpeg. BT.2020 is never guessed.
fn guess_matrix(space: color::Space, height: u32) -> ColorMatrix {
    match space {
        color::Space::BT709 => ColorMatrix::Bt709,
        color::Space::BT2020NCL | color::Space::BT2020CL => ColorMatrix::Bt2020,
        // Same BT.601 matrix. The others (rare) fall back on the heuristic.
        color::Space::SMPTE170M | color::Space::BT470BG => ColorMatrix::Bt601,
        _ => {
            if height >= 720 {
                ColorMatrix::Bt709
            } else {
                ColorMatrix::Bt601
            }
        }
    }
}

/// Decoder open on a single video stream of a media. Not `Sync`: every
/// decode thread (e.g. the decode-ahead in `playback`) owns its own
/// instance.
pub struct Decoder {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::codec::decoder::Video,
    scaler: Scaler,
    video_stream_index: usize,
    time_base: ffmpeg::Rational,
    fps: vv_core::Rational,
    /// After `send_eof` ffmpeg refuses another one before a flush: it only
    /// drains.
    eof_sent: bool,
    /// Frame decoded by `seek_to_time` to check where it landed:
    /// `next_frame` returns it first.
    pending: Option<(FrameIdx, Arc<FrameYuv420>)>,
    /// The single frame of an image: seek and `next_frame` always return it,
    /// the rest of the decode would treat EOF after one frame as an error.
    still_image: Option<Arc<FrameYuv420>>,
    /// Index growing on every `next_frame` on an image, as in a video.
    synthetic_idx: FrameIdx,
    /// Next index to hand out. The decoder emits a contiguous CFR sequence:
    /// a VFR source (phone screen capture) leaves holes between one pts and
    /// the next, and whoever caches by index would wait forever for frames
    /// the stream never produces.
    emit_idx: Option<FrameIdx>,
    /// Last emitted frame, repeated to fill those holes. Shared, not
    /// copied: a still stretch of a screen capture costs one frame however
    /// many CFR slots it spans.
    held: Option<Arc<FrameYuv420>>,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self, crate::MediaError> {
        crate::probe::ensure_init();

        let ictx = ffmpeg::format::input(&path)?;
        let video_stream = ictx
            .streams()
            .best(Type::Video)
            .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
        let video_stream_index = video_stream.index();
        let time_base = video_stream.time_base();
        let fps = crate::probe::media_fps(path, &video_stream);

        let mut decoder_ctx =
            ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?.decoder();
        // Multithreaded decode: a seek decodes from the last keyframe up to
        // the target, in parallel it is much faster.
        decoder_ctx.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Frame,
            count: 0,
            ..Default::default()
        });
        let decoder = decoder_ctx.video()?;

        let source_format = without_deprecated_range(decoder.format());
        let scaler = Scaler::get(
            source_format,
            decoder.width(),
            decoder.height(),
            target_format(source_format),
            decoder.width(),
            decoder.height(),
            Flags::BILINEAR,
        )?;

        Ok(Self {
            ictx,
            decoder,
            scaler,
            video_stream_index,
            time_base,
            fps,
            eof_sent: false,
            pending: None,
            still_image: None,
            synthetic_idx: 0,
            emit_idx: None,
            held: None,
        })
    }

    /// Like `open`, for a still image: decodes the single frame immediately.
    pub fn open_image(path: &Path) -> Result<Self, crate::MediaError> {
        let mut decoder = Self::open(path)?;
        // The same fps as `probe_image`, not the demuxer's (fictitious) one, or
        // the seek seconds would not match the media frames.
        decoder.fps = crate::probe::IMAGE_FPS;
        let frame = decoder
            .decode_next_frame()?
            .map(|(_, f)| f)
            .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
        decoder.still_image = Some(frame);
        Ok(decoder)
    }

    pub fn width(&self) -> u32 {
        self.decoder.width()
    }

    pub fn height(&self) -> u32 {
        self.decoder.height()
    }

    pub fn fps(&self) -> vv_core::Rational {
        self.fps
    }

    /// Seek to the keyframe `<= secs`, guaranteed: on mp4 with B-frames
    /// `avformat_seek_file` sometimes lands on the keyframe *after*, and the
    /// frames in between would never be decoded. The landing frame is decoded
    /// immediately (kept in `pending`) and, if it is past the target, the seek
    /// is retried further back, doubling the step.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<(), crate::MediaError> {
        let target_idx = (secs.max(0.0) * self.fps.as_f64()).round() as FrameIdx;
        if let Some(frame) = &self.still_image {
            // An image lands exactly on the target; `synthetic_idx` restarts from
            // there.
            self.pending = Some((target_idx, frame.clone()));
            self.synthetic_idx = target_idx + 1;
            return Ok(());
        }
        let mut ts = (secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
        // Initial backoff step: one second, doubled on every attempt.
        let mut step = i64::from(ffmpeg::ffi::AV_TIME_BASE);
        const MAX_RETRIES: u32 = 20;
        for _ in 0..MAX_RETRIES {
            self.ictx.seek(ts, ..ts)?;
            self.decoder.flush();
            self.eof_sent = false;
            self.pending = None;
            self.emit_idx = None;
            self.held = None;
            match self.decode_next_frame()? {
                Some((idx, frame)) if idx > target_idx && ts > 0 => {
                    ts = ts.saturating_sub(step).max(0);
                    step = step.saturating_mul(2);
                    let _ = frame; // discarded, retried further back
                }
                landed => {
                    self.pending = landed;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Decodes the next available video frame in presentation order.
    /// `Ok(None)` at the end of the stream.
    pub fn next_frame(
        &mut self,
    ) -> Result<Option<(FrameIdx, Arc<FrameYuv420>)>, crate::MediaError> {
        if let Some(frame) = self.still_image.clone() {
            if let Some(landed) = self.pending.take() {
                return Ok(Some(landed));
            }
            // Called without a preceding seek (`pending` empty): "advances" by
            // a synthetic frame, always the same image — see the docs of
            // `still_image`/`synthetic_idx`.
            let idx = self.synthetic_idx;
            self.synthetic_idx += 1;
            return Ok(Some((idx, frame)));
        }
        loop {
            if self.pending.is_none() {
                self.pending = self.decode_next_frame()?;
            }
            let Some((decoded_idx, _)) = self.pending.as_ref().map(|(i, f)| (*i, f)) else {
                return Ok(None);
            };
            let emit = *self.emit_idx.get_or_insert(decoded_idx);
            if decoded_idx == emit {
                let (_, frame) = self.pending.take().unwrap();
                self.emit_idx = Some(emit + 1);
                self.held = Some(frame.clone());
                return Ok(Some((emit, frame)));
            }
            if decoded_idx < emit {
                // Burst denser than the declared fps: its slot is already
                // gone. Dropping it keeps the index tied to the pts —
                // letting it through instead would drift the sequence past
                // the real duration and break sync with the audio.
                self.pending = None;
                continue;
            }
            match &self.held {
                Some(held) => {
                    let frame = held.clone();
                    self.emit_idx = Some(emit + 1);
                    return Ok(Some((emit, frame)));
                }
                // Nothing to repeat yet (first frame after a seek): start the
                // sequence where the stream actually is.
                None => self.emit_idx = Some(decoded_idx),
            }
        }
    }

    /// `next_frame` without `pending`.
    fn decode_next_frame(
        &mut self,
    ) -> Result<Option<(FrameIdx, Arc<FrameYuv420>)>, crate::MediaError> {
        let mut decoded = ffmpeg::frame::Video::empty();

        // EOF already sent: only the remaining frames are drained.
        if self.eof_sent {
            return Ok(if self.decoder.receive_frame(&mut decoded).is_ok() {
                Some(self.finish_frame(&mut decoded)?)
            } else {
                None
            });
        }

        let mut packet = ffmpeg::Packet::empty();

        loop {
            match packet.read(&mut self.ictx) {
                Ok(()) => {
                    if packet.stream() != self.video_stream_index {
                        continue;
                    }
                    self.decoder.send_packet(&packet)?;
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        return Ok(Some(self.finish_frame(&mut decoded)?));
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof()?;
                    self.eof_sent = true;
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        return Ok(Some(self.finish_frame(&mut decoded)?));
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn finish_frame(
        &mut self,
        decoded: &mut ffmpeg::frame::Video,
    ) -> Result<(FrameIdx, Arc<FrameYuv420>), crate::MediaError> {
        let pts = decoded.pts().unwrap_or(0);
        let secs =
            pts as f64 * self.time_base.numerator() as f64 / self.time_base.denominator() as f64;
        let idx = (secs * self.fps.as_f64()).round() as FrameIdx;
        let matrix = guess_matrix(decoded.color_space(), decoded.height());
        let format = decoded.format();
        // An RGB frame always declares itself `JPEG`, but that is the range of
        // the RGB, not of the YUV sws derives from it: there it converts to
        // limited unless told otherwise, and believing it full would wash out
        // every imported image.
        let full_range = !format_is_rgb(format)
            && (decoded.color_range() == color::Range::JPEG
                || without_deprecated_range(format) != format);
        decoded.set_format(without_deprecated_range(format));
        let frame = yuv420_from_decoded(&mut self.scaler, decoded, matrix, full_range)?;
        Ok((idx, Arc::new(frame)))
    }
}

/// The flags of `format`: ffmpeg-next does not expose
/// `AVPixFmtDescriptor::flags`, and `nb_components` alone would not
/// tell an alpha channel from the padding of an `0RGB`.
fn format_flags(format: Pixel) -> u64 {
    match format.descriptor() {
        Some(descriptor) => unsafe { (*descriptor.as_ptr()).flags },
        None => 0,
    }
}

/// `true` if `format` carries a real alpha channel (PNG, WebM with alpha,
/// ProRes 4444) and not a fourth padding channel.
fn format_has_alpha(format: Pixel) -> bool {
    format_flags(format) & ffmpeg::ffi::AV_PIX_FMT_FLAG_ALPHA as u64 != 0
}

/// `true` for RGB formats (an imported PNG, say), which the scaler must
/// convert to YUV instead of just rearranging the planes.
fn format_is_rgb(format: Pixel) -> bool {
    format_flags(format) & ffmpeg::ffi::AV_PIX_FMT_FLAG_RGB as u64 != 0
}

/// The format to scale to: YUVA420P preserves the alpha in the fourth plane,
/// YUV420P would throw it away.
fn target_format(source: Pixel) -> Pixel {
    if format_has_alpha(source) {
        Pixel::YUVA420P
    } else {
        Pixel::YUV420P
    }
}

/// The `yuvj*` formats make sws compress the full range into limited, but
/// the range is already carried by `FrameYuv420::full_range`: the conversion
/// treats them as `yuv*`, leaving the values intact.
fn without_deprecated_range(format: Pixel) -> Pixel {
    match format {
        Pixel::YUVJ420P => Pixel::YUV420P,
        Pixel::YUVJ422P => Pixel::YUV422P,
        Pixel::YUVJ444P => Pixel::YUV444P,
        Pixel::YUVJ440P => Pixel::YUV440P,
        Pixel::YUVJ411P => Pixel::YUV411P,
        other => other,
    }
}

fn yuv420_from_decoded(
    scaler: &mut Scaler,
    decoded: &ffmpeg::frame::Video,
    matrix: ColorMatrix,
    full_range: bool,
) -> Result<FrameYuv420, crate::MediaError> {
    let mut scaled = ffmpeg::frame::Video::empty();
    scaler.run(decoded, &mut scaled)?;

    // `sws_scale` may leave padding at the end of a row: each plane is recompacted.
    let pack_plane = |index: usize| -> Vec<u8> {
        let w = scaled.plane_width(index) as usize;
        let h = scaled.plane_height(index) as usize;
        let stride = scaled.stride(index);
        let plane = scaled.data(index);
        let mut out = Vec::with_capacity(w * h);
        for row in 0..h {
            let start = row * stride;
            out.extend_from_slice(&plane[start..start + w]);
        }
        out
    };

    let width = scaled.width();
    let height = scaled.height();
    let u_width = scaled.plane_width(1);
    let u_height = scaled.plane_height(1);
    let y = pack_plane(0);
    let u = pack_plane(1);
    let v = pack_plane(2);
    let alpha = (scaled.format() == Pixel::YUVA420P).then(|| pack_plane(3));

    Ok(FrameYuv420 {
        width,
        height,
        y,
        u,
        v,
        u_width,
        u_height,
        matrix,
        full_range,
        alpha,
    })
}

#[cfg(test)]
mod tests {
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
        assert_eq!(distinct.len(), 6, "expected the 6 allocations of the real frames");
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
        assert_eq!(alpha.len(), (frame.width * frame.height) as usize, "alpha not subsampled");
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
        let (idx_far, frame_far) = decoder.next_frame().unwrap().expect("frame expected even far ahead in time");
        assert_eq!(idx_far, (120.0 * crate::probe::IMAGE_FPS.as_f64()).round() as FrameIdx);
        assert_eq!(frame_far.y, frame0.y, "the very same frame, whatever the position");

        // Without a seek in between, next_frame keeps returning
        // something (never None) instead of behaving like a real EOF.
        let (idx_next, frame_next) = decoder.next_frame().unwrap().expect("never EOF for an image");
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
            &["-f", "lavfi", "-i", "color=white:size=64x64", "-frames:v", "1", "-update", "1"],
            &path,
        );
        let mut decoder = Decoder::open_image(&path).expect("image open failed");
        let (_, frame) = decoder.next_frame().unwrap().expect("frame expected");
        assert!(frame.full_range);
        assert!(frame.y.iter().all(|&y| y >= 250), "compressed luma: {}", frame.y[0]);
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
}


