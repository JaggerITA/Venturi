//! Encoding + mux in MP4 (H.264 x264/NVENC + AAC). Riceve frame I420 già
//! compositati (BT.709 limited) e PCM già mixato; converte solo l'audio nel
//! formato nativo dell'encoder AAC.

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

    /// Dal più veloce al più lento.
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
            // A parità di CRF qualità simile a "medium", ~3× più veloce e
            // file ~2× più grande.
            Self::X264 => "superfast",
            Self::Nvenc => "p5",
        }
    }

    /// Prova ad aprire davvero l'encoder (risultato in cache): NVENC è
    /// compilato in molte build di ffmpeg anche dove non c'è una GPU
    /// NVIDIA, e la prima apertura può costare un secondo.
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
    /// Uno di `codec.presets()`.
    pub preset: String,
    /// CRF per x264, CQ per NVENC: più basso = qualità più alta.
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
        // FDK è non-free: mai nel binario statico redistribuibile.
        if cfg!(feature = "static-ffmpeg") && self == Self::FdkAac {
            return false;
        }
        crate::probe::ensure_init();
        encoder::find_by_name(self.ffmpeg_name()).is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSettings {
    pub codec: AudioCodec,
    pub bitrate_kbps: u32,
    /// Solo per `AudioCodec::Aac`: coder "fast" invece di "twoloop", molto
    /// più rapido in stereo a qualità un po' inferiore.
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
    /// Time base dell'encoder (1/fps: un pts in unità = un frame), diverso
    /// da quello dello stream di output (`ost_time_base`) su cui vanno
    /// riscalati i pacchetti prima di scriverli (muxer MP4).
    time_base: ffmpeg::Rational,
    ost_time_base: ffmpeg::Rational,
    next_pts: i64,
}

struct AudioState {
    encoder: encoder::Audio,
    /// F32 packed -> formato dell'encoder AAC, stesso rate (il ricampionamento
    /// è già fatto a monte).
    resampler: Resampler,
    input_layout: ChannelLayout,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    ost_time_base: ffmpeg::Rational,
    /// Campioni per canale che l'encoder AAC si aspetta in ogni frame
    /// (fisso per l'encoder nativo `aac`, non a frame size variabile).
    frame_size: usize,
    channels: u16,
    rate: u32,
    /// Campioni interleaved accumulati in attesa di raggiungere un multiplo
    /// di `frame_size * channels`.
    pending: Vec<f32>,
    next_pts: i64,
}

/// Encoder video + audio opzionale + muxer, un frame alla volta. Uno per
/// export.
pub struct Encoder {
    octx: format::context::Output,
    video: VideoState,
    audio: Option<AudioState>,
}

impl Encoder {
    /// `audio`: `Some((sample_rate, channels, impostazioni))` se il progetto
    /// ha una traccia audio da esportare, `None` per un export solo video.
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
                "encoder {} non disponibile",
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
        // ffmpeg-next non ha setter per primaries/trc.
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
                // Senza bit_rate a 0 NVENC ignora `cq` e resta sul bitrate
                // di default del contesto.
                video_ctx.set_bit_rate(0);
                video_opts.set("rc", "vbr");
                video_opts.set("cq", &quality);
            }
        }
        let video_encoder = video_ctx.open_with(video_opts)?;
        let mut video_ost = video_ost;
        video_ost.set_parameters(&video_encoder);

        // --- audio: AAC (opzionale) ---
        let audio_state = match audio {
            Some((sample_rate, channels, settings)) => {
                let audio_codec =
                    encoder::find_by_name(settings.codec.ffmpeg_name()).ok_or_else(|| {
                        crate::MediaError::NoStream(format!(
                            "encoder {} non disponibile",
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
                                "nessun formato campione supportato dall'encoder AAC".into(),
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
                        // Aggiornato dopo `write_header`.
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

        // Il muxer può normalizzare il time_base dichiarato in fase di
        // `write_header`: quello *effettivo* dello stream di output va
        // riletto da lì, non assunto uguale a quello dell'encoder.
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

    /// `i420`: piani Y, U, V densi e consecutivi, croma `(w+1)/2 x (h+1)/2`
    /// (formato di `vv_render::Compositor::render_layers_i420`) — un
    /// avanzamento di un frame video in output.
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
    /// `samples`: PCM f32 interleaved, già al sample rate/canali dichiarati
    /// in `new`. Bufferizza internamente ai confini di frame AAC
    /// (`frame_size`); no-op se il progetto non ha audio.
    pub fn write_audio_samples(&mut self, samples: &[f32]) -> Result<(), crate::MediaError> {
        let Self { octx, audio, .. } = self;
        let Some(audio) = audio else {
            return Ok(());
        };
        // Mai `drain` dalla testa di `pending` blocco per blocco: con tutto
        // l'audio dell'export in un colpo diventa quadratico (minuti).
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

    /// Flush degli encoder e `write_trailer`. L'ultimo blocco audio va
    /// imbottito di silenzio: l'AAC nativo accetta solo frame di `frame_size`.
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

/// Scrive nel muxer i pacchetti già pronti di `encoder`.
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

/// Copia un piano denso (righe di `row_width` byte) in un piano di `frame`,
/// che può avere uno stride più largo.
pub(crate) fn fill_plane(frame: &mut ffmpeg::frame::Video, plane: usize, src: &[u8], row_width: usize) {
    let stride = frame.stride(plane);
    let data = frame.data_mut(plane);
    for (y, row) in src.chunks_exact(row_width.max(1)).enumerate() {
        data[y * stride..y * stride + row.len()].copy_from_slice(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verifica il file prodotto rileggendolo con le funzioni di decodifica
    // già esistenti/testate in questo stesso crate (`probe`/`Decoder`/
    // `decode_audio_track`), invece di introdurre una dipendenza da
    // ffprobe/JSON solo per i test: lo stesso principio "dogfooding" già
    // implicito nel resto della test suite (i fixture generati con
    // `ffmpeg` CLI vengono verificati decodificandoli con questo crate).

    fn solid_i420(width: usize, height: usize, [y, u, v]: [u8; 3]) -> Vec<u8> {
        let chroma = width.div_ceil(2) * height.div_ceil(2);
        let mut data = vec![y; width * height];
        data.extend(std::iter::repeat_n(u, chroma));
        data.extend(std::iter::repeat_n(v, chroma));
        data
    }

    #[test]
    fn encodes_video_only_file_with_correct_dimensions_and_frame_count() {
        let dir = std::env::temp_dir().join("vv-media-encode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("video_only.mp4");

        let fps = vv_core::Rational::new(25, 1);
        let mut encoder =
            Encoder::new(&path, 16, 16, fps, &VideoSettings::default(), None).unwrap();
        // Rosso in BT.709 range limitato.
        let red = solid_i420(16, 16, [63, 102, 240]);
        for _ in 0..25 {
            encoder.write_video_frame(&red).unwrap();
        }
        encoder.finish().unwrap();

        let meta = crate::probe::probe(&path).unwrap();
        assert_eq!(meta.width, 16);
        assert_eq!(meta.height, 16);
        assert!(!meta.has_audio, "solo video, niente audio");

        let mut decoder = crate::decode::Decoder::open(&path).unwrap();
        let mut count = 0;
        while decoder.next_frame().unwrap().is_some() {
            count += 1;
        }
        // Con B-frame (preset x264 di default) il riordino encoder/muxer
        // può accorciare di un frame quel che si rilegge dal container:
        // stessa tolleranza già usata altrove in questo crate per
        // arrotondamenti GOP/container, non un conteggio esatto.
        assert!((24..=25).contains(&count), "count={count}");
    }

    #[test]
    fn encodes_video_and_audio_streams_together() {
        let dir = std::env::temp_dir().join("vv-media-encode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("video_audio.mp4");

        let fps = vv_core::Rational::new(25, 1);
        let mut encoder = Encoder::new(
            &path,
            16,
            16,
            fps,
            &VideoSettings::default(),
            Some((48000, 2, &AudioSettings::default())),
        )
        .unwrap();
        let black = solid_i420(16, 16, [16, 128, 128]);
        for _ in 0..25 {
            encoder.write_video_frame(&black).unwrap();
        }
        // 1s di audio stereo a 48kHz, un semplice seno generato a mano.
        let samples: Vec<f32> = (0..48000 * 2)
            .map(|i| {
                let t = (i / 2) as f32 / 48000.0;
                (t * 440.0 * std::f32::consts::TAU).sin() * 0.5
            })
            .collect();
        encoder.write_audio_samples(&samples).unwrap();
        encoder.finish().unwrap();

        let meta = crate::probe::probe(&path).unwrap();
        assert!(meta.has_audio);
        assert_eq!(meta.channels, 2);

        let audio = crate::audio::decode_audio_track(&path, 0)
            .unwrap()
            .expect("audio atteso");
        let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "peak={peak}, atteso un segnale non silenzioso");
    }

    /// Ogni combinazione di encoder disponibile su questa macchina deve
    /// produrre un file con entrambi gli stream.
    #[test]
    fn encodes_with_every_available_codec_and_preset_choice() {
        let dir = std::env::temp_dir().join("vv-media-encode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let fps = vv_core::Rational::new(25, 1);
        let frame = solid_i420(256, 256, [63, 102, 240]);
        let samples = vec![0.1_f32; 48000 * 2];
        let audio_choices = [
            AudioSettings::default(),
            AudioSettings {
                fast_coder: true,
                bitrate_kbps: 192,
                ..AudioSettings::default()
            },
            AudioSettings {
                codec: AudioCodec::FdkAac,
                ..AudioSettings::default()
            },
        ];
        for codec in VideoCodec::ALL.into_iter().filter(|c| c.is_available()) {
            for (i, audio) in audio_choices
                .iter()
                .filter(|a| a.codec.is_available())
                .enumerate()
            {
                let path = dir.join(format!("{codec:?}-{i}.mp4"));
                let video = VideoSettings {
                    codec,
                    preset: codec.presets()[0].into(),
                    quality: 30,
                };
                let mut encoder =
                    Encoder::new(&path, 256, 256, fps, &video, Some((48000, 2, audio))).unwrap();
                for _ in 0..10 {
                    encoder.write_video_frame(&frame).unwrap();
                }
                encoder.write_audio_samples(&samples).unwrap();
                encoder.finish().unwrap();

                let meta = crate::probe::probe(&path).unwrap();
                assert_eq!((meta.width, meta.height), (256, 256), "{codec:?} {audio:?}");
                assert!(meta.has_audio, "{codec:?} {audio:?}");
            }
        }
    }
}
