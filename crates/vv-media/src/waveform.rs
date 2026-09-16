//! Estrazione dei picchi (peak) della traccia audio per il disegno della
//! waveform sulla track audio della timeline (milestone 8).
//!
//! Stesso modello di `proxy.rs`: cache globale su disco chiave
//! `content_hash` del `MediaItem` (indipendente dal progetto — una
//! waveform generata in una sessione resta riusabile da un altro progetto
//! che referenzia lo stesso file), generata in background da un worker
//! dedicato (vedi `vv-app::waveform_worker`) e mai sul thread UI.
//!
//! I picchi sono un vettore di `f32` in [0,1] (massimo assoluto per
//! "bin" di campioni), uno per bin di durata fissa: disegnare la waveform
//! è poi solo un max-per-colonna-pixel su questo vettore, indipendente
//! dallo zoom. La decodifica è *streaming* (un frame alla volta, mai il
//! buffer intero in RAM) così anche un file da un'ora non gonfia la
//! memoria — i picchi finali pesano ~4 byte × numero di bin, non i
//! campioni.
//!
//! **Allineamento al suono.** I bin sono dimensionati sulla durata della
//! *traccia audio* (non del container/video): la traccia audio può essere
//! più corta o più lunga del video (es. il video finisce prima dell'audio,
//! o viceversa), e dimensionare i bin sulla durata del container
//! stirerebbe/comprimerebbe la waveform rispetto al suono reale — ogni
//! evento audio cadrebbe in un bin sbagliato, spostando la forma d'onda
//! rispetto al playback (bug segnalato: "la waveform è anticipata di mezzo
//! secondo"). La durata audio viene salvata nel file di cache e usata
//! anche dal disegno (vedi `draw_clip_waveform` in `timeline_ui`), così
//! generazione e disegno usano la stessa base temporale.

use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type;
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg_next as ffmpeg;
use std::path::{Path, PathBuf};

/// Numero di picchi per secondo di audio: abbastanza da rendere la
/// waveform leggibile anche a zoom medio/alto, con un file di cache
/// contenuto (~4 byte/peak → 100 peak/s ≈ 400 B/s ≈ 1,4 MB per un'ora).
const PEAKS_PER_SECOND: f64 = 100.0;
/// Piani di picchi minimi/massimi: un clip brevissimo non merita più di
/// un centinaio di bin (sarebbero tutti uguali), un file lunghissimo non
/// deve gonfiare la cache oltre un limite ragionevole.
const MIN_PEAKS: usize = 100;
const MAX_PEAKS: usize = 200_000;

/// Numero di picchi consigliato per un media di `duration_secs` secondi:
/// proporzionale alla durata, clampato a `[MIN_PEAKS, MAX_PEAKS]`.
pub fn recommended_num_peaks(duration_secs: f64) -> usize {
    let raw = (duration_secs * PEAKS_PER_SECOND).round() as usize;
    raw.clamp(MIN_PEAKS, MAX_PEAKS)
}

/// Cartella cache globale delle waveform: `$XDG_CACHE_HOME/vibevideo/
/// waveforms/`, o `~/.cache/vibevideo/waveforms/` se `XDG_CACHE_HOME` non è
/// impostata (stesso fallback di `proxy::proxies_dir`).
pub fn waveforms_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("vibevideo").join("waveforms")
}

/// Path del file di picchi per questo `content_hash`, che esista o no
/// ancora — vedi `waveform_exists`.
pub fn waveform_path_for(content_hash: u64) -> PathBuf {
    waveforms_dir().join(format!("{content_hash:016x}.peaks"))
}

/// Il file di picchi per questo `content_hash` è già stato generato?
/// Un `stat` economico, fatto a ogni disegno invece di tenere stato in
/// memoria: il file può comparire in qualunque momento dal worker
/// separato, e un semplice `is_file()` non ha bisogno di sincronizzazione
/// (lo stesso pattern di `proxy::proxy_exists`).
pub fn waveform_exists(content_hash: u64) -> bool {
    waveform_path_for(content_hash).is_file()
}

/// I picchi di una waveform e la durata della traccia audio che coprono
/// (in secondi): la durata serve al disegno per mappare i bin della clip
/// sulla stessa base temporale dei picchi (vedi doc del modulo).
pub struct Waveform {
    pub peaks: Vec<f32>,
    /// Durata della traccia audio (in secondi) coperta da `peaks`.
    pub audio_duration_secs: f64,
}

/// Estrae i picchi della traccia audio di `source_path` in `num_peaks`
/// bin (massimo assoluto per bin, in [0,1]) e li scrive **atomicamente** a
/// `waveform_path_for(content_hash)`: prima un file temporaneo nella stessa
/// cartella, poi `rename` (atomico sullo stesso filesystem) — mai un file
/// a metà scritto visibile a un lettore concorrente (la timeline, su un
/// altro thread, potrebbe controllarlo in qualunque istante mentre questa
/// funzione è ancora in corso).
///
/// `Ok(None)` se il media non ha traccia audio (niente da disegnare).
pub fn generate_waveform(
    source_path: &Path,
    content_hash: u64,
    num_peaks: usize,
) -> Result<Option<Waveform>, crate::MediaError> {
    crate::probe::ensure_init();

    let mut ictx = ffmpeg::format::input(source_path)?;
    let Some(audio_stream) = ictx.streams().best(Type::Audio) else {
        return Ok(None);
    };
    let audio_stream_index = audio_stream.index();

    let mut decoder = ffmpeg::codec::context::Context::from_parameters(audio_stream.parameters())?
        .decoder()
        .audio()?;

    let channel_layout = decoder.channel_layout();
    let sample_rate = decoder.rate();

    let mut resampler = Resampler::get(
        decoder.format(),
        channel_layout,
        sample_rate,
        Sample::F32(SampleType::Packed),
        channel_layout,
        sample_rate,
    )?;

    // Durata della *traccia audio* (non del container): la base temporale
    // sui cui dimensionare i bin. `audio_stream.duration()` è espresso nel
    // time_base della *stream* (per l'audio ≈ 1/sample_rate), NON in
    // AV_TIME_BASE: va riscalata con `duration * tb.num / tb.den` per
    // ottenere i secondi. Se è 0/assente (alcuni formati non la segnalano,
    // o la danno come sconosciuta, -1) ricade sulla durata del container.
    let audio_duration_secs = {
        let tb = audio_stream.time_base();
        let d = audio_stream.duration() as f64 * tb.0 as f64 / tb.1 as f64;
        if d > 0.0 {
            d
        } else {
            // Alcuni formati non segnalano la durata della traccia audio:
            // ricade sulla durata del container (stima ragionevole).
            ictx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE)
        }
    };

    let total_samples = (audio_duration_secs * sample_rate as f64) as usize;
    let samples_per_bin = (total_samples / num_peaks).max(1);

    let mut peaks = vec![0.0_f32; num_peaks];
    let mut global_sample = 0usize;

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
                    push_resampled_peaks(
                        &mut resampler,
                        &decoded,
                        &mut peaks,
                        &mut global_sample,
                        samples_per_bin,
                        num_peaks,
                    )?;
                }
            }
            Err(ffmpeg::Error::Eof) => {
                decoder.send_eof()?;
                while decoder.receive_frame(&mut decoded).is_ok() {
                    push_resampled_peaks(
                        &mut resampler,
                        &decoded,
                        &mut peaks,
                        &mut global_sample,
                        samples_per_bin,
                        num_peaks,
                    )?;
                }
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Normalizza sul picco globale così la waveform usa l'intera altezza
    // della clip (come nei player): un file quieto non si disegna
    // invisibilmente piccolo. Se il picco è 0 (silenzio totale) resta 0.
    let global_peak = peaks.iter().cloned().fold(0.0_f32, f32::max);
    if global_peak > 0.0 {
        for p in &mut peaks {
            *p /= global_peak;
        }
    }

    let dir = waveforms_dir();
    std::fs::create_dir_all(&dir).map_err(io_err)?;
    let final_path = waveform_path_for(content_hash);
    let tmp_path = dir.join(format!(
        "{content_hash:016x}.tmp-{}.peaks",
        std::process::id()
    ));
    write_peaks_file(&tmp_path, &peaks, audio_duration_secs)?;
    std::fs::rename(&tmp_path, &final_path).map_err(io_err)?;

    Ok(Some(Waveform {
        peaks,
        audio_duration_secs,
    }))
}

/// Carica i picchi già generati per `content_hash` dal file di cache, se
/// esiste. `Ok(None)` se il file non c'è (da generare) o è corrotto.
pub fn load_waveform(content_hash: u64) -> Option<Waveform> {
    let path = waveform_path_for(content_hash);
    let bytes = std::fs::read(&path).ok()?;
    read_peaks_file(&bytes)
}

/// Formato del file di picchi: magic (4 byte) + versione (u32 LE) +
/// numero di picchi (u32 LE) + durata audio (f64 LE) + i picchi (f32 LE
/// ciascuno). La versione permette di scartare i file generati da una
/// versione precedente del formato senza doverli rigenerare a mano.
const PEAKS_MAGIC: &[u8; 4] = b"vbwf";
// v3: la durata audio è riscalata dal time_base della stream (v2 la
// divideva per AV_TIME_BASE, sbagliando di ~sample_rate volte e
// comprimendo i picchi nei primi bin — waveform desincronizzata).
const PEAKS_VERSION: u32 = 3;

fn write_peaks_file(path: &Path, peaks: &[f32], audio_duration_secs: f64) -> Result<(), crate::MediaError> {
    let mut bytes = Vec::with_capacity(20 + peaks.len() * 4);
    bytes.extend_from_slice(PEAKS_MAGIC);
    bytes.extend_from_slice(&PEAKS_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(peaks.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&audio_duration_secs.to_le_bytes());
    for p in peaks {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    std::fs::write(path, bytes).map_err(io_err)
}

fn read_peaks_file(bytes: &[u8]) -> Option<Waveform> {
    if bytes.len() < 20 || &bytes[..4] != PEAKS_MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != PEAKS_VERSION {
        return None;
    }
    let count = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    let audio_duration_secs = f64::from_le_bytes(bytes[12..20].try_into().ok()?);
    if bytes.len() < 20 + count * 4 {
        return None;
    }
    let mut peaks = Vec::with_capacity(count);
    for chunk in bytes[20..20 + count * 4].as_chunks::<4>().0.iter() {
        peaks.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(Waveform {
        peaks,
        audio_duration_secs,
    })
}

fn io_err(e: std::io::Error) -> crate::MediaError {
    crate::MediaError::NoStream(e.to_string())
}

/// Risample un frame audio in f32 packed e aggiorna i picchi: per ogni
/// campione, il bin `global_sample / samples_per_bin` prende il massimo
/// assoluto visto finora. `global_sample` avanza del numero di campioni
/// nel frame — così il bin di ogni campione è corretto anche se i frame
/// hanno durate diverse (tipico di un decoder con priming/padding).
fn push_resampled_peaks(
    resampler: &mut Resampler,
    decoded: &ffmpeg::frame::Audio,
    peaks: &mut [f32],
    global_sample: &mut usize,
    samples_per_bin: usize,
    num_peaks: usize,
) -> Result<(), crate::MediaError> {
    let mut resampled = ffmpeg::frame::Audio::empty();
    resampler.run(decoded, &mut resampled)?;
    let byte_len = resampled.samples() * resampled.channels() as usize * 4;
    let bytes = &resampled.data(0)[..byte_len];
    // Stesso pattern di `audio::push_resampled`: `as_chunks::<4>` (stabilizzata)
    // dà i 4 byte di ogni campione f32 senza copie.
    for chunk in bytes.as_chunks::<4>().0.iter() {
        let value = f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let bin = (*global_sample / samples_per_bin).min(num_peaks - 1);
        let abs = value.abs();
        if abs > peaks[bin] {
            peaks[bin] = abs;
        }
        *global_sample += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Genera un vero file x264 con audio AAC (un seno) via ffmpeg CLI —
    /// il caso d'uso primario (compressed x264 + audio), non un mock.
    fn make_test_clip(file_name: &str, duration_secs: u32) -> PathBuf {
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:sample_rate=48000:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());
        path
    }

    #[test]
    fn recommended_num_peaks_scales_with_duration_and_is_clamped() {
        assert_eq!(recommended_num_peaks(0.5), MIN_PEAKS);
        assert_eq!(recommended_num_peaks(60.0), 6000);
        assert_eq!(recommended_num_peaks(3600.0), MAX_PEAKS);
    }

    #[test]
    fn peaks_file_round_trips() {
        let peaks: Vec<f32> = (0..100).map(|i| (i as f32) / 99.0).collect();
        let bytes = {
            let mut b = Vec::new();
            b.extend_from_slice(PEAKS_MAGIC);
            b.extend_from_slice(&PEAKS_VERSION.to_le_bytes());
            b.extend_from_slice(&(peaks.len() as u32).to_le_bytes());
            b.extend_from_slice(&5.5f64.to_le_bytes());
            for p in &peaks {
                b.extend_from_slice(&p.to_le_bytes());
            }
            b
        };
        let loaded = read_peaks_file(&bytes).expect("read fallito");
        assert_eq!(loaded.peaks, peaks);
        assert!((loaded.audio_duration_secs - 5.5).abs() < 1e-9);
    }

    #[test]
    fn peaks_file_rejects_bad_magic_and_version() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"XXXX");
        bytes.extend_from_slice(&PEAKS_VERSION.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1.0f64.to_le_bytes());
        bytes.extend_from_slice(&0.5f32.to_le_bytes());
        assert!(read_peaks_file(&bytes).is_none());

        let mut bytes = Vec::new();
        bytes.extend_from_slice(PEAKS_MAGIC);
        bytes.extend_from_slice(&99u32.to_le_bytes()); // versione sconosciuta
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1.0f64.to_le_bytes());
        bytes.extend_from_slice(&0.5f32.to_le_bytes());
        assert!(read_peaks_file(&bytes).is_none());
    }

    #[test]
    fn generate_waveform_produces_normalized_peaks_and_caches_to_disk() {
        let path = make_test_clip("source.mp4", 2);
        let content_hash = 0xCAFEBABE;
        let _ = std::fs::remove_file(waveform_path_for(content_hash));

        assert!(!waveform_exists(content_hash));
        let wf = generate_waveform(&path, content_hash, 500)
            .expect("generazione waveform fallita")
            .expect("audio atteso");
        assert_eq!(wf.peaks.len(), 500);
        assert!(waveform_exists(content_hash));

        // Un seno a 440Hz non è silenzioso: il picco globale (normalizzato)
        // deve toccare 1.0 e i valori devono stare in [0,1].
        let peak = wf.peaks.iter().cloned().fold(0.0_f32, f32::max);
        assert!((peak - 1.0).abs() < 1e-6, "peak={peak}, atteso 1.0 dopo normalizzazione");
        assert!(wf.peaks.iter().all(|p| (0.0..=1.0).contains(p)));

        // La durata audio deve essere ~2s (il file è di 2s).
        assert!((wf.audio_duration_secs - 2.0).abs() < 0.2, "dur={:?}", wf.audio_duration_secs);

        // Il file di cache, riletto, restituisce gli stessi picchi.
        let reloaded = load_waveform(content_hash).expect("reload fallito");
        assert_eq!(reloaded.peaks, wf.peaks);
        assert!((reloaded.audio_duration_secs - wf.audio_duration_secs).abs() < 1e-9);
    }

    #[test]
    fn generate_waveform_returns_none_for_a_video_without_audio() {
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("silent.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let result = generate_waveform(&path, 0xDEADBEEF, 200).expect("generazione fallita");
        assert!(result.is_none(), "nessuna traccia audio: nessun picco");
    }
}