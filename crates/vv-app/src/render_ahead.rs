//! Buffer video a livello di *timeline*, non di singola clip: un thread
//! dedicato cammina in avanti dal playhead per `LOOKAHEAD_SECS`,
//! attraversando quante clip servono (tagli netti, vuoti, stesso media o
//! diverso — nessun caso speciale), e mantiene una `FrameCache` per ogni
//! media coinvolto nella finestra. Sostituisce il precedente sistema di
//! preload "per clip" (`GapPlayback`/`NextPreload`/
//! `is_seamless_continuation`), che richiedeva un caso a parte per ogni
//! nuovo scenario incontrato (proposta dell'utente, che ha notato con
//! l'indicatore "buffered" che il buffer si fermava sempre al bordo
//! della clip successiva).
//!
//! Il rendering (compositing crop/zoom via `vv_render::Compositor`) resta
//! sul thread UI, invariato: qui si bufferizza solo il decode — il passo
//! costoso — non il compositing, che è già economico (vedi doc di
//! `vv_render::Compositor`).
//!
//! Stesso stile di `vv_media::playback::DecodeAhead` (thread singolo,
//! target atomico, canale di comandi) ma generalizzato per camminare la
//! timeline invece di un solo file. Solo il VIDEO: l'audio resta gestito
//! da `Player` (clip attiva, scambiata al taglio) — vedi ARCHITECTURE.md
//! e la nota nel piano di questa modifica sul perché l'audio non è (per
//! ora) parte di questo worker.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use vv_core::{ClipSource, FrameIdx, MediaId, Project, Timeline, TimelineId};
use vv_media::{Decoder, FrameCache, FrameRgba};

const VIDEO_TRACK: usize = 0;

/// Quanti secondi di timeline tenere bufferizzati avanti dal playhead,
/// attraversando quante clip servono per coprirli.
const LOOKAHEAD_SECS: f64 = 3.0;

/// Intervallo di poll del thread: ogni ciclo rivaluta il target corrente
/// e completa quel che manca fino all'orizzonte di lookahead — una volta
/// raggiunto, i cicli successivi trovano tutto già in cache e tornano
/// subito, quindi un intervallo breve non ha un costo significativo.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Oltre quanti frame di distanza in avanti conviene un seek reale
/// invece di continuare a decodificare in sequenza scartando gli
/// intermedi (un seek flush comunque il decoder, quindi per piccoli
/// spostamenti decodificare in sequenza resta più efficiente).
const SEEK_THRESHOLD_FRAMES: FrameIdx = 120;

enum Command {
    UpdateProject(Box<Project>, TimelineId),
    Stop,
}

/// Vedi il doc del modulo. Uno per `VibeVideoApp` (non uno per clip: la
/// differenza chiave rispetto al sistema precedente).
pub struct RenderAhead {
    caches: Arc<Mutex<HashMap<MediaId, Arc<FrameCache>>>>,
    target: Arc<AtomicI64>,
    cache_budget_bytes: Arc<AtomicUsize>,
    tx: mpsc::Sender<Command>,
    handle: Option<JoinHandle<()>>,
}

impl RenderAhead {
    pub fn spawn(project: Project, timeline_id: TimelineId, cache_budget_bytes: usize) -> Self {
        let caches: Arc<Mutex<HashMap<MediaId, Arc<FrameCache>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let target = Arc::new(AtomicI64::new(0));
        let budget = Arc::new(AtomicUsize::new(cache_budget_bytes));
        let (tx, rx) = mpsc::channel();

        let thread_caches = caches.clone();
        let thread_target = target.clone();
        let thread_budget = budget.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(
                rx,
                thread_caches,
                thread_target,
                thread_budget,
                project,
                timeline_id,
            );
        });

        Self {
            caches,
            target,
            cache_budget_bytes: budget,
            tx,
            handle: Some(handle),
        }
    }

    /// Aggiorna solo il target (playhead di timeline, in frame): chiamata
    /// economica da fare a ogni frame UI durante lo scrub/playback, sia
    /// per un avanzamento continuo sia per un salto — il worker rivaluta
    /// da zero la finestra a ogni ciclo, quindi non serve distinguere i
    /// due casi (a differenza di `DecodeAhead::set_target`/`seek`).
    pub fn set_target(&self, frame: FrameIdx) {
        self.target.store(frame, Ordering::Relaxed);
    }

    pub fn set_cache_budget_bytes(&self, bytes: usize) {
        self.cache_budget_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Da chiamare dopo ogni comando che cambia la disposizione delle
    /// clip (ogni `history.do_command`): il worker lavora su una propria
    /// copia del progetto, non condivisa con la UI (`Project` è già
    /// `Clone`, stesso principio dello snapshot per l'export).
    pub fn update_project(&self, project: &Project, timeline_id: TimelineId) {
        let _ = self.tx.send(Command::UpdateProject(
            Box::new(project.clone()),
            timeline_id,
        ));
    }

    /// Il frame decodificato per `(media_id, source_frame)`, se già in
    /// cache.
    pub fn get_frame(&self, media_id: MediaId, source_frame: FrameIdx) -> Option<Arc<FrameRgba>> {
        self.caches
            .lock()
            .unwrap()
            .get(&media_id)?
            .get(source_frame)
    }

    /// Intervalli (in frame *sorgente*) attualmente in cache per un
    /// media — per l'indicatore visivo "buffered" sulla timeline (il
    /// chiamante li traduce in spazio timeline per la clip in questione,
    /// vedi `map_source_ranges_to_timeline` in main.rs).
    pub fn cached_ranges_for(&self, media_id: MediaId) -> Vec<(FrameIdx, FrameIdx)> {
        self.caches
            .lock()
            .unwrap()
            .get(&media_id)
            .map(|c| c.cached_ranges())
            .unwrap_or_default()
    }
}

impl Drop for RenderAhead {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Decoder tenuto aperto per un media, con la posizione (frame sorgente)
/// che produrrà al prossimo `next_frame()`: permette di decidere se
/// conviene continuare a decodificare in sequenza o fare un seek reale
/// (vedi `ensure_positioned`).
struct OpenDecoder {
    media_id: MediaId,
    decoder: Decoder,
    next_frame: FrameIdx,
}

fn worker_loop(
    rx: mpsc::Receiver<Command>,
    caches: Arc<Mutex<HashMap<MediaId, Arc<FrameCache>>>>,
    target: Arc<AtomicI64>,
    cache_budget_bytes: Arc<AtomicUsize>,
    mut project: Project,
    mut timeline_id: TimelineId,
) {
    let mut open: Option<OpenDecoder> = None;
    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Command::Stop) => return,
            Ok(Command::UpdateProject(p, id)) => {
                project = *p;
                timeline_id = id;
                open = None;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        // Drena eventuali altri comandi già in coda (non bloccante): solo
        // l'ultimo conta, i precedenti sono superati.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::Stop => return,
                Command::UpdateProject(p, id) => {
                    project = *p;
                    timeline_id = id;
                    open = None;
                }
            }
        }

        let from = target.load(Ordering::Relaxed);
        let budget = cache_budget_bytes.load(Ordering::Relaxed);
        walk_and_fill(&project, timeline_id, &caches, &mut open, from, budget);
    }
}

/// Un tratto contiguo di timeline coperto da una singola clip Media, in
/// spazio frame *sorgente*.
struct MediaSegment {
    media_id: MediaId,
    source_start: FrameIdx,
    source_end: FrameIdx,
}

/// Divide `[from_frame, end_frame)` di timeline in segmenti, uno per ogni
/// clip Media attraversata sulla track video: cammina quante clip
/// servono nella stessa passata, saltando vuoti (nessuna clip, quindi
/// nessun segmento) e clip SolidColor (nessun decode necessario) senza
/// bisogno di casi speciali. Funzione pura, testabile senza ffmpeg.
fn collect_media_segments(
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut segments = Vec::new();
    let mut frame = from_frame;
    while frame < end_frame {
        let next_frame = match timeline.active_clip_at(VIDEO_TRACK, frame) {
            Some(clip) => {
                let segment_end_timeline = clip.timeline_end().min(end_frame);
                if let ClipSource::Media(media_id) = &clip.source {
                    let media_id = *media_id;
                    let source_start = clip.source_in + (frame - clip.timeline_start);
                    let source_end =
                        (clip.source_in + (segment_end_timeline - clip.timeline_start) - 1)
                            .max(source_start);
                    segments.push(MediaSegment {
                        media_id,
                        source_start,
                        source_end,
                    });
                }
                segment_end_timeline
            }
            None => timeline
                .tracks
                .get(VIDEO_TRACK)
                .and_then(|t| {
                    t.clips
                        .iter()
                        .filter(|c| c.timeline_start >= frame)
                        .map(|c| c.timeline_start)
                        .min()
                })
                .map(|s| s.min(end_frame))
                .unwrap_or(end_frame),
        };
        if next_frame <= frame {
            break; // sicurezza: non dovrebbe succedere, evita un loop infinito
        }
        frame = next_frame;
    }
    segments
}

/// Assicura che `open` sia un decoder per `media_id` posizionato in modo
/// da poter raggiungere `target` decodificando in avanti in modo
/// efficiente: lo (ri)apre con un seek reale solo se serve un media
/// diverso, o se `target` è molto lontano dalla posizione attuale
/// (indietro, o troppo in avanti) — altrimenti lo lascia continuare da
/// dove si trovava, decodifica in sequenza più efficiente di un seek per
/// piccoli spostamenti. Ritorna `false` se l'apertura del media fallisce
/// (il chiamante salta quel segmento).
fn ensure_positioned(
    open: &mut Option<OpenDecoder>,
    media_id: MediaId,
    path: &Path,
    target: FrameIdx,
) -> bool {
    let needs_new_decoder = match open {
        Some(o) if o.media_id == media_id => {
            target < o.next_frame || target > o.next_frame + SEEK_THRESHOLD_FRAMES
        }
        _ => true,
    };
    if !needs_new_decoder {
        return true;
    }
    let Ok(mut decoder) = Decoder::open(path) else {
        return false;
    };
    let secs = target as f64 / decoder.fps().as_f64().max(1e-9);
    let _ = decoder.seek_to_time(secs);
    // Placeholder: il prossimo `next_frame()` restituisce l'idx *reale*
    // del keyframe da cui riparte (può essere < target), che aggiorna
    // subito questo campo nel loop di decodifica sotto.
    *open = Some(OpenDecoder {
        media_id,
        decoder,
        next_frame: 0,
    });
    true
}

/// Cammina `LOOKAHEAD_SECS` avanti da `from_frame` e riempie la
/// `FrameCache` di ogni media coinvolto. Il budget di memoria è diviso
/// tra i media *distinti* effettivamente nella finestra (non uno fisso a
/// testa: pochi media nella finestra hanno più margine a testa, tanti ne
/// hanno meno, il totale resta sotto controllo) — enumerati con un primo
/// passaggio a secco (`collect_media_segments`) prima di decodificare
/// davvero. I media usciti dalla finestra vengono sfrattati dalla cache
/// condivisa, altrimenti crescerebbe senza limite scorrendo la timeline.
fn walk_and_fill(
    project: &Project,
    timeline_id: TimelineId,
    caches: &Mutex<HashMap<MediaId, Arc<FrameCache>>>,
    open: &mut Option<OpenDecoder>,
    from_frame: FrameIdx,
    cache_budget_bytes: usize,
) {
    let Some(timeline) = project.timelines.get(timeline_id) else {
        return;
    };
    let fps = timeline.fps.as_f64().max(1e-9);
    let lookahead_frames = ((LOOKAHEAD_SECS * fps).round() as FrameIdx).max(1);
    let end_frame = from_frame + lookahead_frames;

    let segments = collect_media_segments(timeline, from_frame, end_frame);
    if segments.is_empty() {
        return;
    }
    let distinct_media: HashSet<MediaId> = segments.iter().map(|s| s.media_id).collect();
    let per_media_budget = cache_budget_bytes / distinct_media.len().max(1);

    caches
        .lock()
        .unwrap()
        .retain(|id, _| distinct_media.contains(id));

    for segment in segments {
        let Some(path) = project
            .media_pool
            .get(segment.media_id)
            .map(|m| m.path.clone())
        else {
            continue;
        };
        if !ensure_positioned(open, segment.media_id, &path, segment.source_start) {
            continue;
        }
        let (width, height) = {
            let d = &open.as_ref().unwrap().decoder;
            (d.width(), d.height())
        };
        let capacity = vv_media::frame_cache_capacity(per_media_budget, width, height);
        let cache = caches
            .lock()
            .unwrap()
            .entry(segment.media_id)
            .or_insert_with(|| Arc::new(FrameCache::new(capacity)))
            .clone();

        loop {
            let next = open.as_ref().unwrap().next_frame;
            if next > segment.source_end {
                break;
            }
            if cache.contains(next) {
                open.as_mut().unwrap().next_frame = next + 1;
                continue;
            }
            match open.as_mut().unwrap().decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    cache.insert(idx, Arc::new(frame));
                    open.as_mut().unwrap().next_frame = idx + 1;
                }
                _ => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as OsCommand;
    use vv_core::{Clip, ClipId, EffectStack, MediaItem, MediaMeta, Rational, Track, TrackKind};

    fn make_test_clip(dir_name: &str, file_name: &str, duration_secs: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        let status = OsCommand::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());
        path
    }

    /// Due `MediaId` distinti (slotmap key, non generabili a mano):
    /// bastano per i test puri di `collect_media_segments`, che non
    /// hanno bisogno di un `MediaItem` reale dietro.
    fn dummy_media_item() -> MediaItem {
        MediaItem {
            path: "dummy.mp4".into(),
            meta: MediaMeta {
                duration_frames: 0,
                fps: Rational::new(25, 1),
                width: 0,
                height: 0,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        }
    }

    fn two_media_ids() -> (MediaId, MediaId) {
        let mut project = Project::default();
        let a = project.media_pool.insert(dummy_media_item());
        let b = project.media_pool.insert(dummy_media_item());
        (a, b)
    }

    fn media_clip(id: u64, media_id: MediaId, start: FrameIdx, len: FrameIdx) -> Clip {
        Clip {
            id: ClipId(id),
            source: ClipSource::Media(media_id),
            source_in: 0,
            source_out: len,
            timeline_start: start,
            effects: EffectStack::default(),
            linked: None,
        }
    }

    fn solid_clip(id: u64, start: FrameIdx, len: FrameIdx) -> Clip {
        Clip {
            id: ClipId(id),
            source: ClipSource::SolidColor,
            source_in: 0,
            source_out: len,
            timeline_start: start,
            effects: EffectStack::default(),
            linked: None,
        }
    }

    fn timeline_with(tracks: Vec<Track>) -> Timeline {
        Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (320, 240),
            tracks,
        }
    }

    #[test]
    fn collect_media_segments_walks_across_a_straight_cut_between_two_media() {
        let (media_a, media_b) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 50),
                media_clip(2, media_b, 50, 50),
            ],
            muted: false,
        }]);

        let segments = collect_media_segments(&tl, 40, 60);
        assert_eq!(
            segments.len(),
            2,
            "deve attraversare il taglio in un colpo solo"
        );
        assert_eq!(segments[0].source_start, 40);
        assert_eq!(segments[0].source_end, 49);
        assert_eq!(segments[1].source_start, 0);
        assert_eq!(segments[1].source_end, 9);
    }

    #[test]
    fn collect_media_segments_skips_gaps_and_solid_color_without_decoding() {
        let (media_a, _) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 10),
                // vuoto 10..20
                solid_clip(2, 20, 10),
                media_clip(3, media_a, 30, 10),
            ],
            muted: false,
        }]);

        let segments = collect_media_segments(&tl, 0, 40);
        assert_eq!(
            segments.len(),
            2,
            "solo le due clip Media generano segmenti"
        );
        assert_eq!((segments[0].source_start, segments[0].source_end), (0, 9));
        assert_eq!((segments[1].source_start, segments[1].source_end), (0, 9));
    }

    #[test]
    fn collect_media_segments_is_empty_for_a_timeline_with_no_clips() {
        let tl = timeline_with(vec![Track::new(TrackKind::Video)]);
        assert!(collect_media_segments(&tl, 0, 100).is_empty());
    }

    /// Test end-to-end: il worker attraversa un taglio netto tra due
    /// media diversi in un'unica finestra di lookahead, bufferizzando
    /// *entrambi* senza bisogno di alcun caso speciale — esattamente il
    /// comportamento richiesto ("buffer a livello di timeline, non di
    /// singola clip").
    #[test]
    fn render_ahead_buffers_across_a_straight_cut_between_two_different_media() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "b.mp4", 2);

        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path: path_a,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let media_b = project.media_pool.insert(MediaItem {
            path: path_b,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 50),  // [0,50)
                media_clip(2, media_b, 50, 50), // [50,100), adiacente
            ],
            muted: false,
        }]));

        let render_ahead = RenderAhead::spawn(project, timeline_id, 100_000_000);
        // Target vicino alla fine della prima clip: la finestra di
        // lookahead (3s = 75 frame a 25fps) attraversa abbondantemente il
        // taglio a 50.
        render_ahead.set_target(45);

        let start = std::time::Instant::now();
        loop {
            let a_ready = !render_ahead.cached_ranges_for(media_a).is_empty();
            let b_ready = !render_ahead.cached_ranges_for(media_b).is_empty();
            if a_ready && b_ready {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout: a_ready={a_ready} b_ready={b_ready}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
