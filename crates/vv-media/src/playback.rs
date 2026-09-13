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

impl DecodeAhead {
    /// Apre il decoder subito nel thread chiamante (per propagare un
    /// eventuale errore di apertura in modo sincrono), poi sposta il
    /// decode continuo su un thread dedicato.
    pub fn spawn(
        path: PathBuf,
        cache_capacity: usize,
        ahead_frames: i64,
    ) -> Result<Self, crate::MediaError> {
        let decoder = Decoder::open(&path)?;
        let width = decoder.width();
        let height = decoder.height();
        let fps = decoder.fps();
        let decoder = SendDecoder(decoder);

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
        let player = DecodeAhead::spawn(path, 100, 50).unwrap();
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
        let player = DecodeAhead::spawn(path, 200, 50).unwrap();

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
