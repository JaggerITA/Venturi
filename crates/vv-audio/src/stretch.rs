//! Time-stretch con pitch preservato per lo speed change, via il filtro
//! `rubberband` di libavfilter (ffmpeg-next::filter) — già linkato nel
//! ffmpeg di sistema (`--enable-librubberband`), nessun binding extra.
//!
//! Elabora l'intero buffer in un colpo solo (non è uno stretch realtime a
//! blocchi): il chiamante lavora già su un buffer audio intero
//! pre-decodificato in RAM (vedi `vv-app::player::Player`/
//! `vv_media::decode_audio_track`), quindi non serve un filtro a bassa
//! latenza per campione — un singolo passaggio batch è più semplice e
//! sufficientemente veloce (va comunque chiamato fuori dal thread UI,
//! vedi i chiamanti).

use ffmpeg_next as ffmpeg;
use ffmpeg::channel_layout::ChannelLayout;
use ffmpeg::format::sample::{Sample as SampleFormat, Type as SampleType};
use std::sync::Once;

static INIT: Once = Once::new();

fn ensure_init() {
    INIT.call_once(|| {
        ffmpeg::init().expect("impossibile inizializzare ffmpeg");
    });
}

const CHUNK_FRAMES: usize = 4096;

/// Applica il filtro `rubberband` a un buffer audio interleaved f32,
/// restituendo il buffer risultante allo stesso `sample_rate`/`channels`
/// ma con durata moltiplicata per `1/tempo` e pitch preservato:
/// `tempo=2.0` dimezza la durata (l'audio suona 2x più veloce), `tempo=4.0`
/// la riduce a un quarto.
pub fn stretch_samples(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    tempo: f64,
) -> Result<Vec<f32>, String> {
    if !(tempo > 0.0) {
        return Err(format!("tempo di stretch non valido: {tempo}"));
    }
    ensure_init();

    let format = SampleFormat::F32(SampleType::Packed);
    let layout = ChannelLayout::default(channels as i32);

    let mut graph = ffmpeg::filter::Graph::new();
    let args = format!(
        "time_base=1/{sample_rate}:sample_rate={sample_rate}:sample_fmt={}:channel_layout=0x{:x}",
        format.name(),
        layout.bits()
    );
    graph
        .add(
            &ffmpeg::filter::find("abuffer").ok_or("filtro abuffer non trovato")?,
            "in",
            &args,
        )
        .map_err(|e| e.to_string())?;
    graph
        .add(
            &ffmpeg::filter::find("abuffersink").ok_or("filtro abuffersink non trovato")?,
            "out",
            "",
        )
        .map_err(|e| e.to_string())?;

    graph
        .output("in", 0)
        .and_then(|p| p.input("out", 0))
        .and_then(|p| p.parse(&format!("rubberband=tempo={tempo}")))
        .map_err(|e| e.to_string())?;
    graph.validate().map_err(|e| e.to_string())?;

    let channels_usize = channels as usize;
    let total_frames = samples.len() / channels_usize.max(1);

    let mut out = Vec::new();
    let mut pos = 0;
    while pos < total_frames {
        let n = CHUNK_FRAMES.min(total_frames - pos);
        let mut frame = ffmpeg::frame::Audio::new(format, n, layout);
        frame.set_rate(sample_rate);
        let start = pos * channels_usize;
        frame.plane_mut::<f32>(0)[..n * channels_usize]
            .copy_from_slice(&samples[start..start + n * channels_usize]);

        graph
            .get("in")
            .ok_or("pad 'in' mancante")?
            .source()
            .add(&frame)
            .map_err(|e| e.to_string())?;
        drain_filtered(&mut graph, channels_usize, &mut out)?;
        pos += n;
    }
    graph
        .get("in")
        .ok_or("pad 'in' mancante")?
        .source()
        .flush()
        .map_err(|e| e.to_string())?;
    drain_filtered(&mut graph, channels_usize, &mut out)?;

    Ok(out)
}

fn drain_filtered(
    graph: &mut ffmpeg::filter::Graph,
    channels: usize,
    out: &mut Vec<f32>,
) -> Result<(), String> {
    let mut filtered = ffmpeg::frame::Audio::empty();
    while graph
        .get("out")
        .ok_or("pad 'out' mancante")?
        .sink()
        .frame(&mut filtered)
        .is_ok()
    {
        let n = filtered.samples();
        out.extend_from_slice(&filtered.plane::<f32>(0)[..n * channels]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, sample_rate: u32, secs: f32) -> Vec<f32> {
        let n = (sample_rate as f32 * secs) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin())
            .collect()
    }

    #[test]
    fn stretch_at_tempo_2x_roughly_halves_duration() {
        let sample_rate = 48_000;
        let samples = sine(440.0, sample_rate, 2.0); // mono, 2s
        let stretched = stretch_samples(&samples, sample_rate, 1, 2.0).unwrap();

        let original_frames = samples.len();
        let stretched_frames = stretched.len();
        let ratio = original_frames as f64 / stretched_frames as f64;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "original={original_frames} stretched={stretched_frames} ratio={ratio}"
        );

        let peak = stretched.iter().cloned().fold(0.0_f32, |a, b| a.max(b.abs()));
        assert!(peak > 0.1, "peak={peak}, atteso un segnale non silenzioso");
    }

    #[test]
    fn stretch_at_tempo_4x_roughly_quarters_duration() {
        let sample_rate = 48_000;
        let samples = sine(440.0, sample_rate, 2.0);
        let stretched = stretch_samples(&samples, sample_rate, 1, 4.0).unwrap();

        let ratio = samples.len() as f64 / stretched.len() as f64;
        assert!((ratio - 4.0).abs() < 0.1, "ratio={ratio}");
    }
}
