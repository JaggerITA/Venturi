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
    /// Built from the first frame that needs one: the format the decoder
    /// declares before decoding is not always the one of its frames.
    scaler: Option<Scaler>,
    video_stream_index: usize,
    time_base: ffmpeg::Rational,
    fps: vv_core::Rational,
    /// After `send_eof` ffmpeg refuses another one before a flush: it only
    /// drains.
    eof_sent: bool,
    /// Frame decoded by `seek_to_time` to check where it landed:
    /// `next_frame` returns it first. Not converted yet: it may be skipped.
    pending: Option<(FrameIdx, ffmpeg::frame::Video)>,
    /// See `skip_before`.
    skip_before: Option<FrameIdx>,
    skipping_nonref: bool,
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
        });
        let decoder = decoder_ctx.video()?;

        Ok(Self {
            ictx,
            decoder,
            scaler: None,
            video_stream_index,
            time_base,
            fps,
            eof_sent: false,
            pending: None,
            skip_before: None,
            skipping_nonref: false,
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
        let (_, mut raw) = decoder
            .decode_next_raw()?
            .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
        decoder.still_image = Some(decoder.convert(&mut raw)?);
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
        if self.still_image.is_some() {
            // An image lands exactly on the target.
            self.synthetic_idx = target_idx;
            return Ok(());
        }
        self.skip_before = None;
        self.set_skip_nonref(false);
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
            match self.decode_next_raw()? {
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

    /// Frame the last `seek_to_time` landed on (its keyframe), until
    /// `next_frame` hands it out.
    pub fn landing(&self) -> Option<FrameIdx> {
        self.pending.as_ref().map(|(idx, _)| *idx)
    }

    /// After a seek: the frames before `idx` are wanted by nobody, only
    /// decoded as far as later frames reference them. `next_frame` skips them
    /// without converting them, and the non-reference ones (most B-frames)
    /// are not even decoded. Lasts until the first frame handed out.
    pub fn skip_before(&mut self, idx: FrameIdx) {
        if self.still_image.is_none() {
            self.skip_before = Some(idx);
        }
    }

    pub fn is_skipping(&self) -> bool {
        self.skip_before.is_some()
    }

    /// Advances a `skip_before` by at most `max` frames, so a caller can
    /// check between steps whether it still wants the frame it is heading to.
    /// Returns the last frame skipped.
    pub fn skip_some(&mut self, max: usize) -> Result<Option<FrameIdx>, crate::MediaError> {
        let mut last_skipped = None;
        for _ in 0..max {
            let Some(skip_before) = self.skip_before else {
                break;
            };
            if self.pending.is_none() {
                self.pending = self.decode_next_raw()?;
            }
            match self.landing() {
                Some(idx) if idx < skip_before => {
                    self.pending = None;
                    last_skipped = Some(idx);
                }
                // A frame to hand out, or the end of the stream. The skipped
                // stretch is no hole to fill with the last frame handed out.
                _ => {
                    self.skip_before = None;
                    self.set_skip_nonref(false);
                    self.emit_idx = None;
                    self.held = None;
                }
            }
        }
        Ok(last_skipped)
    }

    fn set_skip_nonref(&mut self, skip: bool) {
        if skip == self.skipping_nonref {
            return;
        }
        self.skipping_nonref = skip;
        // SAFETY: plain field of the open codec context, read by libavcodec
        // on every packet sent afterwards.
        unsafe {
            (*self.decoder.as_mut_ptr()).skip_frame = if skip {
                ffmpeg::ffi::AVDiscard::AVDISCARD_NONREF
            } else {
                ffmpeg::ffi::AVDiscard::AVDISCARD_DEFAULT
            };
        }
    }

    fn frame_idx(&self, pts: i64) -> FrameIdx {
        let secs =
            pts as f64 * self.time_base.numerator() as f64 / self.time_base.denominator() as f64;
        (secs * self.fps.as_f64()).round() as FrameIdx
    }

    /// Decodes the next available video frame in presentation order.
    /// `Ok(None)` at the end of the stream.
    pub fn next_frame(
        &mut self,
    ) -> Result<Option<(FrameIdx, Arc<FrameYuv420>)>, crate::MediaError> {
        if let Some(frame) = self.still_image.clone() {
            // "Advances" by a synthetic frame, always the same image — see the
            // docs of `still_image`/`synthetic_idx`.
            let idx = self.synthetic_idx;
            self.synthetic_idx += 1;
            return Ok(Some((idx, frame)));
        }
        self.skip_some(usize::MAX)?;
        loop {
            if self.pending.is_none() {
                self.pending = self.decode_next_raw()?;
            }
            let Some(decoded_idx) = self.landing() else {
                return Ok(None);
            };
            let emit = *self.emit_idx.get_or_insert(decoded_idx);
            if decoded_idx == emit {
                let (_, mut raw) = self.pending.take().unwrap();
                let frame = self.convert(&mut raw)?;
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

    /// The next decoded frame in presentation order, not converted yet.
    fn decode_next_raw(
        &mut self,
    ) -> Result<Option<(FrameIdx, ffmpeg::frame::Video)>, crate::MediaError> {
        let mut decoded = ffmpeg::frame::Video::empty();

        // EOF already sent: only the remaining frames are drained.
        if self.eof_sent {
            return Ok(if self.decoder.receive_frame(&mut decoded).is_ok() {
                Some(self.raw_frame(decoded))
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
                    // A non-reference frame is needed only by itself: before
                    // `skip_before` it can go undecoded. Without a pts, decode it.
                    let skip = self.skip_before.is_some_and(|skip_before| {
                        packet
                            .pts()
                            .is_some_and(|pts| self.frame_idx(pts) < skip_before)
                    });
                    self.set_skip_nonref(skip);
                    self.decoder.send_packet(&packet)?;
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        return Ok(Some(self.raw_frame(decoded)));
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof()?;
                    self.eof_sent = true;
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        return Ok(Some(self.raw_frame(decoded)));
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn raw_frame(&self, decoded: ffmpeg::frame::Video) -> (FrameIdx, ffmpeg::frame::Video) {
        (self.frame_idx(decoded.pts().unwrap_or(0)), decoded)
    }

    fn convert(
        &mut self,
        decoded: &mut ffmpeg::frame::Video,
    ) -> Result<Arc<FrameYuv420>, crate::MediaError> {
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
        Ok(Arc::new(frame))
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

/// The scaler converting `frame` to its target format, rebuilt when the
/// frames change format or size.
fn scaler_for<'a>(
    scaler: &'a mut Option<Scaler>,
    frame: &ffmpeg::frame::Video,
) -> Result<&'a mut Scaler, crate::MediaError> {
    let fits = scaler.as_ref().is_some_and(|s| {
        let input = s.input();
        input.format == frame.format()
            && input.width == frame.width()
            && input.height == frame.height()
    });
    if !fits {
        *scaler = Some(Scaler::get(
            frame.format(),
            frame.width(),
            frame.height(),
            target_format(frame.format()),
            frame.width(),
            frame.height(),
            Flags::BILINEAR,
        )?);
    }
    Ok(scaler.as_mut().unwrap())
}

fn yuv420_from_decoded(
    scaler: &mut Option<Scaler>,
    decoded: &ffmpeg::frame::Video,
    matrix: ColorMatrix,
    full_range: bool,
) -> Result<FrameYuv420, crate::MediaError> {
    // Already in the target format (most camera and screen recordings): sws
    // would only copy the planes once more, on the thread feeding the decoder.
    let converted;
    let scaled = if decoded.format() == target_format(decoded.format()) {
        decoded
    } else {
        let mut frame = ffmpeg::frame::Video::empty();
        scaler_for(scaler, decoded)?.run(decoded, &mut frame)?;
        converted = frame;
        &converted
    };

    // Rows may carry padding (decoder or sws): each plane is recompacted.
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
#[path = "tests/decode.rs"]
mod tests;
