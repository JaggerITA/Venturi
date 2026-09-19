//! Picchi audio per disegnare la waveform in timeline. Cache globale su
//! disco a chiave `content_hash` + stream, come i proxy; generata da
//! `vv-app::waveform_worker`, mai sul thread UI.
//!
//! I bin coprono la durata della *traccia audio*, non del container: la
//! durata è salvata nel file e il disegno usa la stessa base, altrimenti la
//! waveform scivola rispetto al suono quando audio e video hanno durate
//! diverse.

use ffmpeg::media::Type;
use ffmpeg_next as ffmpeg;
use std::ops::ControlFlow;
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

pub fn waveforms_dir() -> PathBuf {
    crate::cache_dir("waveforms")
}

/// Path del file di picchi di uno stream, che esista o no.
pub fn waveform_path_for(content_hash: u64, stream_index: usize) -> PathBuf {
    waveforms_dir().join(format!("{content_hash:016x}_{stream_index}.peaks"))
}

/// Il file esiste *nel formato corrente*? Un file di un `PEAKS_VERSION`
/// precedente verrebbe scartato da `load_waveform`, ma con un semplice
/// `is_file()` il worker non lo rigenererebbe mai. Legge solo l'header.
pub fn waveform_exists(content_hash: u64, stream_index: usize) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(waveform_path_for(content_hash, stream_index)) else {
        return false;
    };
    let mut header = [0u8; 8];
    if f.read_exact(&mut header).is_err() {
        return false;
    }
    &header[..4] == PEAKS_MAGIC && u32::from_le_bytes(header[4..8].try_into().unwrap()) == PEAKS_VERSION
}

/// I picchi di una waveform e la durata della traccia audio che coprono
/// (in secondi): la durata serve al disegno per mappare i bin della clip
/// sulla stessa base temporale dei picchi (vedi doc del modulo).
pub struct Waveform {
    pub peaks: Vec<f32>,
    /// Durata della traccia audio (in secondi) coperta da `peaks`.
    pub audio_duration_secs: f64,
}

/// Picchi degli stream `stream_indices` in una sola lettura del file,
/// `num_peaks` bin ciascuno in [0,1], scritti atomicamente (tmp +
/// `rename`). `None` per uno stream inesistente.
pub fn generate_waveforms(
    source_path: &Path,
    content_hash: u64,
    stream_indices: &[usize],
    num_peaks: usize,
) -> Result<Vec<Option<Waveform>>, crate::MediaError> {
    crate::probe::ensure_init();
    let mut bins: Vec<Option<PeakBins>> = audio_stream_timing(source_path, stream_indices)?
        .into_iter()
        .map(|timing| timing.map(|(secs, rate)| PeakBins::new(secs, rate, num_peaks)))
        .collect();
    crate::audio::decode_audio_streams_streaming(
        source_path,
        stream_indices,
        None,
        |slot, channels, chunk| {
            if let Some(bins) = &mut bins[slot] {
                bins.push(chunk, channels as usize);
            }
            ControlFlow::Continue(())
        },
    )?;

    let dir = waveforms_dir();
    std::fs::create_dir_all(&dir)?;
    let mut out = Vec::with_capacity(bins.len());
    for (&stream_index, bins) in stream_indices.iter().zip(bins) {
        let Some(bins) = bins else {
            out.push(None);
            continue;
        };
        let waveform = bins.finish();
        let tmp_path = dir.join(format!(
            "{content_hash:016x}_{stream_index}.tmp-{}.peaks",
            std::process::id()
        ));
        write_peaks_file(&tmp_path, &waveform.peaks, waveform.audio_duration_secs)?;
        std::fs::rename(&tmp_path, waveform_path_for(content_hash, stream_index))?;
        out.push(Some(waveform));
    }
    Ok(out)
}

#[cfg(test)]
fn generate_waveform(
    source_path: &Path,
    content_hash: u64,
    stream_index: usize,
    num_peaks: usize,
) -> Result<Option<Waveform>, crate::MediaError> {
    Ok(generate_waveforms(source_path, content_hash, &[stream_index], num_peaks)?
        .pop()
        .flatten())
}

/// `(durata in secondi, sample rate)` di ciascuno stream audio richiesto.
/// La durata è quella della traccia (nel suo time_base); se il formato non
/// la segnala, quella del container.
fn audio_stream_timing(
    path: &Path,
    stream_indices: &[usize],
) -> Result<Vec<Option<(f64, u32)>>, crate::MediaError> {
    let ictx = ffmpeg::format::input(path)?;
    let container_secs = ictx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
    let streams: Vec<_> = ictx
        .streams()
        .filter(|s| s.parameters().medium() == Type::Audio)
        .collect();
    stream_indices
        .iter()
        .map(|&i| {
            let Some(stream) = streams.get(i) else {
                return Ok(None);
            };
            let rate = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
                .decoder()
                .audio()?
                .rate();
            let tb = stream.time_base();
            let secs = stream.duration() as f64 * tb.0 as f64 / tb.1 as f64;
            Ok(Some((if secs > 0.0 { secs } else { container_secs }, rate)))
        })
        .collect()
}

/// Picchi in costruzione per uno stream.
struct PeakBins {
    peaks: Vec<f32>,
    /// Frazionario: troncarlo accumulerebbe ritardo lungo il file.
    samples_per_bin: f64,
    /// Frame audio (un campione per canale) visti finora.
    frames_seen: usize,
    audio_duration_secs: f64,
}

impl PeakBins {
    fn new(audio_duration_secs: f64, sample_rate: u32, num_peaks: usize) -> Self {
        let num_peaks = num_peaks.max(1);
        Self {
            peaks: vec![0.0; num_peaks],
            samples_per_bin: (audio_duration_secs * sample_rate as f64 / num_peaks as f64)
                .max(1.0),
            frames_seen: 0,
            audio_duration_secs,
        }
    }

    /// `chunk` interleaved: il bin avanza di un frame per volta, non di un
    /// campione, o l'audio multicanale finirebbe compresso nei primi bin.
    fn push(&mut self, chunk: &[f32], channels: usize) {
        let last = self.peaks.len() - 1;
        for frame in chunk.chunks_exact(channels.max(1)) {
            let bin = ((self.frames_seen as f64 / self.samples_per_bin) as usize).min(last);
            let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            self.peaks[bin] = self.peaks[bin].max(peak);
            self.frames_seen += 1;
        }
    }

    /// Normalizza sul picco globale: un file quieto usa comunque tutta
    /// l'altezza della clip.
    fn finish(mut self) -> Waveform {
        let global_peak = self.peaks.iter().copied().fold(0.0f32, f32::max);
        if global_peak > 0.0 {
            for p in &mut self.peaks {
                *p /= global_peak;
            }
        }
        Waveform {
            peaks: self.peaks,
            audio_duration_secs: self.audio_duration_secs,
        }
    }
}
/// Carica i picchi già generati per `content_hash`/`stream_index` dal file
/// di cache, se esiste. `Ok(None)` se il file non c'è (da generare) o è
/// corrotto.
pub fn load_waveform(content_hash: u64, stream_index: usize) -> Option<Waveform> {
    let path = waveform_path_for(content_hash, stream_index);
    let bytes = std::fs::read(&path).ok()?;
    read_peaks_file(&bytes)
}

/// Magic, versione (u32), numero di picchi (u32), durata audio (f64),
/// picchi (f32), tutto LE.
const PEAKS_MAGIC: &[u8; 4] = b"vbwf";
// v3: durata audio dal time_base dello stream.
// v4: bin di campioni frazionari (troncarli faceva scivolare la waveform).
const PEAKS_VERSION: u32 = 4;

fn write_peaks_file(path: &Path, peaks: &[f32], audio_duration_secs: f64) -> Result<(), crate::MediaError> {
    let mut bytes = Vec::with_capacity(20 + peaks.len() * 4);
    bytes.extend_from_slice(PEAKS_MAGIC);
    bytes.extend_from_slice(&PEAKS_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(peaks.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&audio_duration_secs.to_le_bytes());
    for p in peaks {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    Ok(std::fs::write(path, bytes)?)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Genera un vero file x264 con audio AAC (un seno) via ffmpeg CLI —
    /// il caso d'uso primario (compressed x264 + audio), non un mock.
    fn make_test_clip(file_name: &str, duration_secs: u32) -> PathBuf {
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        crate::test_support::ffmpeg(
            &[
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
            ],
            &path,
        );
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
        let _ = std::fs::remove_file(waveform_path_for(content_hash, 0));

        assert!(!waveform_exists(content_hash, 0));
        let wf = generate_waveform(&path, content_hash, 0, 500)
            .expect("generazione waveform fallita")
            .expect("audio atteso");
        assert_eq!(wf.peaks.len(), 500);
        assert!(waveform_exists(content_hash, 0));

        // Un seno a 440Hz non è silenzioso: il picco globale (normalizzato)
        // deve toccare 1.0 e i valori devono stare in [0,1].
        let peak = wf.peaks.iter().cloned().fold(0.0_f32, f32::max);
        assert!((peak - 1.0).abs() < 1e-6, "peak={peak}, atteso 1.0 dopo normalizzazione");
        assert!(wf.peaks.iter().all(|p| (0.0..=1.0).contains(p)));

        // La durata audio deve essere ~2s (il file è di 2s).
        assert!((wf.audio_duration_secs - 2.0).abs() < 0.2, "dur={:?}", wf.audio_duration_secs);

        // Il file di cache, riletto, restituisce gli stessi picchi.
        let reloaded = load_waveform(content_hash, 0).expect("reload fallito");
        assert_eq!(reloaded.peaks, wf.peaks);
        assert!((reloaded.audio_duration_secs - wf.audio_duration_secs).abs() < 1e-9);
    }

    #[test]
    fn stereo_audio_peaks_are_not_compressed_into_the_first_half_of_bins() {
        // 4s stereo: silenzio nei primi 2s, tono nei secondi 2s. Col bug
        // (global_sample avanzava una volta per *campione interleaved*
        // invece che una volta per *frame*, quindi 2 volte più in fretta
        // per lo stereo) l'intera waveform finiva compressa/clampata nella
        // prima metà dei bin, e il tono (che inizia a metà della durata
        // reale) appariva già ai 3/4 della larghezza invece che all'ultimo
        // quarto — "la waveform è in anticipo rispetto al suono".
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stereo_half_silent.mp4");
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=exprs='if(lt(t,2),0,sin(2*PI*440*t))':s=48000:d=4:c=stereo",
                "-c:a",
                "aac",
            ],
            &path,
        );

        let content_hash = 0x51DE7357;
        let _ = std::fs::remove_file(waveform_path_for(content_hash, 0));
        let wf = generate_waveform(&path, content_hash, 0, 400)
            .expect("generazione waveform fallita")
            .expect("audio atteso");

        // Il tono inizia esattamente a metà della durata (2s su 4s): con
        // 400 bin, il bin dov'esce dal silenzio deve stare vicino al bin
        // 200 (metà larghezza). Col bug il clamp a `num_peaks - 1` nascondeva
        // un controllo troppo debole su "solo prima/ultima porzione" (il
        // tono, raddoppiato di velocità, restava comunque schiacciato
        // nell'ultimissimo bin anziché sparire) — qui verifichiamo la
        // *posizione* della transizione, non solo che il tono compaia da
        // qualche parte.
        let first_loud_bin = wf
            .peaks
            .iter()
            .position(|&p| p > 0.3)
            .expect("il tono dovrebbe superare la soglia da qualche parte");
        assert!(
            (170..230).contains(&first_loud_bin),
            "il tono inizia al bin {first_loud_bin} (atteso vicino al bin 200, metà dei 400 bin totali per una transizione a metà dei 4s)"
        );
    }

    /// Stessa transizione silenzio->tono del test stereo, ma su 6 canali
    /// (5.1 side, l'esatto layout del file reale che ha esposto il bug
    /// dei canali stereo — bbb_sunflower con traccia AC-3 5.1): verifica
    /// che il fix `chunks_exact(channels)` regga anche channels > 2, non
    /// solo il caso a 2 canali già coperto.
    /// Con un numero di campioni per bin non intero (il caso normale: i
    /// bin vengono dalla durata del video) la posizione dei picchi non
    /// deve scivolare lungo il file.
    #[test]
    fn peaks_stay_aligned_when_samples_per_bin_is_fractional() {
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tono_a_8s.wav");
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=exprs='if(lt(t,8),0,sin(2*PI*440*t))':s=48000:d=10",
            ],
            &path,
        );

        let content_hash = 0xF4AC7;
        let _ = std::fs::remove_file(waveform_path_for(content_hash, 0));
        // 480000 campioni / 250000 bin = 1,92 campioni per bin.
        let num_peaks = 250_000;
        let wf = generate_waveform(&path, content_hash, 0, num_peaks)
            .expect("generazione waveform fallita")
            .expect("audio atteso");
        let first_loud_bin = wf.peaks.iter().position(|&p| p > 0.3).expect("tono atteso");
        let expected = num_peaks * 8 / 10;
        assert!(
            // Il seno parte da 0: supera la soglia qualche campione dopo.
            first_loud_bin.abs_diff(expected) <= 5,
            "tono al bin {first_loud_bin}, atteso {expected} (8 s su 10)"
        );
        let _ = std::fs::remove_file(waveform_path_for(content_hash, 0));
    }

    #[test]
    fn six_channel_audio_peaks_are_not_compressed_into_the_first_half_of_bins() {
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("six_channel_half_silent.mp4");
        let tone = "if(lt(t,2),0,sin(2*PI*440*t))";
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("aevalsrc=exprs='{tone}|{tone}|{tone}|{tone}|{tone}|{tone}':s=48000:d=4:c=5.1"),
                "-c:a",
                "ac3",
            ],
            &path,
        );

        let content_hash = 0x51DE7357_6C6C6C6C;
        let _ = std::fs::remove_file(waveform_path_for(content_hash, 0));
        let wf = generate_waveform(&path, content_hash, 0, 400)
            .expect("generazione waveform fallita")
            .expect("audio atteso");

        let first_loud_bin = wf
            .peaks
            .iter()
            .position(|&p| p > 0.3)
            .expect("il tono dovrebbe superare la soglia da qualche parte");
        assert!(
            (170..230).contains(&first_loud_bin),
            "il tono inizia al bin {first_loud_bin} (atteso vicino al bin 200, metà dei 400 bin totali per una transizione a metà dei 4s)"
        );
    }

    #[test]
    fn generate_waveform_returns_none_for_a_video_without_audio() {
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("silent.mp4");
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            &path,
        );

        let result = generate_waveform(&path, 0xDEADBEEF, 0, 200).expect("generazione fallita");
        assert!(result.is_none(), "nessuna traccia audio: nessun picco");
    }

    /// Un file con due stream audio (vedi `audio::decode_audio_track_selects_the_requested_stream_index_not_just_the_best`
    /// per lo stesso fixture): la waveform generata per lo stream 1 deve
    /// essere quella del *secondo* segnale, non ricadere sempre sul primo.
    #[test]
    fn generate_waveform_selects_the_requested_stream_index() {
        let dir = std::env::temp_dir().join("vv-media-waveform-test");
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

        let content_hash = 0x57EA2001;
        let _ = std::fs::remove_file(waveform_path_for(content_hash, 0));
        let _ = std::fs::remove_file(waveform_path_for(content_hash, 1));

        let first = generate_waveform(&path, content_hash, 0, 200)
            .unwrap()
            .expect("stream 0 atteso");
        assert!((first.audio_duration_secs - 1.0).abs() < 0.2);

        let second = generate_waveform(&path, content_hash, 1, 200)
            .unwrap()
            .expect("stream 1 atteso");
        assert!((second.audio_duration_secs - 1.0).abs() < 0.2);

        assert!(
            generate_waveform(&path, content_hash, 2, 200).unwrap().is_none(),
            "nessuno stream audio all'indice 2"
        );
    }
}