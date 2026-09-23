//! Encoding + muxing into MP4 (H.264 x264/NVENC + AAC). It receives already
//! composited I420 frames (BT.709 limited) and already mixed PCM; it only
//! converts the audio into the AAC encoder's native format.

use ffmpeg::codec::{self, encoder};
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::format::{self, Pixel};
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg::{ChannelLayout, Dictionary};
use ffmpeg_next as ffmpeg;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
    X264,
    Nvenc,
}

impl VideoCodec {
    pub const ALL: [Self; 2] = [Self::X264, Self::Nvenc];

    fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::X264 => "libx264",
            Self::Nvenc => "h264_nvenc",
        }
    }

    /// From fastest to slowest.
    pub fn presets(self) -> &'static [&'static str] {
        match self {
            Self::X264 => &[
                "ultrafast",
                "superfast",
                "veryfast",
                "faster",
                "fast",
                "medium",
                "slow",
                "slower",
                "veryslow",
            ],
            Self::Nvenc => &["p1", "p2", "p3", "p4", "p5", "p6", "p7"],
        }
    }

    pub fn default_preset(self) -> &'static str {
        match self {
            // At equal CRF, quality similar to "medium", ~3× faster and
            // ~2× larger files.
            Self::X264 => "superfast",
            Self::Nvenc => "p5",
        }
    }

    /// Tries to actually open the encoder (result cached): NVENC is
    /// compiled into many ffmpeg builds even where there is no NVIDIA GPU,
    /// and the first open can cost a second.
    pub fn is_available(self) -> bool {
        static CACHE: [OnceLock<bool>; 2] = [OnceLock::new(), OnceLock::new()];
        *CACHE[self as usize].get_or_init(|| {
            crate::probe::ensure_init();
            let Some(codec) = encoder::find_by_name(self.ffmpeg_name()) else {
                return false;
            };
            let Ok(mut ctx) = codec::context::Context::new_with_codec(codec)
                .encoder()
                .video()
            else {
                return false;
            };
            ctx.set_width(256);
            ctx.set_height(256);
            ctx.set_format(Pixel::YUV420P);
            ctx.set_time_base(ffmpeg::Rational::new(1, 25));
            ctx.open().is_ok()
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSettings {
    pub codec: VideoCodec,
    /// One of `codec.presets()`.
    pub preset: String,
    /// CRF for x264, CQ for NVENC: lower = higher quality.
    pub quality: u8,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            codec: VideoCodec::X264,
            preset: VideoCodec::X264.default_preset().into(),
            quality: 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    FdkAac,
}

impl AudioCodec {
    pub const ALL: [Self; 2] = [Self::Aac, Self::FdkAac];

    fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::FdkAac => "libfdk_aac",
        }
    }

    pub fn is_available(self) -> bool {
        crate::probe::ensure_init();
        encoder::find_by_name(self.ffmpeg_name()).is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSettings {
    pub codec: AudioCodec,
    pub bitrate_kbps: u32,
    /// Only for `AudioCodec::Aac`: "fast" coder instead of "twoloop", much
    /// quicker in stereo at slightly lower quality.
    pub fast_coder: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            codec: AudioCodec::Aac,
            bitrate_kbps: 128,
            fast_coder: false,
        }
    }
}

struct VideoState {
    encoder: encoder::Video,
    stream_index: usize,
    /// Encoder time base (1/fps: one pts unit = one frame), different from
    /// the output stream's (`ost_time_base`), onto which the packets must be
    /// rescaled before writing them (MP4 muxer).
    time_base: ffmpeg::Rational,
    ost_time_base: ffmpeg::Rational,
    next_pts: i64,
}

struct AudioState {
    encoder: encoder::Audio,
    /// F32 packed -> AAC encoder format, same rate (resampling is already
    /// done upstream).
    resampler: Resampler,
    input_layout: ChannelLayout,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    ost_time_base: ffmpeg::Rational,
    /// Samples per channel the AAC encoder expects in each frame
    /// (fixed for the native `aac` encoder, not variable frame size).
    frame_size: usize,
    channels: u16,
    rate: u32,
    /// Interleaved samples accumulated while waiting to reach a multiple
    /// of `frame_size * channels`.
    pending: Vec<f32>,
    next_pts: i64,
}

/// Video encoder + optional audio + muxer, one frame at a time. One per
/// export.
pub struct Encoder {
    octx: format::context::Output,
    video: VideoState,
    audio: Option<AudioState>,
}

impl Encoder {
    /// `audio`: `Some((sample_rate, channels, settings))` if the project
    /// has an audio track to export, `None` for a video-only export.
    pub fn new(
        path: &Path,
        width: u32,
        height: u32,
        fps: vv_core::Rational,
        video: &VideoSettings,
        audio: Option<(u32, u16, &AudioSettings)>,
    ) -> Result<Self, crate::MediaError> {
        crate::probe::ensure_init();

        let mut octx = format::output(path)?;
        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

        let video_codec = encoder::find_by_name(video.codec.ffmpeg_name()).ok_or_else(|| {
            crate::MediaError::NoStream(format!(
                "encoder {} not available",
                video.codec.ffmpeg_name()
            ))
        })?;
        let video_ost = octx.add_stream(video_codec)?;
        let video_stream_index = video_ost.index();

        let video_time_base = ffmpeg::Rational::new(fps.den, fps.num);
        let mut video_ctx = codec::context::Context::new_with_codec(video_codec)
            .encoder()
            .video()?;
        video_ctx.set_width(width);
        video_ctx.set_height(height);
        video_ctx.set_format(Pixel::YUV420P);
        video_ctx.set_time_base(video_time_base);
        video_ctx.set_frame_rate(Some(ffmpeg::Rational::new(fps.num, fps.den)));
        video_ctx.set_colorspace(ffmpeg::color::Space::BT709);
        video_ctx.set_color_range(ffmpeg::color::Range::MPEG);
        // ffmpeg-next has no setter for primaries/trc.
        unsafe {
            let raw = video_ctx.as_mut_ptr();
            (*raw).color_primaries = ffmpeg::ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*raw).color_trc = ffmpeg::ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        }
        if global_header {
            video_ctx.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let mut video_opts = Dictionary::new();
        video_opts.set("preset", &video.preset);
        let quality = video.quality.to_string();
        match video.codec {
            VideoCodec::X264 => video_opts.set("crf", &quality),
            VideoCodec::Nvenc => {
                // Without bit_rate at 0, NVENC ignores `cq` and stays on the
                // context's default bitrate.
                video_ctx.set_bit_rate(0);
                video_opts.set("rc", "vbr");
                video_opts.set("cq", &quality);
            }
        }
        let video_encoder = video_ctx.open_with(video_opts)?;
        let mut video_ost = video_ost;
        video_ost.set_parameters(&video_encoder);

        // --- audio: AAC (optional) ---
        let audio_state = match audio {
            Some((sample_rate, channels, settings)) => {
                let audio_codec =
                    encoder::find_by_name(settings.codec.ffmpeg_name()).ok_or_else(|| {
                        crate::MediaError::NoStream(format!(
                            "encoder {} not available",
                            settings.codec.ffmpeg_name()
                        ))
                    })?;
                let audio_ost = octx.add_stream(audio_codec)?;
                let audio_stream_index = audio_ost.index();

                let input_layout = if channels >= 2 {
                    ChannelLayout::STEREO
                } else {
                    ChannelLayout::MONO
                };

                let mut audio_ctx = codec::context::Context::new_with_codec(audio_codec)
                    .encoder()
                    .audio()?;
                audio_ctx.set_rate(sample_rate as i32);
                audio_ctx.set_bit_rate(settings.bitrate_kbps as usize * 1000);
                audio_ctx.set_channel_layout(input_layout);
                audio_ctx.set_format(
                    audio_codec
                        .audio()?
                        .formats()
                        .and_then(|mut formats| formats.next())
                        .ok_or_else(|| {
                            crate::MediaError::NoStream(
                                "no sample format supported by the AAC encoder".into(),
                            )
                        })?,
                );
                let audio_time_base = ffmpeg::Rational::new(1, sample_rate as i32);
                audio_ctx.set_time_base(audio_time_base);
                if global_header {
                    audio_ctx.set_flags(codec::Flags::GLOBAL_HEADER);
                }
                let mut audio_opts = Dictionary::new();
                if settings.codec == AudioCodec::Aac {
                    let coder = if settings.fast_coder { "fast" } else { "twoloop" };
                    audio_opts.set("aac_coder", coder);
                }
                let audio_encoder = audio_ctx.open_as_with(audio_codec, audio_opts)?;
                let mut audio_ost = audio_ost;
                audio_ost.set_parameters(&audio_encoder);

                let out_format = audio_encoder.format();
                let out_layout = audio_encoder.channel_layout();
                let resampler = Resampler::get(
                    Sample::F32(SampleType::Packed),
                    input_layout,
                    sample_rate,
                    out_format,
                    out_layout,
                    sample_rate,
                )?;
                let frame_size = (audio_encoder.frame_size() as usize).max(1);

                Some(AudioState {
                        encoder: audio_encoder,
                        resampler,
                        input_layout,
                        stream_index: audio_stream_index,
                        time_base: audio_time_base,
                        // Updated after `write_header`.
                        ost_time_base: audio_time_base,
                        frame_size,
                        channels,
                        rate: sample_rate,
                        pending: Vec::new(),
                        next_pts: 0,
                    })
            }
            None => None,
        };

        octx.write_header()?;

        // The muxer may normalize the time_base declared during
        // `write_header`: the *effective* one of the output stream must be
        // read back from there, not assumed equal to the encoder's.
        let video_ost_time_base = octx.stream(video_stream_index).unwrap().time_base();
        let video = VideoState {
            encoder: video_encoder,
            stream_index: video_stream_index,
            time_base: video_time_base,
            ost_time_base: video_ost_time_base,
            next_pts: 0,
        };

        let audio = audio_state.map(|mut state| {
            state.ost_time_base = octx.stream(state.stream_index).unwrap().time_base();
            state
        });

        Ok(Self { octx, video, audio })
    }

    /// `i420`: dense and consecutive Y, U, V planes, chroma `(w+1)/2 x (h+1)/2`
    /// (the format of `vv_render::Compositor::render_layers_i420`) — one
    /// video frame of output advanced.
    pub fn write_video_frame(&mut self, i420: &[u8]) -> Result<(), crate::MediaError> {
        let video = &mut self.video;
        let width = video.encoder.width() as usize;
        let height = video.encoder.height() as usize;
        let chroma_width = width.div_ceil(2);
        let mut yuv = ffmpeg::frame::Video::new(Pixel::YUV420P, width as u32, height as u32);
        let (luma, chroma) = i420.split_at(width * height);
        let (u, v) = chroma.split_at(chroma.len() / 2);
        fill_plane(&mut yuv, 0, luma, width);
        fill_plane(&mut yuv, 1, u, chroma_width);
        fill_plane(&mut yuv, 2, v, chroma_width);
        yuv.set_pts(Some(video.next_pts));
        yuv.set_kind(ffmpeg::picture::Type::None);
        video.next_pts += 1;

        video.encoder.send_frame(&yuv)?;
        drain_packets(
            &mut self.octx,
            &mut video.encoder,
            video.stream_index,
            video.time_base,
            video.ost_time_base,
        )
    }
    /// `samples`: interleaved f32 PCM, already at the sample rate/channels declared
    /// in `new`. Buffers internally at AAC frame boundaries
    /// (`frame_size`); no-op if the project has no audio.
    pub fn write_audio_samples(&mut self, samples: &[f32]) -> Result<(), crate::MediaError> {
        let Self { octx, audio, .. } = self;
        let Some(audio) = audio else {
            return Ok(());
        };
        // Never `drain` from the head of `pending` block by block: with all
        // the export audio at once it becomes quadratic (minutes).
        let mut pending = std::mem::take(&mut audio.pending);
        pending.extend_from_slice(samples);
        let frame_len = audio.frame_size * audio.channels as usize;
        let mut chunks = pending.chunks_exact(frame_len);
        for chunk in &mut chunks {
            write_audio_chunk(octx, audio, chunk)?;
        }
        audio.pending = chunks.remainder().to_vec();
        Ok(())
    }

    /// Flushes the encoders and `write_trailer`. The last audio block must be
    /// padded with silence: native AAC only accepts frames of `frame_size`.
    pub fn finish(mut self) -> Result<(), crate::MediaError> {
        self.video.encoder.send_eof()?;
        let video = &mut self.video;
        drain_packets(
            &mut self.octx,
            &mut video.encoder,
            video.stream_index,
            video.time_base,
            video.ost_time_base,
        )?;

        if let Some(mut audio) = self.audio.take() {
            if !audio.pending.is_empty() {
                let frame_len = audio.frame_size * audio.channels as usize;
                audio.pending.resize(frame_len, 0.0);
                let chunk = std::mem::take(&mut audio.pending);
                write_audio_chunk(&mut self.octx, &mut audio, &chunk)?;
            }
            audio.encoder.send_eof()?;
            drain_packets(
                &mut self.octx,
                &mut audio.encoder,
                audio.stream_index,
                audio.time_base,
                audio.ost_time_base,
            )?;
        }

        self.octx.write_trailer()?;
        Ok(())
    }
}

fn write_audio_chunk(
    octx: &mut format::context::Output,
    audio: &mut AudioState,
    samples: &[f32],
) -> Result<(), crate::MediaError> {
    let channels = audio.channels as usize;
    let frame_len = samples.len() / channels.max(1);
    let mut src = ffmpeg::frame::Audio::new(
        Sample::F32(SampleType::Packed),
        frame_len,
        audio.input_layout,
    );
    src.set_rate(audio.rate);
    {
        let data = src.data_mut(0);
        for (bytes, &sample) in data.chunks_exact_mut(4).zip(samples.iter()) {
            bytes.copy_from_slice(&sample.to_ne_bytes());
        }
    }

    let mut resampled = ffmpeg::frame::Audio::empty();
    audio.resampler.run(&src, &mut resampled)?;
    resampled.set_pts(Some(audio.next_pts));
    audio.next_pts += frame_len as i64;

    audio.encoder.send_frame(&resampled)?;
    drain_packets(
        octx,
        &mut audio.encoder,
        audio.stream_index,
        audio.time_base,
        audio.ost_time_base,
    )
}

/// Writes the packets already ready from `encoder` into the muxer.
pub(crate) fn drain_packets(
    octx: &mut format::context::Output,
    encoder: &mut encoder::Encoder,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    ost_time_base: ffmpeg::Rational,
) -> Result<(), crate::MediaError> {
    let mut packet = ffmpeg::Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(stream_index);
        packet.rescale_ts(time_base, ost_time_base);
        packet.write_interleaved(octx)?;
    }
    Ok(())
}

/// Copies a dense plane (rows of `row_width` bytes) into a plane of `frame`,
/// which may have a wider stride.
pub(crate) fn fill_plane(frame: &mut ffmpeg::frame::Video, plane: usize, src: &[u8], row_width: usize) {
    let stride = frame.stride(plane);
    let data = frame.data_mut(plane);
    for (y, row) in src.chunks_exact(row_width.max(1)).enumerate() {
        data[y * stride..y * stride + row.len()].copy_from_slice(row);
    }
}

#[cfg(test)]
#[path = "tests/encode.rs"]
mod tests;
