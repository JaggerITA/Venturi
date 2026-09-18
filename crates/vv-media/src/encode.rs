//! Encoding + muxing verso file (H.264 + AAC in MP4), via `ffmpeg-next` —
//! simmetrico a `decode.rs` ma nella direzione opposta: chi chiama fornisce
//! frame I420 già compositati (BT.709 range limitato, convertiti su GPU dal
//! compositor) e campioni PCM già mixati (vedi la pipeline di export in
//! `vv-app`), `Encoder` converte l'audio nel formato nativo dell'encoder
//! AAC (tipicamente FLTP) e scrive il file. Pattern di invio
//! pacchetti/flush preso dagli esempi ufficiali di `ffmpeg-next`
//! (`examples/transcode-x264.rs`, `examples/transcode-audio.rs`):
//! `send_frame`/`receive_packet`/`write_interleaved`, `send_eof` per il
//! flush finale.

use ffmpeg::codec::{self, encoder};
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::format::{self, Pixel};
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg::{ChannelLayout, Dictionary};
use ffmpeg_next as ffmpeg;
use std::path::Path;

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
    /// F32 packed (formato/layout con cui arrivano i campioni da chi
    /// chiama) -> formato/layout richiesto dall'encoder AAC, stesso sample
    /// rate (nessuna conversione di frequenza qui: quella è già stata
    /// fatta a monte, in `vv-app`, per mixare più clip a un unico rate di
    /// progetto).
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

/// Apre un file di output e incapsula encoder video (H.264) + encoder audio
/// (AAC, opzionale) + muxer in un'unica interfaccia a "scrivi un frame alla
/// volta". Non `Sync`: un'istanza per export, come `Decoder` per la
/// decodifica.
pub struct Encoder {
    octx: format::context::Output,
    video: VideoState,
    audio: Option<AudioState>,
}

impl Encoder {
    /// `audio`: `Some((sample_rate, channels))` se il progetto ha una
    /// traccia audio da esportare, `None` per un export solo video.
    pub fn new(
        path: &Path,
        width: u32,
        height: u32,
        fps: vv_core::Rational,
        audio: Option<(u32, u16)>,
    ) -> Result<Self, crate::MediaError> {
        crate::probe::ensure_init();

        let mut octx = format::output(path)?;
        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

        // --- video: H.264 (libx264) ---
        let video_codec = encoder::find(codec::Id::H264)
            .ok_or_else(|| crate::MediaError::NoStream("encoder H264 non disponibile".into()))?;
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
        let mut x264_opts = Dictionary::new();
        x264_opts.set("preset", "medium");
        x264_opts.set("crf", "20");
        let video_encoder = video_ctx.open_with(x264_opts)?;
        let mut video_ost = video_ost;
        video_ost.set_parameters(&video_encoder);

        // --- audio: AAC (opzionale) ---
        let audio_state = match audio {
            Some((sample_rate, channels)) => {
                let audio_codec = encoder::find(codec::Id::AAC).ok_or_else(|| {
                    crate::MediaError::NoStream("encoder AAC non disponibile".into())
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
                let audio_encoder = audio_ctx.open_as(audio_codec)?;
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

                Some((
                    audio_stream_index,
                    audio_time_base,
                    AudioState {
                        encoder: audio_encoder,
                        resampler,
                        input_layout,
                        stream_index: audio_stream_index,
                        time_base: audio_time_base,
                        ost_time_base: audio_time_base, // aggiornato sotto, dopo write_header
                        frame_size,
                        channels,
                        rate: sample_rate,
                        pending: Vec::new(),
                        next_pts: 0,
                    },
                ))
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

        let audio = audio_state.map(|(idx, _, mut state)| {
            state.ost_time_base = octx.stream(idx).unwrap().time_base();
            state
        });

        Ok(Self { octx, video, audio })
    }

    /// `i420`: piani Y, U, V densi e consecutivi, croma `(w+1)/2 x (h+1)/2`
    /// (formato di `vv_render::Compositor::render_layers_i420`) — un
    /// avanzamento di un frame video in output.
    pub fn write_video_frame(&mut self, i420: &[u8]) -> Result<(), crate::MediaError> {
        let Self { octx, video, .. } = self;
        write_video_frame_impl(octx, video, i420)
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

    /// Flush di entrambi gli encoder (`send_eof` + drain, con l'ultimo
    /// blocco audio parziale imbottito di silenzio fino a `frame_size` —
    /// l'encoder AAC nativo non accetta frame di dimensione diversa da
    /// quella dichiarata) e `write_trailer`.
    pub fn finish(mut self) -> Result<(), crate::MediaError> {
        self.video.encoder.send_eof()?;
        drain_video_packets(&mut self.octx, &mut self.video)?;

        if let Some(mut audio) = self.audio.take() {
            if !audio.pending.is_empty() {
                let frame_len = audio.frame_size * audio.channels as usize;
                audio.pending.resize(frame_len, 0.0);
                let chunk = std::mem::take(&mut audio.pending);
                write_audio_chunk(&mut self.octx, &mut audio, &chunk)?;
            }
            audio.encoder.send_eof()?;
            drain_audio_packets(&mut self.octx, &mut audio)?;
        }

        self.octx.write_trailer()?;
        Ok(())
    }
}

fn write_video_frame_impl(
    octx: &mut format::context::Output,
    video: &mut VideoState,
    i420: &[u8],
) -> Result<(), crate::MediaError> {
    let width = video.encoder.width() as usize;
    let height = video.encoder.height() as usize;
    let chroma_width = width.div_ceil(2);
    let mut yuv = ffmpeg::frame::Video::new(Pixel::YUV420P, width as u32, height as u32);
    let (luma, chroma) = i420.split_at(width * height);
    let (u, v) = chroma.split_at(chroma.len() / 2);
    for (plane, (src, row_bytes)) in [(luma, width), (u, chroma_width), (v, chroma_width)]
        .into_iter()
        .enumerate()
    {
        let stride = yuv.stride(plane);
        let data = yuv.data_mut(plane);
        for (y, row) in src.chunks_exact(row_bytes).enumerate() {
            data[y * stride..y * stride + row_bytes].copy_from_slice(row);
        }
    }
    yuv.set_pts(Some(video.next_pts));
    yuv.set_kind(ffmpeg::picture::Type::None);
    video.next_pts += 1;

    video.encoder.send_frame(&yuv)?;
    drain_video_packets(octx, video)
}

fn drain_video_packets(
    octx: &mut format::context::Output,
    video: &mut VideoState,
) -> Result<(), crate::MediaError> {
    let mut packet = ffmpeg::Packet::empty();
    while video.encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(video.stream_index);
        packet.rescale_ts(video.time_base, video.ost_time_base);
        packet.write_interleaved(octx)?;
    }
    Ok(())
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
    drain_audio_packets(octx, audio)
}

fn drain_audio_packets(
    octx: &mut format::context::Output,
    audio: &mut AudioState,
) -> Result<(), crate::MediaError> {
    let mut packet = ffmpeg::Packet::empty();
    while audio.encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(audio.stream_index);
        packet.rescale_ts(audio.time_base, audio.ost_time_base);
        packet.write_interleaved(octx)?;
    }
    Ok(())
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
        let mut encoder = Encoder::new(&path, 16, 16, fps, None).unwrap();
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
        let mut encoder = Encoder::new(&path, 16, 16, fps, Some((48000, 2))).unwrap();
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
}
