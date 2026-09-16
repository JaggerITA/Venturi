//! Decodifica dell'intera traccia audio di un media in un buffer
//! interleaved f32.
//!
//! Milestone 2: l'MVP decodifica tutta la traccia in anticipo. Per singole
//! clip di durata "da editing" è un approccio semplice e corretto; lo
//! streaming a chunk per file molto lunghi è un'ottimizzazione futura, non
//! necessaria finché il player lavora su una clip alla volta.

use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type;
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg_next as ffmpeg;
use std::path::Path;

pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved f32, lunghezza = frame_totali * channels.
    pub samples: Vec<f32>,
}

/// Decodifica lo stream audio N-esimo del contenitore (`stream_index`,
/// stesso ordine di `probe::audio_streams` — non l'euristica "best" di
/// ffmpeg, che ne sceglierebbe uno solo ignorando le altre tracce di un
/// media multi-audio, vedi doc di `Clip::audio_stream_index`).
/// `Ok(None)` se il media non ha uno stream audio a quell'indice.
pub fn decode_audio_track(
    path: &Path,
    stream_index: usize,
) -> Result<Option<AudioBuffer>, crate::MediaError> {
    crate::probe::ensure_init();

    let mut ictx = ffmpeg::format::input(&path)?;
    let Some(audio_stream) = ictx
        .streams()
        .filter(|s| s.parameters().medium() == Type::Audio)
        .nth(stream_index)
    else {
        return Ok(None);
    };
    let audio_stream_index = audio_stream.index();

    let mut decoder = ffmpeg::codec::context::Context::from_parameters(audio_stream.parameters())?
        .decoder()
        .audio()?;

    let channel_layout = decoder.channel_layout();
    let sample_rate = decoder.rate();
    let channels = decoder.channels();

    let mut resampler = Resampler::get(
        decoder.format(),
        channel_layout,
        sample_rate,
        Sample::F32(SampleType::Packed),
        channel_layout,
        sample_rate,
    )?;

    let mut samples = Vec::new();
    let mut decoded = ffmpeg::frame::Audio::empty();
    let mut packet = ffmpeg::Packet::empty();

    loop {
        match packet.read(&mut ictx) {
            Ok(()) => {
                if packet.stream() != audio_stream_index {
                    continue;
                }
                decoder.send_packet(&packet)?;
                while decoder.receive_frame(&mut decoded).is_ok() {
                    push_resampled(&mut resampler, &decoded, &mut samples)?;
                }
            }
            Err(ffmpeg::Error::Eof) => {
                decoder.send_eof()?;
                while decoder.receive_frame(&mut decoded).is_ok() {
                    push_resampled(&mut resampler, &decoded, &mut samples)?;
                }
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(Some(AudioBuffer {
        sample_rate,
        channels,
        samples,
    }))
}

fn push_resampled(
    resampler: &mut Resampler,
    decoded: &ffmpeg::frame::Audio,
    out: &mut Vec<f32>,
) -> Result<(), crate::MediaError> {
    let mut resampled = ffmpeg::frame::Audio::empty();
    resampler.run(decoded, &mut resampled)?;

    let byte_len = resampled.samples() * resampled.channels() as usize * 4;
    let bytes = &resampled.data(0)[..byte_len];
    out.extend(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]])),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn decode_audio_track_reads_correct_length_and_is_not_silent() {
        let dir = std::env::temp_dir().join("vv-media-audio-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.mp4");

        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1",
                "-c:a",
                "aac",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

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

        let status = Command::new("ffmpeg")
            .args([
                "-y",
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
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

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
