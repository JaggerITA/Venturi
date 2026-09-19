//! Decodifica di tracce audio intere in f32 interleaved. La traccia sta
//! tutta in RAM; la versione streaming la consegna a blocchi così il mixer
//! può suonarne l'inizio prima della fine del decode.

use ffmpeg::ChannelLayout;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type;
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg_next as ffmpeg;
use std::ops::ControlFlow;
use std::path::Path;

pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved f32, lunghezza = frame_totali * channels.
    pub samples: Vec<f32>,
}

/// Decodifica lo stream audio N-esimo (ordine di `probe::audio_streams`).
/// `Ok(None)` se non esiste.
pub fn decode_audio_track(
    path: &Path,
    stream_index: usize,
) -> Result<Option<AudioBuffer>, crate::MediaError> {
    let mut samples = Vec::new();
    let formats = decode_audio_streams_streaming(path, &[stream_index], None, |_, _, chunk| {
        samples.extend_from_slice(chunk);
        ControlFlow::Continue(())
    })?;
    Ok(formats[0].map(|(sample_rate, channels)| AudioBuffer {
        sample_rate,
        channels,
        samples,
    }))
}

struct StreamDecoder {
    decoder: ffmpeg::decoder::Audio,
    resampler: Resampler,
    layout: ChannelLayout,
    in_rate: u32,
    out_rate: u32,
    channels: u16,
}

impl StreamDecoder {
    fn open(
        stream: &ffmpeg::format::stream::Stream,
        out_rate: Option<u32>,
    ) -> Result<Self, crate::MediaError> {
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        let layout = decoder_channel_layout(&decoder);
        let in_rate = decoder.rate();
        let out_rate = out_rate.unwrap_or(in_rate);
        let resampler = Resampler::get(
            decoder.format(),
            layout,
            in_rate,
            Sample::F32(SampleType::Packed),
            layout,
            out_rate,
        )?;
        Ok(Self {
            channels: decoder.channels(),
            decoder,
            resampler,
            layout,
            in_rate,
            out_rate,
        })
    }

    /// Ricampiona e consegna tutti i frame pronti nel decoder.
    fn drain(
        &mut self,
        slot: usize,
        samples: &mut Vec<f32>,
        on_chunk: &mut impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, crate::MediaError> {
        let mut decoded = ffmpeg::frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            // `Resampler::run` alloca l'uscita grande quanto l'ingresso, che
            // non basta quando si ricampiona verso l'alto.
            let capacity =
                decoded.samples() * self.out_rate as usize / self.in_rate.max(1) as usize + 64;
            let mut resampled =
                ffmpeg::frame::Audio::new(Sample::F32(SampleType::Packed), capacity, self.layout);
            run_resampler(&mut self.resampler, &mut decoded, &mut resampled)?;
            if emit(slot, self.channels, &resampled, samples, on_chunk).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn flush(
        &mut self,
        slot: usize,
        samples: &mut Vec<f32>,
        on_chunk: &mut impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, crate::MediaError> {
        while self.resampler.delay().is_some() {
            let mut resampled =
                ffmpeg::frame::Audio::new(Sample::F32(SampleType::Packed), 4096, self.layout);
            self.resampler.flush(&mut resampled)?;
            if resampled.samples() == 0 {
                break;
            }
            if emit(slot, self.channels, &resampled, samples, on_chunk).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

fn emit(
    slot: usize,
    channels: u16,
    frame: &ffmpeg::frame::Audio,
    samples: &mut Vec<f32>,
    on_chunk: &mut impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
) -> ControlFlow<()> {
    samples.clear();
    append_f32(frame, samples);
    if samples.is_empty() {
        ControlFlow::Continue(())
    } else {
        on_chunk(slot, channels, samples)
    }
}

/// Più stream in una sola lettura, a blocchi: `on_chunk(i, canali,
/// campioni)` con `i` indice in `stream_indices`; `Break` interrompe.
/// `out_rate` ricampiona con un contesto swresample per stream, senza
/// discontinuità tra blocchi. Ritorna `(rate, canali)` per stream, `None`
/// se non esiste.
pub fn decode_audio_streams_streaming(
    path: &Path,
    stream_indices: &[usize],
    out_rate: Option<u32>,
    mut on_chunk: impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
) -> Result<Vec<Option<(u32, u16)>>, crate::MediaError> {
    crate::probe::ensure_init();

    let mut ictx = ffmpeg::format::input(&path)?;
    let audio_streams: Vec<_> = ictx
        .streams()
        .filter(|s| s.parameters().medium() == Type::Audio)
        .collect();
    // (indice nel contenitore, decoder) per slot.
    let mut decoders: Vec<Option<(usize, StreamDecoder)>> = stream_indices
        .iter()
        .map(|&i| {
            audio_streams
                .get(i)
                .map(|stream| Ok((stream.index(), StreamDecoder::open(stream, out_rate)?)))
                .transpose()
        })
        .collect::<Result<_, crate::MediaError>>()?;
    drop(audio_streams);
    let formats = decoders
        .iter()
        .map(|d| d.as_ref().map(|(_, d)| (d.out_rate, d.channels)))
        .collect();
    if decoders.iter().all(Option::is_none) {
        return Ok(formats);
    }

    let mut packet = ffmpeg::Packet::empty();
    let mut samples = Vec::new();
    loop {
        match packet.read(&mut ictx) {
            Ok(()) => {
                let Some((slot, (_, stream))) = decoders
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(slot, d)| Some((slot, d.as_mut()?)))
                    .find(|(_, (i, _))| *i == packet.stream())
                else {
                    continue;
                };
                stream.decoder.send_packet(&packet)?;
                if stream.drain(slot, &mut samples, &mut on_chunk)?.is_break() {
                    return Ok(formats);
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    for (slot, entry) in decoders.iter_mut().enumerate() {
        let Some((_, stream)) = entry else {
            continue;
        };
        stream.decoder.send_eof()?;
        if stream.drain(slot, &mut samples, &mut on_chunk)?.is_break()
            || stream.flush(slot, &mut samples, &mut on_chunk)?.is_break()
        {
            break;
        }
    }
    Ok(formats)
}

/// Layout canali del decoder, con uno di default al posto di uno non
/// specificato (es. PCM in MKV): swresample altrimenti rifiuta ogni frame
/// con "Input changed".
pub(crate) fn decoder_channel_layout(decoder: &ffmpeg::decoder::Audio) -> ChannelLayout {
    let layout = decoder.channel_layout();
    if layout.is_empty() {
        ChannelLayout::default(i32::from(decoder.channels()))
    } else {
        layout
    }
}

/// `Resampler::run` dopo aver allineato il layout del frame a quello del
/// resampler (vedi `decoder_channel_layout`).
pub(crate) fn run_resampler(
    resampler: &mut Resampler,
    decoded: &mut ffmpeg::frame::Audio,
    resampled: &mut ffmpeg::frame::Audio,
) -> Result<(), crate::MediaError> {
    if decoded.channel_layout().is_empty() {
        decoded.set_channel_layout(resampler.input().channel_layout);
    }
    resampler.run(decoded, resampled)?;
    Ok(())
}

fn append_f32(frame: &ffmpeg::frame::Audio, out: &mut Vec<f32>) {
    let byte_len = frame.samples() * frame.channels() as usize * 4;
    out.extend(
        frame.data(0)[..byte_len]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_ne_bytes(*b)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PCM stereo in MKV ha layout canali "unknown": falliva con
    /// "Input changed".
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

        let audio = decode_audio_track(&path, 0).unwrap().expect("audio atteso");
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

        let audio = decode_audio_track(&path, 0).unwrap().expect("audio atteso");
        assert_eq!(audio.sample_rate, 48000);
        assert_eq!(audio.channels, 1);

        let total_frames = audio.samples.len() / audio.channels as usize;
        // ~1s a 48kHz: l'encoder AAC aggiunge un po' di priming/padding.
        assert!(
            (45_000..=52_000).contains(&total_frames),
            "total_frames={total_frames}"
        );

        // Un seno a 440Hz non è silenzioso: il picco deve essere ben sopra 0.
        let peak = audio.samples.iter().cloned().fold(0.0_f32, f32::max);
        assert!(peak > 0.1, "peak={peak}, atteso un segnale non silenzioso");
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
        // Un seno continuo: nessun salto tra campioni vicini ai confini dei blocchi.
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
        // Interleaved: il primo blocco di ogni stream arriva ben prima della fine.
        let first_of = |slot| order.iter().position(|&s| s == slot).unwrap();
        assert!([0, 1, 3].iter().all(|&s| first_of(s) < order.len() / 4), "{order:?}");
    }

    /// Un file con *due* stream audio (caso reale: mix stereo + 5.1
    /// separato) deve poter decodificare l'uno o l'altro in base a
    /// `stream_index`, non sempre "il migliore" secondo ffmpeg — qui
    /// distinti per sample_rate (44100 vs 48000) per verificarlo senza
    /// analisi spettrale.
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

        let first = decode_audio_track(&path, 0).unwrap().expect("stream 0 atteso");
        assert_eq!(first.sample_rate, 44100);

        let second = decode_audio_track(&path, 1).unwrap().expect("stream 1 atteso");
        assert_eq!(second.sample_rate, 48000);

        assert!(
            decode_audio_track(&path, 2).unwrap().is_none(),
            "nessuno stream audio all'indice 2"
        );
    }
}
