//! Decode-ahead in background: un thread dedicato decodifica in sequenza a
//! partire da un target (aggiornato su seek) e riempie la `FrameCache`.
//! Vedi ARCHITECTURE.md § Pipeline di decode + cache.
//!
//! Un solo worker per ora (un player = una clip aperta alla volta): il pool
//! di worker per il decode-ahead multi-clip è un'estensione futura, non
//! necessaria finché il player lavora su una sola clip (milestone 2).

use crate::cache::FrameCache;
use crate::decode::Decoder;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;
use vv_core::FrameIdx;

enum Command {
    SeekTo(FrameIdx, f64),
    Stop,
}

/// `Decoder` contiene un puntatore raw FFI (`SwsContext`) che non è `Send`
/// per default. È comunque sicuro spostarlo nel thread di decode-ahead: da
/// quel momento è posseduto ed usato esclusivamente da quel thread, mai
/// condiviso o acceduto concorrentemente dal chiamante.
struct SendDecoder(Decoder);
unsafe impl Send for SendDecoder {}

/// `true` se il comando era `Stop` (il chiamante deve interrompere il loop).
fn apply_command(cmd: Command, decoder: &mut Decoder, next_to_decode: &mut FrameIdx) -> bool {
    match cmd {
        Command::SeekTo(frame_idx, secs) => {
            if let Err(e) = decoder.seek_to_time(secs) {
                eprintln!("vv-media: seek fallito: {e}");
            }
            *next_to_decode = frame_idx;
            false
        }
        Command::Stop => true,
    }
}

pub struct DecodeAhead {
    cache: Arc<FrameCache>,
    target: Arc<AtomicI64>,
    tx: mpsc::Sender<Command>,
    handle: Option<JoinHandle<()>>,
    pub width: u32,
    pub height: u32,
    pub fps: vv_core::Rational,
}

/// Frame minimi tenuti in cache indipendentemente dal budget di memoria:
/// evita che una risoluzione fuori scala (es. 8K) riduca la cache a
/// pochissimi frame, sotto la soglia utile per un margine di sicurezza
/// contro rallentamenti di decodifica transitori.
const MIN_CACHE_FRAMES: usize = 24;

/// Quanti frame RGBA8 (`width*height*4` byte l'uno, non compressi) entrano
/// in `budget_bytes`, con un minimo di `MIN_CACHE_FRAMES`. Funzione pura
/// (nessuna apertura di file/thread) per poterla testare senza dipendere
/// da ffmpeg.
pub fn frame_cache_capacity(budget_bytes: usize, width: u32, height: u32) -> usize {
    let bytes_per_frame = (width as usize * height as usize * 4).max(1);
    (budget_bytes / bytes_per_frame).max(MIN_CACHE_FRAMES)
}

impl DecodeAhead {
    /// Apre il decoder subito nel thread chiamante (per propagare un
    /// eventuale errore di apertura in modo sincrono), poi sposta il
    /// decode continuo su un thread dedicato.
    ///
    /// La capacità della cache (in frame) è derivata da `cache_budget_bytes`
    /// (un budget di *memoria*, non un conteggio di frame) e dalla
    /// risoluzione reale del media, scoperta solo qui dopo l'apertura:
    /// frame RGBA8 non compressi pesano `width*height*4` byte l'uno, quindi
    /// un conteggio fisso di frame ha un costo in RAM molto diverso a
    /// seconda della sorgente (un 1080p e un 4K differiscono di ~4x). Un
    /// budget fisso invece garantisce un tetto di memoria prevedibile per
    /// player aperto qualunque sia la risoluzione, *e* dà automaticamente
    /// più margine di riproduzione fluida (più frame pre-decodificati)
    /// alle sorgenti più leggere invece di limitarle tutte allo stesso
    /// conteggio pensato per il caso peggiore (bug segnalato: cache
    /// tarata bassa per stare sotto OOM su 4K aveva introdotto stutter
    /// anche su sorgenti comuni 1080p/720p, che invece potevano permettersi
    /// molto più margine).
    pub fn spawn(
        path: PathBuf,
        cache_budget_bytes: usize,
        ahead_frames: i64,
    ) -> Result<Self, crate::MediaError> {
        let decoder = Decoder::open(&path)?;
        let width = decoder.width();
        let height = decoder.height();
        let fps = decoder.fps();
        let decoder = SendDecoder(decoder);

        let cache_capacity = frame_cache_capacity(cache_budget_bytes, width, height);
        // Il decode-ahead non deve mai puntare a correre più avanti di
        // quanto la cache possa effettivamente trattenere, altrimenti
        // decodificherebbe frame che vengono sfrattati (LRU) prima di
        // essere mai consumati — lavoro sprecato. Margine di un terzo
        // sotto la capacità per lasciare spazio anche a un po' di
        // rilettura all'indietro.
        let ahead_frames = ahead_frames.min((cache_capacity as i64 * 2) / 3).max(1);

        let cache = Arc::new(FrameCache::new(cache_capacity));
        let target = Arc::new(AtomicI64::new(0));
        let (tx, rx) = mpsc::channel::<Command>();

        let thread_cache = cache.clone();
        let thread_target = target.clone();
        let handle = std::thread::spawn(move || {
            // La cattura "precisa" delle closure (RFC 2229) catturerebbe solo
            // il campo `.0` bypassando il wrapper `Send`: questo passaggio
            // intermedio forza la cattura dell'intero `SendDecoder`.
            let mut decoder = decoder;
            let decoder = &mut decoder.0;
            let mut next_to_decode: FrameIdx = 0;
            loop {
                let target_frame = thread_target.load(Ordering::Relaxed);
                let need_wait = next_to_decode > target_frame + ahead_frames;

                if need_wait {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(cmd) => {
                            if apply_command(cmd, decoder, &mut next_to_decode) {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    continue;
                }

                let mut stop = false;
                while let Ok(cmd) = rx.try_recv() {
                    if apply_command(cmd, decoder, &mut next_to_decode) {
                        stop = true;
                        break;
                    }
                }
                if stop {
                    break;
                }

                if thread_cache.contains(next_to_decode) {
                    next_to_decode += 1;
                    continue;
                }

                match decoder.next_frame() {
                    Ok(Some((idx, frame))) => {
                        thread_cache.insert(idx, Arc::new(frame));
                        next_to_decode = idx + 1;
                    }
                    Ok(None) => {
                        // Fine stream: forza `need_wait` finché non arriva un seek.
                        next_to_decode = FrameIdx::MAX;
                    }
                    Err(e) => {
                        eprintln!("vv-media: decode fallito: {e}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            cache,
            target,
            tx,
            handle: Some(handle),
            width,
            height,
            fps,
        })
    }

    pub fn cache(&self) -> &Arc<FrameCache> {
        &self.cache
    }

    /// Sposta subito il target di decode-ahead e chiede al thread di
    /// worker di fare un seek nel media (usato quando l'utente trascina lo
    /// slider di scrub).
    pub fn seek(&self, frame_idx: FrameIdx, secs: f64) {
        self.target.store(frame_idx, Ordering::Relaxed);
        let _ = self.tx.send(Command::SeekTo(frame_idx, secs));
    }

    /// Aggiorna solo il target (es. durante il playback lineare, dove il
    /// decoder sta già avanzando in sequenza e non serve un seek fisico).
    pub fn set_target(&self, frame_idx: FrameIdx) {
        self.target.store(frame_idx, Ordering::Relaxed);
    }
}

impl Drop for DecodeAhead {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as OsCommand;
    use std::time::Instant;

    /// Bug segnalato: un tetto di frame fisso (indipendente dalla
    /// risoluzione) o penalizza inutilmente le sorgenti leggere (1080p) per
    /// stare sotto OOM sulle pesanti (4K), o va OOM su quelle pesanti se
    /// tarato per quelle leggere. Un budget di memoria elimina il
    /// compromesso: la stessa quantità di RAM, distribuita su più o meno
    /// frame a seconda del peso di ciascuno.
    #[test]
    fn frame_cache_capacity_scales_inversely_with_resolution() {
        let cap_1080p = frame_cache_capacity(1_200_000_000, 1920, 1080);
        let cap_4k = frame_cache_capacity(1_200_000_000, 3840, 2160);

        // 4K ha 4 volte i pixel di 1080p: a parità di budget, circa un
        // quarto dei frame.
        assert!(cap_1080p > cap_4k * 3);
        assert!((cap_1080p as f64 / cap_4k as f64 - 4.0).abs() < 0.1);
    }

    #[test]
    fn frame_cache_capacity_never_drops_below_the_floor_on_extreme_resolutions() {
        // Budget volutamente assurdo per una risoluzione enorme: non deve
        // mai scendere sotto `MIN_CACHE_FRAMES`, né panicare per una
        // divisione che azzera la capacità.
        let cap = frame_cache_capacity(1024, 7680, 4320);
        assert_eq!(cap, MIN_CACHE_FRAMES);
    }

    #[test]
    fn frame_cache_capacity_respects_a_generous_budget() {
        // Budget ampio su una risoluzione piccola: la capacità deve
        // riflettere il budget (molti frame), non restare bloccata al
        // minimo.
        let cap = frame_cache_capacity(100_000_000, 320, 240);
        assert!(cap > MIN_CACHE_FRAMES * 10, "cap={cap}");
    }

    fn make_test_clip(name: &str, duration_secs: u32) -> PathBuf {
        let dir = std::env::temp_dir().join("vv-media-playback-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);

        let status = OsCommand::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-g",
                "10",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());
        path
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn decode_ahead_fills_cache_from_target() {
        let path = make_test_clip("ahead.mp4", 1);
        // Budget generoso per la fixture 320x240 (~40MB => >100 frame di
        // capacità): vedi doc di `DecodeAhead::spawn`, il secondo
        // parametro è un budget di memoria, non più un conteggio di frame.
        let player = DecodeAhead::spawn(path, 40_000_000, 50).unwrap();
        player.set_target(0);

        assert!(
            wait_until(|| player.cache().contains(0), Duration::from_secs(2)),
            "il frame 0 doveva finire in cache entro il timeout"
        );
        assert!(
            wait_until(|| player.cache().contains(10), Duration::from_secs(2)),
            "un frame più avanti doveva finire in cache (decode-ahead)"
        );
    }

    #[test]
    fn decode_ahead_seek_jumps_the_worker() {
        let path = make_test_clip("ahead_seek.mp4", 3);
        let player = DecodeAhead::spawn(path, 65_000_000, 50).unwrap();

        player.seek(50, 2.0);

        assert!(
            wait_until(|| player.cache().contains(50), Duration::from_secs(2))
                || wait_until(
                    || (48..=52).any(|i| player.cache().contains(i)),
                    Duration::from_secs(2)
                ),
            "dopo il seek il worker doveva decodificare vicino al target"
        );
    }
}
