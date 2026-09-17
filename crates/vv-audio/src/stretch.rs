//! Time-stretch con pitch preservato per lo speed change, via il filtro
//! `rubberband` di libavfilter (ffmpeg-next::filter) — già linkato nel
//! ffmpeg di sistema (`--enable-librubberband`), nessun binding extra.
//!
//! Elabora un buffer per volta in un colpo solo (non è un filtro
//! streaming a bassa latenza per campione, ma nemmeno pensato per
//! l'intera traccia): il chiamante (`vv-app::timeline_audio`, vedi
//! `WINDOW_FRAMES`) gli passa una *finestra* di qualche secondo di
//! mix alla volta invece dell'intera traccia — su una
//! clip lunga, stretchare tutti i minuti in un colpo solo prima di poter
//! sentire qualunque cosa introduce un ritardo percepibile (anche
//! diversi secondi) alla pressione del tasto velocità; una finestra
//! piccola torna in ~100-200ms (misurato, vedi test `bench_window`) e la
//! finestra successiva viene calcolata in background mentre la corrente
//! sta ancora suonando.

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

    // `rubberband` negozia liberamente il formato interno e può restituire
    // planar anche se l'input è packed: `aformat` dopo di lui forza di
    // nuovo packed, altrimenti `drain_filtered` (che assume sempre un
    // singolo piano interleaved) legge oltre la fine del primo piano non
    // appena i canali sono più di uno (panic riprodotto con audio stereo).
    graph
        .output("in", 0)
        .and_then(|p| p.input("out", 0))
        .and_then(|p| {
            p.parse(&format!(
                "rubberband=tempo={tempo},aformat=sample_fmts={}",
                format.name()
            ))
        })
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

    /// Regressione: il filtro `rubberband` può restituire l'audio in
    /// formato planar anche con input packed (vedi `aformat` dopo
    /// `rubberband` in `stretch_samples`) — con un solo canale (mono) i
    /// due formati coincidono in memoria, quindi solo un test con più
    /// canali esercita davvero questo percorso.
    #[test]
    fn stretch_stereo_roughly_halves_duration_and_stays_interleaved() {
        let sample_rate = 44_100;
        let channels = 2u16;
        let samples = sine(440.0, sample_rate, 5.0)
            .into_iter()
            .flat_map(|s| [s, s])
            .collect::<Vec<_>>();

        let stretched = stretch_samples(&samples, sample_rate, channels, 2.0).unwrap();

        assert_eq!(
            stretched.len() % channels as usize,
            0,
            "il buffer interleaved deve restare un multiplo esatto di channels"
        );
        let ratio = samples.len() as f64 / stretched.len() as f64;
        assert!((ratio - 2.0).abs() < 0.05, "ratio={ratio}");

        let peak = stretched.iter().cloned().fold(0.0_f32, |a, b| a.max(b.abs()));
        assert!(peak > 0.1, "peak={peak}, atteso un segnale non silenzioso");
    }
}

#[cfg(test)]
mod bench_window {
    use super::*;
    use std::time::{Duration, Instant};

    /// Non una garanzia hard-realtime, ma una soglia larga: stretchare la
    /// finestra scelta da `vv-app` (`WINDOW_FRAMES` in `timeline_audio`, 8s) deve restare
    /// ben sotto il margine reale disponibile prima che l'estensione in
    /// background (triggerata quando restano `EXTEND_TRIGGER_MARGIN_SECS`,
    /// 4s, di finestra non ancora suonata) serva davvero — quel margine
    /// in tempo *reale* si restringe con la velocità (4s di audio
    /// originale sono 1s reale a 4x, 0.5s a 8x), quindi la soglia qui
    /// scala di conseguenza (metà del margine reale, headroom 2x).
    /// Misurato in pratica ~150ms indipendentemente dal tempo, vedi
    /// commento nel modulo.
    #[test]
    fn stretching_an_8s_window_is_well_under_the_extension_margin() {
        let sample_rate = 48_000u32;
        let channels = 2u16;
        let secs = 8.0;
        let n = (sample_rate as f64 * secs) as usize;
        let samples: Vec<f32> = (0..n * channels as usize)
            .map(|i| (i as f32 * 0.01).sin())
            .collect();
        const EXTEND_TRIGGER_MARGIN_SECS: f64 = 4.0;
        for tempo in [2.0, 4.0, 8.0] {
            let real_margin_secs = EXTEND_TRIGGER_MARGIN_SECS / tempo;
            let threshold = Duration::from_secs_f64(real_margin_secs / 2.0);
            let start = Instant::now();
            stretch_samples(&samples, sample_rate, channels, tempo).unwrap();
            let elapsed = start.elapsed();
            assert!(
                elapsed < threshold,
                "tempo={tempo} elapsed={elapsed:?} threshold={threshold:?}, troppo lento per l'estensione in background a questa velocità"
            );
        }
    }
}
