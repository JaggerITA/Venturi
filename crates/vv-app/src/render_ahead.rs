//! Buffer video a livello di timeline: un thread cammina dal playhead in
//! avanti (e un po' indietro) attraversando tagli, vuoti e track senza casi
//! speciali, e riempie una `SharedFrameCache` unica a budget globale (vedi
//! REFACTOR_PIPELINE.md §2). Solo decode: il compositing resta sul thread UI,
//! l'audio lo suona il mixer.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use vv_core::{ClipSource, FrameIdx, MediaId, Project, Timeline, TimelineId};
use vv_media::{Decoder, FrameYuv420, SharedFrameCache, WantedRange};

/// Secondi bufferizzati avanti dal playhead, di default.
pub const DEFAULT_LOOKAHEAD_SECS: f64 = 3.0;

/// Secondi bufferizzati dietro la testina, di default. Pochi apposta:
/// servono solo a uno scrub avanti-indietro ravvicinato; la finestra in
/// avanti ha sempre la precedenza sul budget.
pub const DEFAULT_BEHIND_SECS: f64 = 2.0;

/// Margine minimo di lookahead/behind anche se configurati a 0: senza
/// cuscinetto il playback va a scatti per la normale variabilità di timing.
const MIN_MARGIN_FRAMES: FrameIdx = 4;

/// Blocchi in cui si spezza la finestra dietro la testina. Decodificarla in
/// un solo seek produrrebbe per ultimi i frame vicini alla testina, e uno
/// scrub all'indietro continuo non li raggiungerebbe mai: dal blocco più
/// vicino al più lontano il buco resta al più di un blocco.
const BEHIND_CHUNK_FRAMES: FrameIdx = 15;

/// Tetto al transito (frame decodificati prima del tratto voluto) in frame,
/// non in byte del budget: un budget stretto non dice nulla sulla lunghezza
/// del GOP. Generoso: serve solo contro GOP patologici.
const TRANSIT_SAFETY_CAP_FRAMES: FrameIdx = 3000;

fn store_secs(atomic: &AtomicU64, secs: f64) {
    atomic.store(secs.max(0.0).to_bits(), Ordering::Relaxed);
}

fn load_secs(atomic: &AtomicU64) -> f64 {
    f64::from_bits(atomic.load(Ordering::Relaxed))
}

/// Un ciclo che trova tutto in cache torna subito: un poll breve costa poco.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Soglia di seek finché il GOP del media non è stato osservato. Bassa: un
/// seek di troppo costa poco, riusa il decoder aperto.
const DEFAULT_SEEK_THRESHOLD_FRAMES: FrameIdx = 30;

/// Tetto alla stima del GOP, finché non arriva un'osservazione più stretta.
const MAX_SEEK_THRESHOLD_FRAMES: FrameIdx = 300;

/// Soglia di seek sui proxy: sono tutto-intra, seekare costa quasi nulla.
/// La stima adattiva lì resterebbe alta durante uno scrub veloce e farebbe
/// decodificare in sequenza decine di frame inutili.
const PROXY_SEEK_THRESHOLD_FRAMES: FrameIdx = 1;

enum Command {
    UpdateProject(Box<Project>, TimelineId),
    /// Passa dai comandi e non da un atomico: il worker deve reagire al cambio
    /// svuotando cache e decoder (i frame hanno la risoluzione sbagliata).
    SetProxyEnabled(bool),
    /// Sveglia il worker subito invece di aspettare `POLL_INTERVAL`.
    Wake,
    Stop,
}

/// Log diagnostico su stderr con `VV_DEBUG_RENDER_AHEAD=1`.
fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("VV_DEBUG_RENDER_AHEAD").is_ok())
}

/// Stato condiviso tra `RenderAhead` (thread UI) e il worker.
struct SharedState {
    caches: SharedFrameCache,
    target: AtomicI64,
    cache_budget_bytes: AtomicUsize,
    /// L'ultimo ciclo del worker ha trovato tutta la finestra in cache: la
    /// UI smette di chiedere repaint per la striscia "buffered".
    caught_up: AtomicBool,
    /// Secondi (bit di un `f64`, vedi `store_secs`) da bufferizzare avanti
    /// e dietro la testina. Atomici e non `Command`: il worker li rilegge a
    /// ogni ciclo, non c'è una transizione a cui reagire.
    lookahead_secs: AtomicU64,
    behind_secs: AtomicU64,
}

/// Vedi il doc del modulo. Uno per `VibeVideoApp`.
pub struct RenderAhead {
    shared: Arc<SharedState>,
    tx: mpsc::Sender<Command>,
    handle: Option<JoinHandle<()>>,
}

impl RenderAhead {
    pub fn spawn(
        project: Project,
        timeline_id: TimelineId,
        cache_budget_bytes: usize,
        proxy_enabled: bool,
        lookahead_secs: f64,
        behind_secs: f64,
    ) -> Self {
        let shared = Arc::new(SharedState {
            caches: SharedFrameCache::new(),
            target: AtomicI64::new(0),
            cache_budget_bytes: AtomicUsize::new(cache_budget_bytes),
            caught_up: AtomicBool::new(false),
            lookahead_secs: AtomicU64::new(lookahead_secs.max(0.0).to_bits()),
            behind_secs: AtomicU64::new(behind_secs.max(0.0).to_bits()),
        });
        let (tx, rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let handle = std::thread::spawn(move || {
            worker_loop(rx, &thread_shared, project, timeline_id, proxy_enabled);
        });
        Self {
            shared,
            tx,
            handle: Some(handle),
        }
    }

    /// Nuovo playhead. Se cambia segna subito "non bufferizzato" (o la UI
    /// smetterebbe di chiedere repaint su uno stato vecchio) e sveglia il
    /// worker: con finestre strette 50 ms di attesa limitano il playback.
    pub fn set_target(&self, frame: FrameIdx) {
        let previous = self.shared.target.swap(frame, Ordering::Relaxed);
        if previous != frame {
            self.shared.caught_up.store(false, Ordering::Relaxed);
            let _ = self.tx.send(Command::Wake);
        }
    }

    pub fn set_cache_budget_bytes(&self, bytes: usize) {
        self.shared.cache_budget_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Secondi bufferizzati avanti (menu Playback > Proxy), mai sotto
    /// `MIN_MARGIN_FRAMES`.
    pub fn set_lookahead_secs(&self, secs: f64) {
        store_secs(&self.shared.lookahead_secs, secs);
        self.shared.caught_up.store(false, Ordering::Relaxed);
    }

    /// Quanti secondi di timeline bufferizzare anche *dietro* la testina
    /// (menu Playback > Proxy) — vedi doc di `DEFAULT_BEHIND_SECS`.
    pub fn set_behind_secs(&self, secs: f64) {
        store_secs(&self.shared.behind_secs, secs);
        self.shared.caught_up.store(false, Ordering::Relaxed);
    }

    /// Il worker lavora su una copia del progetto: va aggiornata a ogni
    /// cambio della history.
    pub fn update_project(&self, project: &Project, timeline_id: TimelineId) {
        self.shared.caught_up.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::UpdateProject(
            Box::new(project.clone()),
            timeline_id,
        ));
    }

    /// Toggle "usa proxy" (REFACTOR_PIPELINE.md proxy): il worker
    /// svuota la cache condivisa e riapre da zero ogni decoder sul path
    /// giusto per il nuovo stato — vedi doc di `Command::SetProxyEnabled`.
    pub fn set_proxy_enabled(&self, enabled: bool) {
        self.shared.caught_up.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::SetProxyEnabled(enabled));
    }

    /// Il frame decodificato per `(media_id, source_frame)`, se già in
    /// cache.
    pub fn get_frame(&self, media_id: MediaId, source_frame: FrameIdx) -> Option<Arc<FrameYuv420>> {
        self.shared.caches.get(media_id, source_frame)
    }

    /// `false` finché resta lavoro per la finestra corrente: la UI continua a
    /// chiedere repaint per far avanzare la striscia "buffered".
    pub fn is_caught_up(&self) -> bool {
        self.shared.caught_up.load(Ordering::Relaxed)
    }

    /// Intervalli in cache di un media, in frame sorgente.
    pub fn cached_ranges_for(&self, media_id: MediaId) -> Vec<(FrameIdx, FrameIdx)> {
        self.shared.caches.cached_ranges(media_id)
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

/// Non bloccante e mai un errore: un media che non si decodifica il
/// worker lo salta.
impl crate::frame_provider::FrameProvider for RenderAhead {
    fn frame_for(
        &mut self,
        _project: &Project,
        clip: &vv_core::Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<FrameYuv420>>, String> {
        let Some((media_id, source_frame)) =
            crate::frame_provider::media_source_frame(clip, timeline_frame)
        else {
            return Ok(None);
        };
        Ok(self.get_frame(media_id, source_frame))
    }
}

/// Decoder aperto per un media, con il prossimo frame che produrrà: dice se
/// conviene decodificare in sequenza o seekare. Uno per media, così un
/// taglio tra due media non li fa riaprire.
struct OpenDecoder {
    decoder: Decoder,
    /// Path aperto: se il proxy diventa disponibile o cambia il toggle, il
    /// decoder va riaperto sul nuovo.
    resolved_path: std::path::PathBuf,
    /// `true` quando `resolved_path` è un proxy (REFACTOR_PIPELINE.md
    /// proxy) — vedi `PROXY_SEEK_THRESHOLD_FRAMES` sul perché bypassa la
    /// stima adattiva del GOP invece di limitarsi a inizializzarla.
    is_all_intra: bool,
    next_frame: FrameIdx,
    /// Dopo un seek o un'apertura il primo frame è un keyframe: serve a
    /// imparare il GOP del media.
    just_repositioned: bool,
    /// Ultimo atterraggio da seek e GOP stimato dalla distanza tra atterraggi.
    last_keyframe_landed: Option<FrameIdx>,
    estimated_gop: Option<FrameIdx>,
}

impl OpenDecoder {
    fn fresh(decoder: Decoder, resolved_path: std::path::PathBuf, is_all_intra: bool) -> Self {
        Self {
            decoder,
            resolved_path,
            is_all_intra,
            next_frame: 0,
            just_repositioned: true,
            last_keyframe_landed: None,
            estimated_gop: None,
        }
    }

    /// Circa un GOP osservato, un fallback finché non c'è; fisso sui proxy.
    fn seek_threshold_frames(&self) -> FrameIdx {
        if self.is_all_intra {
            return PROXY_SEEK_THRESHOLD_FRAMES;
        }
        self.estimated_gop.unwrap_or(DEFAULT_SEEK_THRESHOLD_FRAMES)
    }

    /// Aggiorna la stima del GOP con un nuovo atterraggio. Tiene il minimo:
    /// un salto di più GOP la sovrastimerebbe.
    fn record_keyframe_landing(&mut self, idx: FrameIdx) {
        if let Some(prev) = self.last_keyframe_landed
            && idx > prev
        {
            let observed = (idx - prev).min(MAX_SEEK_THRESHOLD_FRAMES);
            self.estimated_gop = Some(match self.estimated_gop {
                Some(g) => g.min(observed),
                None => observed,
            });
        }
        if debug_enabled() {
            eprintln!(
                "[render_ahead] ATTERRAGGIO idx={idx} last_keyframe_landed_prima={:?} estimated_gop_dopo={:?}",
                self.last_keyframe_landed, self.estimated_gop
            );
        }
        self.last_keyframe_landed = Some(idx);
    }
}

fn worker_loop(
    rx: mpsc::Receiver<Command>,
    shared: &SharedState,
    mut project: Project,
    mut timeline_id: TimelineId,
    mut proxy_enabled: bool,
) {
    let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
    // Decoder separati per la finestra dietro la testina: vedi `walk_and_fill`.
    let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
    // `from` del ciclo precedente (solo quello, mai un min/max storico):
    // dice se la testina è tornata indietro.
    let mut last_from_frame: Option<FrameIdx> = None;
    // Ciclo precedente interrotto da un salto della testina: si riparte
    // subito invece di aspettare `POLL_INTERVAL`.
    let mut retry_immediately = false;
    loop {
        let first = if retry_immediately {
            match rx.try_recv() {
                Ok(cmd) => Some(cmd),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        // Anche i comandi già in coda: conta solo lo stato finale.
        for cmd in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match cmd {
                Command::Stop => return,
                Command::UpdateProject(p, id) => {
                    project = *p;
                    timeline_id = id;
                    open.clear();
                    open_behind.clear();
                }
                Command::SetProxyEnabled(v) => {
                    proxy_enabled = v;
                    shared.caches.clear();
                    open.clear();
                    open_behind.clear();
                }
                Command::Wake => {}
            }
        }

        let from = shared.target.load(Ordering::Relaxed);
        let went_backward = last_from_frame.is_some_and(|last| from < last);
        last_from_frame = Some(from);
        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &shared.caches,
            &mut open,
            &mut open_behind,
            from,
            shared.cache_budget_bytes.load(Ordering::Relaxed),
            went_backward,
            proxy_enabled,
            load_secs(&shared.lookahead_secs),
            load_secs(&shared.behind_secs),
            &shared.target,
        );
        retry_immediately = outcome.interrupted;
        shared.caught_up.store(outcome.caught_up, Ordering::Relaxed);
    }
}

/// Cosa ha fatto `position_decoder` per soddisfare la richiesta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Positioned {
    /// Decoder già aperto e già posizionato bene: nessun seek, il fronte
    /// del buffer avanza in sequenza senza alcun costo aggiuntivo.
    Reused,
    /// Decoder già aperto, riposizionato con un seek sul decoder
    /// esistente (`seek_to_time`, non una riapertura).
    Seeked,
    /// Nessun decoder aperto per questo media: aperto da zero.
    Opened,
    /// Apertura del media fallita: il chiamante salta il segmento.
    Failed,
}

/// Tratto di una clip Media nella finestra, in frame sorgente, con la sua
/// posizione in timeline (serve a misurare la distanza dalla testina).
#[derive(Clone, Copy, Debug)]
struct MediaSegment {
    media_id: MediaId,
    source_start: FrameIdx,
    source_end: FrameIdx,
    timeline_start: FrameIdx,
    /// `Clip::rate` della clip da cui viene il segmento: serve a
    /// `chunk_behind_segments_near_to_far` per tradurre un offset in
    /// frame sorgente nel corrispondente offset di timeline.
    rate: vv_core::Rational,
}

/// Segmenti di `[from_frame, end_frame)`, uno per clip Media di ogni track
/// video (anche quelle sotto: si vedono nelle bande di letterbox),
/// dal più vicino alla testina e, a pari posizione, dalla track più alta.
fn collect_media_segments(
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut segments = clipped_media_segments(timeline, from_frame, end_frame);
    segments.sort_by_key(|(track, s)| (s.timeline_start, std::cmp::Reverse(*track)));
    segments.into_iter().map(|(_, s)| s).collect()
}

/// Come `collect_media_segments` per la finestra dietro: dal più vicino
/// alla testina, cioè dal più avanti in timeline.
fn collect_media_segments_behind(
    timeline: &Timeline,
    from_frame: FrameIdx,
    start_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut segments = clipped_media_segments(timeline, start_frame, from_frame);
    segments.sort_by_key(|(track, s)| (std::cmp::Reverse(s.timeline_start), std::cmp::Reverse(*track)));
    segments.into_iter().map(|(_, s)| s).collect()
}

/// La parte comune delle due funzioni sopra: ogni clip Media di ogni track
/// video ritagliata su `[from_frame, end_frame)`, con l'indice della sua
/// track per poterle poi ordinare. Non ordinati.
fn clipped_media_segments(
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
) -> Vec<(usize, MediaSegment)> {
    let mut segments = Vec::new();
    for (track_index, track) in timeline.tracks_of_kind(vv_core::TrackKind::Video) {
        if track.muted {
            continue;
        }
        for clip in track.clips.iter().filter(|c| !c.disabled) {
            let ClipSource::Media(media_id) = &clip.source else {
                continue;
            };
            let segment_start = clip.timeline_start.max(from_frame);
            let segment_end = clip.timeline_end().min(end_frame);
            if segment_end <= segment_start {
                continue;
            }
            // Mappatura clip→frame-sorgente condivisa con l'export
            // (`vv_core::Clip::source_frame_at`, vedi doc lì per il
            // perché — REFACTOR_PIPELINE.md B1).
            let source_start = clip.source_frame_at(segment_start);
            let source_end = clip.source_frame_at(segment_end - 1);
            segments.push((
                track_index,
                MediaSegment {
                    media_id: *media_id,
                    source_start,
                    source_end,
                    timeline_start: segment_start,
                    rate: clip.rate,
                },
            ));
        }
    }
    segments
}

/// Segmenti "in prestito" per le crossing transition attive in
/// `[from_frame, end_frame)`: `extrapolated_frame_for` (`frame_provider.rs`)
/// non congela mai una clip sul suo ultimo/primo frame reale, ma continua a
/// giocare il girato che il trim aveva scartato finché non arriva alla vera
/// fine del media — cioè, oltre il proprio bordo dichiarato, chiede un
/// range di frame sorgente crescente, non un singolo frame fisso. Nessun
/// segmento di `clipped_media_segments` lo copre mai (è ritagliato stretto
/// sul range dichiarato di ciascuna clip), quindi senza questo
/// `SharedFrameCache::reconcile` lo sfratta (o non lo scarica mai) appena
/// la testina supera il taglio, congelando il compositing per quella metà
/// della finestra della crossing.
fn crossing_borrowed_segments(
    project: &Project,
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut segments = Vec::new();
    for (_, track) in timeline.tracks_of_kind(vv_core::TrackKind::Video) {
        if track.muted {
            continue;
        }
        for crossing in &track.crossings {
            let (Some(left), Some(right)) = (track.clip(crossing.left_clip), track.clip(crossing.right_clip)) else {
                continue;
            };
            let window = crossing.window(left, right);
            if window.end <= from_frame || window.start >= end_frame {
                continue;
            }
            // `left` presta il tratto dopo il proprio bordo dichiarato,
            // `right` quello prima: l'altra metà di ciascuna è già coperta
            // dal proprio segmento normale.
            if window.end > left.timeline_end() {
                push_borrowed_segment(&mut segments, project, left, left.timeline_end(), window.end - 1);
            }
            if window.start < right.timeline_start {
                push_borrowed_segment(&mut segments, project, right, window.start, right.timeline_start - 1);
            }
        }
    }
    segments
}

/// Un segmento sorgente per il tratto di `clip` prestato a una crossing,
/// `[from_timeline, to_timeline]` (inclusivo) in frame di timeline,
/// clampato agli stessi bordi di `media.meta.duration_frames` che userebbe
/// `extrapolated_frame_for` — altrimenti si chiederebbe di bufferizzare un
/// frame sorgente che il decoder non produrrà mai.
fn push_borrowed_segment(
    segments: &mut Vec<MediaSegment>,
    project: &Project,
    clip: &vv_core::Clip,
    from_timeline: FrameIdx,
    to_timeline: FrameIdx,
) {
    let ClipSource::Media(media_id) = &clip.source else {
        return;
    };
    let Some(item) = project.media_pool.get(*media_id) else {
        return;
    };
    let last = (item.meta.duration_frames - 1).max(0);
    let a = clip.source_frame_at(from_timeline).clamp(0, last);
    let b = clip.source_frame_at(to_timeline).clamp(0, last);
    segments.push(MediaSegment {
        media_id: *media_id,
        source_start: a.min(b),
        source_end: a.max(b),
        timeline_start: from_timeline,
        rate: clip.rate,
    });
}

/// Spezza i segmenti dietro la testina in blocchi di `BEHIND_CHUNK_FRAMES`,
/// dal bordo vicino alla testina (`source_end`) verso quello lontano.
fn chunk_behind_segments_near_to_far(segments: &[MediaSegment]) -> Vec<MediaSegment> {
    let mut chunks = Vec::new();
    for segment in segments {
        let mut chunk_end = segment.source_end;
        loop {
            let chunk_start = (chunk_end - BEHIND_CHUNK_FRAMES + 1).max(segment.source_start);
            let offset = segment.rate.scale_round(chunk_start)
                - segment.rate.scale_round(segment.source_start);
            chunks.push(MediaSegment {
                media_id: segment.media_id,
                source_start: chunk_start,
                source_end: chunk_end,
                timeline_start: segment.timeline_start + offset,
                rate: segment.rate,
            });
            if chunk_start == segment.source_start {
                break;
            }
            chunk_end = chunk_start - 1;
        }
    }
    chunks
}

/// Toglie i blocchi già in cache. Nella finestra dietro il decoder resta
/// fermo sul blocco più lontano del ciclo prima: senza filtro ogni blocco
/// sembrerebbe da riseekare a ogni ciclo, anche a testina ferma.
fn without_already_cached_chunks(
    caches: &SharedFrameCache,
    chunks: Vec<MediaSegment>,
) -> Vec<MediaSegment> {
    chunks
        .into_iter()
        .filter(|chunk| {
            !caches.covers(chunk.media_id, chunk.source_start, chunk.source_end)
        })
        .collect()
}

/// Porta `open[media_id]` dove può coprire `segment_start` decodificando in
/// avanti. Un decoder già oltre il segmento è lo stato normale e si riusa.
/// Si seeka (sul decoder aperto, mai riaprendo: riparsare il container può
/// costare secondi) solo se il segmento è troppo avanti, se la testina è
/// tornata indietro e il decoder l'ha superata, o se la cache ha un buco
/// dove il decoder crede di essere già passato. `went_backward` è deciso
/// una volta per ciclo: per media lo sporcherebbe l'ordine dei segmenti.
fn position_decoder(
    caches: &SharedFrameCache,
    open: &mut HashMap<MediaId, OpenDecoder>,
    media_id: MediaId,
    path: &Path,
    segment_start: FrameIdx,
    went_backward: bool,
    is_all_intra: bool,
    is_image: bool,
) -> Positioned {
    if open.get(&media_id).is_some_and(|o| o.resolved_path != path) {
        open.remove(&media_id);
    }
    if let Some(o) = open.get_mut(&media_id) {
        let needs_seek = segment_start > o.next_frame + o.seek_threshold_frames()
            || (went_backward && segment_start < o.next_frame)
            // Uno sfratto può aver tolto la coda che il decoder crede di aver già
            // prodotto: si verifica sulla cache.
            || (o.next_frame > segment_start
                && !caches.covers(media_id, segment_start, o.next_frame - 1));
        if !needs_seek {
            return Positioned::Reused;
        }
        let secs = segment_start as f64 / o.decoder.fps().as_f64().max(1e-9);
        let debug_start = debug_enabled().then(std::time::Instant::now);
        let _ = o.decoder.seek_to_time(secs);
        if let Some(t) = debug_start {
            eprintln!(
                "[render_ahead] seek (decoder riusato) media={media_id:?} target={segment_start} elapsed={:?}",
                t.elapsed()
            );
        }
        // Placeholder: il prossimo frame dirà dove il seek è atterrato davvero.
        o.next_frame = 0;
        o.just_repositioned = true;
        return Positioned::Seeked;
    }
    // Nessun decoder aperto per questo media: qui l'apertura reale è
    // inevitabile (prima volta, o media diverso da quello aperto finora).
    let debug_start = debug_enabled().then(std::time::Instant::now);
    // Un'immagine va aperta con `open_image`, o andrebbe in EOF dopo il
    // primo frame.
    let opened = if is_image { Decoder::open_image(path) } else { Decoder::open(path) };
    let Ok(mut decoder) = opened else {
        return Positioned::Failed;
    };
    let secs = segment_start as f64 / decoder.fps().as_f64().max(1e-9);
    let _ = decoder.seek_to_time(secs);
    if let Some(t) = debug_start {
        eprintln!(
            "[render_ahead] OPEN (nuovo decoder) media={media_id:?} path={} target={segment_start} elapsed={:?}",
            path.display(),
            t.elapsed()
        );
    }
    open.insert(
        media_id,
        OpenDecoder::fresh(decoder, path.to_path_buf(), is_all_intra),
    );
    Positioned::Opened
}

/// Esito di un ciclo di `walk_and_fill`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalkOutcome {
    /// La testina si è spostata abbastanza da rendere obsoleto il lavoro
    /// rimasto: si riparte subito col target nuovo.
    interrupted: bool,
    /// Tutta la finestra è in cache: la UI può smettere di chiedere repaint.
    caught_up: bool,
}

impl WalkOutcome {
    const SETTLED: Self = Self {
        interrupted: false,
        caught_up: true,
    };
    /// Fermato da budget o transito: c'è ancora lavoro, ma non subito.
    const UNFINISHED: Self = Self {
        interrupted: false,
        caught_up: false,
    };
}

fn walk_and_fill(
    project: &Project,
    timeline_id: TimelineId,
    caches: &SharedFrameCache,
    open: &mut HashMap<MediaId, OpenDecoder>,
    open_behind: &mut HashMap<MediaId, OpenDecoder>,
    from_frame: FrameIdx,
    cache_budget_bytes: usize,
    went_backward: bool,
    proxy_enabled: bool,
    lookahead_secs: f64,
    behind_secs: f64,
    target: &AtomicI64,
) -> WalkOutcome {
    let Some(timeline) = project.timelines.get(timeline_id) else {
        return WalkOutcome::SETTLED;
    };
    let fps = timeline.fps.as_f64().max(1e-9);
    let lookahead_frames = ((lookahead_secs * fps).round() as FrameIdx).max(MIN_MARGIN_FRAMES);
    let behind_frames = ((behind_secs * fps).round() as FrameIdx).max(MIN_MARGIN_FRAMES);
    let end_frame = from_frame + lookahead_frames;
    let start_frame = (from_frame - behind_frames).max(0);

    let mut forward_segments = collect_media_segments(timeline, from_frame, end_frame);
    let mut behind_segments = collect_media_segments_behind(timeline, from_frame, start_frame);
    forward_segments.extend(crossing_borrowed_segments(project, timeline, from_frame, end_frame));
    behind_segments.extend(crossing_borrowed_segments(project, timeline, start_frame, from_frame));
    if forward_segments.is_empty() && behind_segments.is_empty() {
        return WalkOutcome::SETTLED;
    }
    let forward_media: HashSet<MediaId> = forward_segments.iter().map(|s| s.media_id).collect();
    let behind_media: HashSet<MediaId> = behind_segments.iter().map(|s| s.media_id).collect();
    open.retain(|id, _| forward_media.contains(id));
    open_behind.retain(|id, _| behind_media.contains(id));

    let window: Vec<WantedRange> = forward_segments
        .iter()
        .chain(behind_segments.iter())
        .map(|s| WantedRange {
            media_id: s.media_id,
            source_start: s.source_start,
            source_end: s.source_end,
            timeline_start: s.timeline_start,
            rate: s.rate,
        })
        .collect();
    // Scarta ciò che è fuori da entrambe le finestre e, oltre il budget, il
    // più lontano dalla testina.
    caches.reconcile(from_frame, &window, cache_budget_bytes);
    if debug_enabled() {
        for id in forward_media
            .iter()
            .chain(behind_media.iter())
            .collect::<HashSet<_>>()
        {
            eprintln!(
                "[render_ahead] DOPO-RECONCILE media={id:?} cached_ranges={:?}",
                caches.cached_ranges(*id)
            );
        }
    }

    let ctx = FillContext {
        project,
        caches,
        went_backward,
        cache_budget_bytes,
        from_frame,
        proxy_enabled,
        target,
    };
    // Prima la finestra in avanti: dietro riceve solo il budget che avanza.
    if let ControlFlow::Break(outcome) = fill_segments(&forward_segments, &ctx, open) {
        return outcome;
    }
    // Decoder separati da quelli in avanti: uno stesso decoder resterebbe
    // oltre ogni segmento dietro e li salterebbe tutti. Per i blocchi rimasti
    // un seek serve sempre, quindi `went_backward` è forzato.
    let all_behind_chunks = chunk_behind_segments_near_to_far(&behind_segments);
    let behind_chunks = without_already_cached_chunks(caches, all_behind_chunks.clone());
    if debug_enabled() {
        eprintln!(
            "[render_ahead] DIETRO from_frame={from_frame} behind_secs={behind_secs} behind_frames={behind_frames} blocchi_totali={} blocchi_da_processare={} bytes_used={} budget={cache_budget_bytes}",
            all_behind_chunks.len(),
            behind_chunks.len(),
            caches.bytes_used(),
        );
        for c in &all_behind_chunks {
            let processed = behind_chunks
                .iter()
                .any(|b| b.source_start == c.source_start && b.source_end == c.source_end);
            eprintln!(
                "[render_ahead]   blocco media={:?} [{},{}] {}",
                c.media_id,
                c.source_start,
                c.source_end,
                if processed {
                    "DA PROCESSARE"
                } else {
                    "già in cache, saltato"
                }
            );
        }
    }
    let behind_ctx = FillContext {
        went_backward: true,
        ..ctx
    };
    if let ControlFlow::Break(outcome) = fill_segments(&behind_chunks, &behind_ctx, open_behind) {
        return outcome;
    }
    // Toglie subito i frame di transito rimasti: la UI legge lo stato appena
    // dichiarato `caught_up` e non deve vedere una striscia più larga del vero.
    caches.reconcile(from_frame, &window, cache_budget_bytes);
    WalkOutcome::SETTLED
}

/// Parametri di `fill_segments` comuni alle due finestre.
struct FillContext<'a> {
    project: &'a Project,
    caches: &'a SharedFrameCache,
    went_backward: bool,
    cache_budget_bytes: usize,
    from_frame: FrameIdx,
    /// Toggle "usa proxy" (REFACTOR_PIPELINE.md proxy) come letto
    /// dall'ultimo `Command::SetProxyEnabled` — vedi `fill_segments`
    /// dove decide se risolvere il path sorgente o quello del proxy.
    proxy_enabled: bool,
    target: &'a AtomicI64,
}

/// Decodifica quanto serve a coprire `segments`, fermandosi se il budget è
/// saturo o se la testina live si è spostata troppo. `Break` porta l'esito
/// finale.
fn fill_segments(
    segments: &[MediaSegment],
    ctx: &FillContext,
    open: &mut HashMap<MediaId, OpenDecoder>,
) -> ControlFlow<WalkOutcome> {
    for segment in segments {
        let Some(item) = ctx.project.media_pool.get(segment.media_id) else {
            continue;
        };
        // Già tutto in cache: non ci si fida della posizione che il decoder crede
        // di avere.
        if ctx.caches.covers(segment.media_id, segment.source_start, segment.source_end) {
            continue;
        }
        // Senza spazio nemmeno per un frame non si comincia: si pagherebbe il
        // transito per un frame poi rifiutato, ciclo dopo ciclo. Il transito non
        // conta mai contro il budget del tratto voluto.
        let estimated_frame_bytes = vv_media::yuv420_frame_bytes(item.meta.width, item.meta.height);
        if ctx.caches.bytes_used() + estimated_frame_bytes > ctx.cache_budget_bytes {
            if debug_enabled() {
                eprintln!(
                    "[render_ahead] BUDGET-GIA-SATURO media={:?} segment=[{},{}] bytes_used={} budget={}",
                    segment.media_id,
                    segment.source_start,
                    segment.source_end,
                    ctx.caches.bytes_used(),
                    ctx.cache_budget_bytes
                );
            }
            return ControlFlow::Break(WalkOutcome::UNFINISHED);
        }
        // Proxy solo se attivo e già generato; altrimenti il sorgente, finché il
        // proxy non compare su disco.
        let is_proxy = ctx.proxy_enabled && vv_media::proxy::proxy_exists(item.content_hash);
        let path = if is_proxy {
            vv_media::proxy::proxy_path_for(item.content_hash)
        } else {
            item.path.clone()
        };
        if position_decoder(
            ctx.caches,
            open,
            segment.media_id,
            &path,
            segment.source_start,
            ctx.went_backward,
            is_proxy,
            item.meta.is_image(),
        ) == Positioned::Failed
        {
            continue;
        }

        // `next_frame` è un placeholder finché il primo frame dopo il seek non
        // dice la posizione vera.
        let mut resumed = false;
        // Byte di transito di questo giro: non devono impedire al segmento di
        // raggiungere sé stesso con un budget stretto.
        let mut transit_bytes: usize = 0;
        let mut transit_frames: FrameIdx = 0;
        loop {
            let od = open.get_mut(&segment.media_id).unwrap();
            if od.next_frame > segment.source_end {
                break;
            }
            // Ricongiunti con una parte già in cache fino in fondo al segmento: il
            // resto c'è già. Serve un unico intervallo contiguo, non due punti
            // presenti su isole diverse.
            if resumed
                && ctx.caches.covers(segment.media_id, od.next_frame, segment.source_end)
            {
                if debug_enabled() {
                    eprintln!(
                        "[render_ahead] RIAGGANCIO media={:?} segment=[{},{}] next_frame={}",
                        segment.media_id, segment.source_start, segment.source_end, od.next_frame
                    );
                }
                break;
            }
            let mut threshold_frames = od.seek_threshold_frames();
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    if od.just_repositioned {
                        od.record_keyframe_landing(idx);
                        od.just_repositioned = false;
                        threshold_frames = od.seek_threshold_frames();
                    }
                    let frame_bytes = frame.byte_len();
                    if idx >= segment.source_start {
                        if ctx.caches.bytes_used().saturating_sub(transit_bytes)
                            >= ctx.cache_budget_bytes
                        {
                            if debug_enabled() {
                                eprintln!(
                                    "[render_ahead] BUDGET-SATURO media={:?} segment=[{},{}] next_frame={} bytes_used={} transit_bytes={transit_bytes} budget={}",
                                    segment.media_id,
                                    segment.source_start,
                                    segment.source_end,
                                    od.next_frame,
                                    ctx.caches.bytes_used(),
                                    ctx.cache_budget_bytes
                                );
                            }
                            return ControlFlow::Break(WalkOutcome::UNFINISHED);
                        }
                    } else {
                        // Transito prima del segmento: si tiene comunque, un segmento vicino in
                        // questo stesso giro lo riusa invece di riattraversare il GOP.
                        if transit_frames >= TRANSIT_SAFETY_CAP_FRAMES {
                            if debug_enabled() {
                                eprintln!(
                                    "[render_ahead] TRANSITO-ECCESSIVO media={:?} segment=[{},{}] next_frame={} transit_frames={transit_frames}",
                                    segment.media_id,
                                    segment.source_start,
                                    segment.source_end,
                                    od.next_frame,
                                );
                            }
                            return ControlFlow::Break(WalkOutcome::UNFINISHED);
                        }
                        transit_bytes += frame_bytes;
                        transit_frames += 1;
                    }
                    ctx.caches.insert(segment.media_id, idx, Arc::new(frame));
                    od.next_frame = idx + 1;
                    resumed = true;
                }
                Ok(None) => {
                    if debug_enabled() {
                        eprintln!(
                            "[render_ahead] DECODE-FINE(EOF) media={:?} segment=[{},{}] next_frame={}",
                            segment.media_id,
                            segment.source_start,
                            segment.source_end,
                            od.next_frame
                        );
                    }
                    break;
                }
                Err(e) => {
                    if debug_enabled() {
                        eprintln!(
                            "[render_ahead] DECODE-FINE(ERR) media={:?} segment=[{},{}] next_frame={} errore={e}",
                            segment.media_id,
                            segment.source_start,
                            segment.source_end,
                            od.next_frame
                        );
                    }
                    break;
                }
            }
            // Target live: se la testina si è spostata oltre la soglia il prefetch è
            // obsoleto e si ricomincia subito.
            let live = ctx.target.load(Ordering::Relaxed);
            if (live - ctx.from_frame).abs() > threshold_frames {
                if debug_enabled() {
                    eprintln!(
                        "[render_ahead] INTERROTTO media={:?} segment=[{},{}] live={live} from_frame={} threshold={threshold_frames} estimated_gop={:?} last_keyframe_landed={:?}",
                        segment.media_id,
                        segment.source_start,
                        segment.source_end,
                        ctx.from_frame,
                        open.get(&segment.media_id).unwrap().estimated_gop,
                        open.get(&segment.media_id).unwrap().last_keyframe_landed,
                    );
                }
                return ControlFlow::Break(WalkOutcome {
                    interrupted: true,
                    caught_up: false,
                });
            }
        }

        if debug_enabled() {
            let final_next_frame = open.get(&segment.media_id).unwrap().next_frame;
            let from_frame = ctx.from_frame;
            eprintln!(
                "[render_ahead] media={:?} target_frame={from_frame} segment=[{},{}] next_frame_after={final_next_frame} bytes_used={} cached_ranges={:?}",
                segment.media_id,
                segment.source_start,
                segment.source_end,
                ctx.caches.bytes_used(),
                ctx.caches.cached_ranges(segment.media_id)
            );
        }
    }
    ControlFlow::Continue(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vv_core::{Clip, ClipId, MediaItem, MediaMeta, Rational, Track, TrackKind};

    fn make_test_clip(dir_name: &str, file_name: &str, duration_secs: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            &path,
        );
        path
    }

    fn make_test_image(dir_name: &str, file_name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=green:size=320x240:rate=1:duration=1",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &path,
        );
        path
    }

    /// Regressione end-to-end per il supporto immagini
    /// (`Decoder::open_image`, `position_decoder`): un'immagine ferma
    /// stirata su una clip di 2.4s a 25fps chiede frame sorgente fino a
    /// ~60 — ben oltre l'unico frame reale che un'immagine ha. Prima del
    /// supporto dedicato, un vero `Decoder::open` sarebbe andato in EOF
    /// su qualunque posizione oltre la prima, lasciando la cache scoperta
    /// per il resto della clip.
    #[test]
    fn walk_and_fill_decodes_a_stretched_image_clip_past_its_only_real_frame() {
        let path = make_test_image("vv-app-render-ahead-image-test", "still.png");
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: vv_core::IMAGE_DURATION_FRAMES,
                fps: vv_media::IMAGE_FPS,
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        // 60 frame (2.4s a 25fps): dentro a `DEFAULT_LOOKAHEAD_SECS`
        // (3s), altrimenti l'ultimo frame resterebbe fuori dalla finestra
        // per un motivo indipendente da questo test (il lookahead, non il
        // supporto immagini).
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 60)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 4 * 200;
        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            generous_budget,
            false,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );

        assert!(!outcome.interrupted);
        for frame in [0, 1, 30, 59] {
            assert!(
                caches.get(media_a, frame).is_some(),
                "frame sorgente {frame} dell'immagine non è stato decodificato"
            );
        }
    }

    /// Come `make_test_clip`, ma con un GOP corto ed esplicito: senza
    /// questo, il keyframe più vicino a un target lontano dall'inizio
    /// resta comunque quello iniziale (keyint di default 250, più lungo
    /// della durata dei clip di test), quindi un seek più avanti nel
    /// file dovrebbe comunque riattraversare in sequenza tutto ciò che
    /// lo precede — mascherando un'eventuale sfratto scorretto di una
    /// porzione già bufferizzata, perché verrebbe rigenerata comunque
    /// nel passaggio. Con un GOP corto il seek può saltare direttamente
    /// vicino al target senza toccare le porzioni precedenti già in
    /// cache, rendendo visibile un eventuale sfratto indebito.
    fn make_test_clip_with_short_gop(
        dir_name: &str,
        file_name: &str,
        duration_secs: u32,
        gop: u32,
    ) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-g",
                &gop.to_string(),
                "-keyint_min",
                &gop.to_string(),
            ],
            &path,
        );
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
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
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

    /// Frame 1x1 fittizio: basta per popolare `SharedFrameCache` nei
    /// test puri di `without_already_cached_chunks`, che verificano solo
    /// quali indici risultano coperti, mai il contenuto vero e proprio.
    fn dummy_frame() -> FrameYuv420 {
        FrameYuv420 {
            width: 1,
            height: 1,
            y: vec![0],
            u: vec![0],
            v: vec![0],
            u_width: 1,
            u_height: 1,
            matrix: vv_media::ColorMatrix::Bt601,
            full_range: false,
        }
    }

    fn media_clip(id: u64, media_id: MediaId, start: FrameIdx, len: FrameIdx) -> Clip {
        Clip::from_source_range(
            ClipId(id),
            ClipSource::Media(media_id),
            0,
            len,
            start,
            Rational::one(),
        )
    }

    /// Come `media_clip`, ma con un `source_in` esplicito — serve a
    /// simulare due clip sulla timeline che sono *tagli* dello stesso
    /// file lungo (source range sequenziali, non entrambe da 0), il
    /// caso comune che espone il bug del `evict_before` per-segmento.
    fn media_clip_trimmed(
        id: u64,
        media_id: MediaId,
        timeline_start: FrameIdx,
        source_in: FrameIdx,
        len: FrameIdx,
    ) -> Clip {
        Clip::from_source_range(
            ClipId(id),
            ClipSource::Media(media_id),
            source_in,
            source_in + len,
            timeline_start,
            Rational::one(),
        )
    }

    fn solid_clip(id: u64, start: FrameIdx, len: FrameIdx) -> Clip {
        Clip::from_source_range(ClipId(id), ClipSource::SolidColor, 0, len, start, Rational::one())
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
            solo: false,
            locked: false,
            crossings: Vec::new(),
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
            solo: false,
            locked: false,
            crossings: Vec::new(),
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

    /// Con più track video il buffer deve coprirle tutte, non solo quella
    /// in cima: sotto le bande di una clip con aspect diverso da quello
    /// della timeline si vede il layer sotto, che quindi va decodificato.
    #[test]
    fn collect_media_segments_covers_every_video_track_topmost_first() {
        let (media_a, media_b) = two_media_ids();
        let tl = timeline_with(vec![
            Track {
                kind: TrackKind::Video,
                clips: vec![media_clip(1, media_a, 0, 50)],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
            Track {
                kind: TrackKind::Video,
                clips: vec![media_clip(2, media_b, 0, 50)],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            },
        ]);

        let segments = collect_media_segments(&tl, 0, 50);
        assert_eq!(segments.len(), 2);
        assert_eq!(
            (segments[0].media_id, segments[1].media_id),
            (media_b, media_a),
            "a pari posizione la track in cima ha la priorità"
        );
    }

    #[test]
    fn collect_media_segments_is_empty_for_a_timeline_with_no_clips() {
        let tl = timeline_with(vec![Track::new(TrackKind::Video)]);
        assert!(collect_media_segments(&tl, 0, 100).is_empty());
    }

    #[test]
    fn collect_media_segments_behind_walks_across_a_straight_cut_between_two_media() {
        let (media_a, media_b) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 50),
                media_clip(2, media_b, 50, 50),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        // Finestra dietro [40,60): attraversa il taglio a 50 andando
        // all'indietro, simmetrico al test forward sopra.
        let segments = collect_media_segments_behind(&tl, 60, 40);
        assert_eq!(
            segments.len(),
            2,
            "deve attraversare il taglio all'indietro in un colpo solo"
        );
        // Ordine di scoperta: dal più vicino alla testina (60) al più
        // lontano — prima il pezzo di media_b [50,60), poi quello di
        // media_a [40,50).
        assert_eq!(segments[0].source_start, 0);
        assert_eq!(segments[0].source_end, 9);
        assert_eq!(segments[1].source_start, 40);
        assert_eq!(segments[1].source_end, 49);
    }

    #[test]
    fn collect_media_segments_behind_skips_gaps_and_solid_color_without_decoding() {
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
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        let segments = collect_media_segments_behind(&tl, 40, 0);
        assert_eq!(
            segments.len(),
            2,
            "solo le due clip Media generano segmenti, il vuoto e la SolidColor vengono saltati"
        );
        assert_eq!((segments[0].source_start, segments[0].source_end), (0, 9));
        assert_eq!((segments[1].source_start, segments[1].source_end), (0, 9));
    }

    #[test]
    fn collect_media_segments_behind_is_empty_for_a_timeline_with_no_clips() {
        let tl = timeline_with(vec![Track::new(TrackKind::Video)]);
        assert!(collect_media_segments_behind(&tl, 100, 0).is_empty());
    }

    #[test]
    fn collect_media_segments_behind_stops_at_the_start_frame_bound() {
        // Un'unica clip lunga [0,200): la finestra dietro deve fermarsi
        // esattamente a `start_frame`, non proseguire fino all'inizio
        // della clip.
        let (media_a, _) = two_media_ids();
        let tl = timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 200)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]);

        let segments = collect_media_segments_behind(&tl, 150, 100);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].timeline_start, 100);
        assert_eq!(segments[0].source_start, 100);
        assert_eq!(segments[0].source_end, 149);
    }

    /// Un segmento dietro la testina più lungo di `BEHIND_CHUNK_FRAMES`
    /// va spezzato in blocchi dal bordo vicino (`source_end`) al bordo
    /// lontano (`source_start`), ognuno da al più `BEHIND_CHUNK_FRAMES`,
    /// senza buchi né sovrapposizioni — vedi doc di
    /// `chunk_behind_segments_near_to_far`.
    #[test]
    fn chunk_behind_segments_near_to_far_splits_from_the_near_edge_without_gaps() {
        let (media_a, _) = two_media_ids();
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 100,
            source_end: 132, // 33 frame: 2 blocchi da 15 + 1 da 3
            timeline_start: 500,
            rate: Rational::one(),
        };

        let chunks = chunk_behind_segments_near_to_far(&[segment]);

        assert_eq!(
            chunks
                .iter()
                .map(|c| (c.source_start, c.source_end))
                .collect::<Vec<_>>(),
            vec![(118, 132), (103, 117), (100, 102)],
            "dal bordo vicino (132) al lontano (100), ognuno da al più BEHIND_CHUNK_FRAMES"
        );
        // `timeline_start` segue lo stesso offset di `source_start`
        // rispetto al segmento originale (mappatura affine, vedi doc).
        assert_eq!(chunks[0].timeline_start, 518);
        assert_eq!(chunks[1].timeline_start, 503);
        assert_eq!(chunks[2].timeline_start, 500);
    }

    /// Un segmento più corto di un blocco produce un solo blocco
    /// identico al segmento originale — nessuna divisione superflua.
    #[test]
    fn chunk_behind_segments_near_to_far_keeps_a_short_segment_whole() {
        let (media_a, _) = two_media_ids();
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 40,
            source_end: 44,
            timeline_start: 40,
            rate: Rational::one(),
        };

        let chunks = chunk_behind_segments_near_to_far(&[segment]);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].source_start, 40);
        assert_eq!(chunks[0].source_end, 44);
    }

    /// `without_already_cached_chunks` deve scartare solo i blocchi
    /// interamente coperti da un *singolo* intervallo in cache — un
    /// blocco scoperto o solo parzialmente coperto resta nella lista
    /// (la riverifica in quel caso è comunque economica, vedi doc della
    /// funzione).
    #[test]
    fn without_already_cached_chunks_drops_only_fully_covered_chunks() {
        let (media_a, media_b) = two_media_ids();
        let caches = SharedFrameCache::new();
        // media_a: [100,132] interamente in cache (un unico frame fittizio
        // per ogni indice, giusto per popolare `cached_ranges`).
        for idx in 100..=132 {
            caches.insert(media_a, idx, Arc::new(dummy_frame()));
        }
        // media_b: solo [100,110] in cache, non l'intero blocco richiesto.
        for idx in 100..=110 {
            caches.insert(media_b, idx, Arc::new(dummy_frame()));
        }

        let chunks = vec![
            // media_a: interamente coperto, va scartato.
            MediaSegment {
                media_id: media_a,
                source_start: 118,
                source_end: 132,
                timeline_start: 118,
                rate: Rational::one(),
            },
            // media_b: solo parzialmente coperto, resta.
            MediaSegment {
                media_id: media_b,
                source_start: 95,
                source_end: 110,
                timeline_start: 95,
                rate: Rational::one(),
            },
            // media_a: fuori dall'intervallo in cache, resta.
            MediaSegment {
                media_id: media_a,
                source_start: 50,
                source_end: 64,
                timeline_start: 50,
                rate: Rational::one(),
            },
        ];

        let remaining = without_already_cached_chunks(&caches, chunks);

        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].media_id, media_b);
        assert_eq!(remaining[1].source_start, 50);
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
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
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
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
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
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
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

    /// Riproduzione end-to-end del bug segnalato dall'utente: durante una
    /// crossing transition, `extrapolated_frame_for` (`frame_provider.rs`)
    /// chiede per il lato "in prestito" un tratto di frame sorgente che
    /// appartiene al range NORMALE di una clip ma cade fuori dal range
    /// dichiarato dell'ALTRA — nessun `MediaSegment` normale lo copre, e
    /// senza `crossing_borrowed_segments` `SharedFrameCache::reconcile` lo
    /// sfratta (o non lo scarica mai) appena la testina supera il taglio,
    /// congelando il compositing per metà della finestra della crossing.
    /// `clip_a` è tagliata corta (30 delle 50 frame reali disponibili) e
    /// `clip_b` parte da `source_in=5`: la crossing mangia sia il girato
    /// scartato dal trim di `clip_a` sia quello prima dell'inizio
    /// dichiarato di `clip_b`.
    #[test]
    fn render_ahead_keeps_both_sides_of_a_crossing_readable_through_the_whole_window() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "crossing_a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "crossing_b.mp4", 2);

        let mut project = Project::default();
        let item = |path: std::path::PathBuf| MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        };
        let media_a = project.media_pool.insert(item(path_a));
        let media_b = project.media_pool.insert(item(path_b));

        let clip_a = media_clip(1, media_a, 0, 30); // timeline [0,30)
        let clip_b = media_clip_trimmed(2, media_b, 30, 5, 30); // timeline [30,60), source_in=5

        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![clip_a, clip_b],
            muted: false,
            solo: false,
            locked: false,
            crossings: vec![vv_core::CrossTransition {
                left_clip: ClipId(1),
                right_clip: ClipId(2),
                transition: vv_core::Transition {
                    kind: vv_core::TransitionKind::Push,
                    duration: 16,
                    direction: vv_core::PushDirection::Right,
                    ease: vv_core::Ease::None,
                    curve: 0.0,
                },
            }],
        }]));
        let timeline = project.timelines.get(timeline_id).unwrap().clone();

        let mut render_ahead = RenderAhead::spawn(
            project.clone(),
            timeline_id,
            100_000_000,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );

        // Finestra della crossing (duration=16, split 8/8 attorno al
        // taglio a 30): [22,38). Copre un buon margine prima e dopo.
        let mut missing = Vec::new();
        for frame in 15..45 {
            render_ahead.set_target(frame);
            let start = std::time::Instant::now();
            let (mut ok, mut expected);
            loop {
                let clips = timeline.active_video_clips_at(frame);
                ok = 0;
                expected = 0;
                for (track_index, clip) in &clips {
                    let involved = match timeline.tracks[*track_index].crossing_at(frame) {
                        Some((left, right, _)) if left.id == clip.id || right.id == clip.id => 2,
                        _ => 1,
                    };
                    expected += involved;
                    let layers = crate::frame_provider::track_layers_at(
                        &project,
                        &timeline,
                        *track_index,
                        clip,
                        frame,
                        (320, 240),
                        &mut render_ahead,
                    )
                    .unwrap();
                    ok += layers.len();
                }
                if ok >= expected || start.elapsed() > Duration::from_secs(5) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if ok < expected {
                missing.push((frame, ok, expected));
            }
        }
        assert!(missing.is_empty(), "frame con layer mancanti (frame, ok, expected): {missing:?}");
    }

    /// Riproduzione end-to-end (thread worker reale, non `walk_and_fill`
    /// diretto) dello scenario originale segnalato dall'utente: taglia
    /// una clip, posiziona la testina *ferma* appena prima del punto di
    /// taglio (finestra di lookahead che include un pezzo di entrambe le
    /// metà, due segmenti dello stesso media). Con la testina davvero
    /// ferma per diversi cicli di poll reali (non solo due chiamate
    /// dirette a `walk_and_fill` come nel test unitario equivalente), il
    /// buffer deve convergere e restare stabile — non ricalcolarsi né
    /// restringersi a ripetizione.
    #[test]
    fn render_ahead_does_not_loop_when_the_playhead_sits_still_just_before_a_cut() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "stationary_before_cut.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        // Taglio a timeline_start=50 tra due pezzi *contigui* dello
        // stesso file (un plain split, non un trim con buco in mezzo):
        // source [0,50) e poi [50,150).
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip_trimmed(1, media_a, 0, 0, 50),
                media_clip_trimmed(2, media_a, 50, 50, 100),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
        render_ahead.set_target(40);

        // Aspetta che il buffer arrivi almeno fino al taglio.
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if ranges.iter().any(|&(s, e)| s <= 40 && e >= 50) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout: il buffer non ha mai raggiunto il taglio: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Testina ferma per una manciata di cicli di poll reali (50ms
        // l'uno): se ci fosse un loop calcola/invalida, qui l'intervallo
        // bufferizzato attorno alla testina sparirebbe e riapparirebbe
        // a ripetizione invece di restare semplicemente stabile (o
        // crescere in avanti, mai restringersi da dietro la testina).
        let mut samples = Vec::new();
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(50));
            samples.push(render_ahead.cached_ranges_for(media_a));
        }
        for (i, ranges) in samples.iter().enumerate() {
            assert!(
                ranges.iter().any(|&(s, e)| s <= 40 && e >= 50),
                "campione {i}: il buffer attorno alla testina è sparito con la testina ferma: {ranges:?}"
            );
        }
    }

    /// REFACTOR_PIPELINE.md §3.3: una `OpenDecoder` appena aperta non ha
    /// ancora osservazioni, quindi usa il fallback di default.
    #[test]
    fn open_decoder_seek_threshold_uses_the_default_fallback_before_any_observation() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_fresh.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let od = OpenDecoder::fresh(decoder, path.clone(), false);
        assert_eq!(od.seek_threshold_frames(), DEFAULT_SEEK_THRESHOLD_FRAMES);
    }

    /// Due atterraggi consecutivi da seek aggiornano la stima del GOP
    /// alla distanza osservata tra loro.
    #[test]
    fn open_decoder_records_the_observed_gap_between_two_consecutive_landings() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_observed.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), false);

        od.record_keyframe_landing(25);
        od.record_keyframe_landing(50);

        assert_eq!(od.seek_threshold_frames(), 25);
    }

    /// La stima è un *minimo*: un salto accidentale di più GOP alla
    /// volta (qui una distanza di 200 dopo una di 25) non deve far
    /// salire la soglia — solo un'osservazione più *stretta* la
    /// stringe ulteriormente, mai il contrario.
    #[test]
    fn open_decoder_gop_estimate_never_grows_from_a_wider_observation() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_min.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), false);

        od.record_keyframe_landing(0);
        od.record_keyframe_landing(25); // distanza 25: stima = 25
        assert_eq!(od.seek_threshold_frames(), 25);

        od.record_keyframe_landing(225); // distanza 200: non deve salire a 200
        assert_eq!(
            od.seek_threshold_frames(),
            25,
            "un salto più largo di uno già osservato non deve far crescere la stima"
        );

        od.record_keyframe_landing(235); // distanza 10: deve stringersi
        assert_eq!(od.seek_threshold_frames(), 10);
    }

    /// Senza il tetto (`MAX_SEEK_THRESHOLD_FRAMES`), la *prima*
    /// osservazione da sola potrebbe far esplodere la soglia se due
    /// seek capitano per caso a molti GOP di distanza prima che ne
    /// arrivi una più stretta.
    #[test]
    fn open_decoder_gop_estimate_is_capped_even_on_the_first_observation() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_cap.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), false);

        od.record_keyframe_landing(0);
        od.record_keyframe_landing(10_000);

        assert_eq!(od.seek_threshold_frames(), MAX_SEEK_THRESHOLD_FRAMES);
    }

    /// Regressione: uno scrub veloce e monotono (target sempre più
    /// avanti, mai un atterraggio ravvicinato) su un proxy non deve far
    /// restare `seek_threshold_frames` bloccata su una stima larga come
    /// per il sorgente reale (`PROXY_SEEK_THRESHOLD_FRAMES` bypassa del
    /// tutto la stima, vedi la sua doc) — è esattamente lo scenario
    /// diagnosticato con `VV_DEBUG_RENDER_AHEAD=1`: senza il bypass, ogni
    /// ciclo del worker restava bloccato 15-60ms a decodificare in
    /// sequenza invece di seekare (quasi gratis su un proxy all-intra),
    /// più del tempo tra due tick di uno scrub veloce.
    #[test]
    fn open_decoder_ignores_the_learned_gop_estimate_for_an_all_intra_proxy() {
        let path = make_test_clip("vv-app-render-ahead-test", "gop_proxy_bypass.mp4", 2);
        let decoder = Decoder::open(&path).unwrap();
        let mut od = OpenDecoder::fresh(decoder, path.clone(), true);
        assert_eq!(od.seek_threshold_frames(), PROXY_SEEK_THRESHOLD_FRAMES);

        // Atterraggi larghi e mai ravvicinati, come durante uno scrub
        // veloce e monotono: per un decoder "normale" la stima
        // convergerebbe su un valore grande (il minimo osservato finora,
        // qui 90) invece di stringersi verso il vero GOP.
        od.record_keyframe_landing(90);
        od.record_keyframe_landing(180);
        od.record_keyframe_landing(270);

        assert_eq!(
            od.seek_threshold_frames(),
            PROXY_SEEK_THRESHOLD_FRAMES,
            "un proxy all-intra non deve mai usare la soglia imparata, qualunque atterraggio osservi"
        );
    }

    /// Regressione: un seek reale per un media già aperto deve riusare
    /// il decoder esistente (`seek_to_time`), non buttarlo via per
    /// riaprire il file da zero — per un file grande/non ottimizzato per
    /// lo streaming, riaprire vuol dire riparsare l'intero indice ogni
    /// volta (anche secondi), e se quel costo eccede la tolleranza il
    /// target avanza oltre durante l'apertura stessa, scatenandone
    /// un'altra al giro successivo: un loop che non recupera mai
    /// (osservato: un frame ogni pochi secondi). Verificato dal valore
    /// di ritorno: `Seeked` (riuso) invece di `Opened` (riapertura) alla
    /// seconda chiamata sullo stesso path.
    ///
    /// Nota: qui il path è lo stesso a entrambe le chiamate di
    /// proposito — un path *diverso* per lo stesso media_id forza ora
    /// una riapertura anche a parità di posizione (vedi
    /// `position_decoder_reopens_when_the_resolved_path_changes_even_without_a_seek`,
    /// il proxy che diventa disponibile a metà sessione ha bisogno
    /// esattamente di questo).
    #[test]
    fn position_decoder_reuses_the_open_decoder_for_a_real_seek_instead_of_reopening_the_file() {
        let path = make_test_clip("vv-app-render-ahead-test", "reuse.mp4", 3);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Opened
        );

        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 1000, false, false, false),
            Positioned::Seeked,
            "un seek reale su un media già aperto deve riusare il decoder, non riaprirlo"
        );
    }

    /// REFACTOR_PIPELINE.md proxy: un proxy che diventa disponibile in
    /// background (o il toggle "usa proxy" che cambia) fa risolvere un
    /// path diverso per lo stesso media_id — il decoder aperto sul path
    /// vecchio non ha alcun senso da riusare/seekare (punta a un file
    /// diverso), va riaperto da zero anche se la posizione richiesta
    /// sarebbe altrimenti "abbastanza vicina" da non giustificare un
    /// seek.
    #[test]
    fn position_decoder_reopens_when_the_resolved_path_changes_even_without_a_seek() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "swap_a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "swap_b.mp4", 2);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path_a, 0, false, false, false),
            Positioned::Opened
        );

        // Stessa posizione richiesta (0): senza il controllo sul path
        // risolto, `needs_seek` sarebbe `false` (0 non è "troppo avanti"
        // rispetto a un decoder appena aperto) e la chiamata
        // restituirebbe `Reused` — riusando un decoder che punta al file
        // sbagliato.
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path_b, 0, false, false, false),
            Positioned::Opened,
            "il path è cambiato: deve riaprire sul nuovo, non riusare il decoder del vecchio"
        );
    }

    /// Regressione per un bug reale, confermato dall'utente: `next_frame`
    /// è solo quel che il decoder *crede* di aver già prodotto, non una
    /// garanzia che sia ancora in cache — uno sfratto tra un ciclo e
    /// l'altro (`reconcile`, o un budget stretto durante un fill
    /// precedente) può aver rimosso la coda che il decoder pensa di
    /// avere già dietro di sé. Qui si simula esattamente questo: un
    /// decoder "avanzato" con `next_frame` oltre il target, ma con il
    /// contenuto che next_frame presume di avere copiato rimosso a mano
    /// dalla cache (come farebbe un `reconcile` reale) — `position_decoder`
    /// deve accorgersene e forzare un seek reale, non fidarsi di
    /// `next_frame` e restituire `Reused` su un buco.
    #[test]
    fn position_decoder_reseeks_when_next_frame_claims_coverage_the_cache_no_longer_has() {
        let path = make_test_clip("vv-app-render-ahead-test", "stale_next_frame.mp4", 3);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Opened
        );
        // Decodifica e mette in cache qualche frame, come farebbe
        // walk_and_fill — il decoder ora "crede" di essere avanti con
        // tutto quel tratto genuinamente dietro di sé in cache.
        for _ in 0..20 {
            let od = open.get_mut(&media_a).unwrap();
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    caches.insert(media_a, idx, Arc::new(frame));
                    od.next_frame = idx + 1;
                }
                _ => break,
            }
        }
        let advanced_next_frame = open.get(&media_a).unwrap().next_frame;
        assert!(
            advanced_next_frame > 5,
            "il decoder deve aver avanzato di parecchio"
        );

        // Simula uno sfratto reale: `reconcile` con una finestra che
        // esclude deliberatamente un solo frame nel mezzo di quel che
        // `next_frame` presume coperto — la cache perde quel frame senza
        // che il decoder ne sappia nulla (esattamente quel che farebbe
        // un budget stretto o una finestra che si restringe).
        let gap_at = advanced_next_frame - 3;
        let window = [
            WantedRange {
                media_id: media_a,
                source_start: 0,
                source_end: gap_at - 1,
                timeline_start: 0,
                rate: vv_core::Rational::one(),
            },
            WantedRange {
                media_id: media_a,
                source_start: gap_at + 1,
                source_end: advanced_next_frame - 1,
                timeline_start: gap_at + 1,
                rate: vv_core::Rational::one(),
            },
        ];
        caches.reconcile(0, &window, usize::MAX);

        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Seeked,
            "un buco lasciato da uno sfratto dietro a next_frame deve forzare un seek reale, \
             non un Reused che lo lascia scoperto per sempre"
        );
    }

    /// Regressione per il bug segnalato: durante il playback normale il
    /// decoder è quasi sempre *avanti* rispetto al target (è lo stato
    /// sano di un buffer che lavora bene). Prima del fix,
    /// `position_decoder` interpretava questo come "troppo indietro" e
    /// riapriva il file con un seek reale a ogni ciclo di poll,
    /// invalidando il lavoro appena fatto — da cui l'indicatore che
    /// "gira in tondo" senza mai avanzare stabilmente.
    #[test]
    fn position_decoder_does_not_reseek_when_already_usefully_ahead_of_the_segment_start() {
        let path = make_test_clip("vv-app-render-ahead-test", "steady.mp4", 3);
        let (media_a, _) = two_media_ids();

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Opened
        );

        // Decodifica qualche frame in avanti "a mano" *e* li inserisce in
        // cache, come farebbe walk_and_fill, per simulare un decoder già
        // bufferizzato oltre il target attuale — dalla riverifica di
        // copertura in `position_decoder` (vedi la sua doc), un
        // `next_frame` avanzato senza contenuto in cache dietro di sé
        // non basterebbe più a evitare un seek.
        for _ in 0..20 {
            let od = open.get_mut(&media_a).unwrap();
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    caches.insert(media_a, idx, Arc::new(frame));
                    od.next_frame = idx + 1;
                }
                _ => break,
            }
        }
        let advanced_next_frame = open.get(&media_a).unwrap().next_frame;
        assert!(advanced_next_frame > 0, "il decoder deve aver avanzato");

        // Un ciclo successivo con il target ancora dietro alla posizione
        // del decoder — lo stato normale durante il playback in avanti —
        // non deve riaprire/riazzerare il decoder.
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 0, false, false, false),
            Positioned::Reused
        );
        assert_eq!(
            open.get(&media_a).unwrap().next_frame,
            advanced_next_frame,
            "non deve aver riaperto il decoder mentre è ancora utilmente avanti"
        );
    }

    /// `WalkOutcome::caught_up` è la base di `RenderAhead::is_caught_up`,
    /// che la UI usa per decidere se vale la pena richiedere un altro
    /// repaint (vedi doc lì): con un budget ampio a sufficienza per
    /// l'intera finestra di lookahead, un ciclo deve bastare a coprirla
    /// tutta e segnalarlo.
    #[test]
    fn walk_and_fill_reports_caught_up_when_the_whole_window_fits_the_budget() {
        let path = make_test_clip("vv-app-render-ahead-test", "caught_up.mp4", 3);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 75,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 75)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 4 * 200; // ben oltre i 75 frame della finestra
        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            generous_budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );

        assert!(
            outcome.caught_up,
            "budget e finestra coprono tutta la clip: non dovrebbe restare altro da fare"
        );
        assert!(!outcome.interrupted);
    }

    /// La finestra di retention dietro la testina (`behind_secs`) non è
    /// solo "non scartare quel che c'è già": su una zona *mai visitata
    /// prima* deve venire davvero decodificata, non solo trattenuta se
    /// già presente — altrimenti uno scrub in una zona nuova poco dopo
    /// l'inizio della clip non avrebbe nulla da retention dietro di sé.
    #[test]
    fn walk_and_fill_decodes_the_behind_window_on_a_fresh_area() {
        let path = make_test_clip("vv-app-render-ahead-test", "fresh_behind.mp4", 4);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget generoso: niente sfratto per capacità a confondere il
        // risultato, qui interessa solo "viene decodificato" o no.
        let generous_budget = 320 * 240 * 3 / 2 * 200;

        // Prima volta che questa zona viene vista: playhead a 60, mai
        // stato altrove prima (`went_backward` irrilevante al primo
        // ciclo).
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            60,
            generous_budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(60),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 40 && e >= 59),
            "il tratto dietro la testina [40,59] (dentro behind_secs) deve essere stato decodificato, non solo trattenuto se già presente: {ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(s, e)| s <= 60 && e >= 99),
            "la finestra in avanti deve comunque essere coperta normalmente: {ranges:?}"
        );
    }

    /// Regressione segnalata dall'utente: durante uno scrub all'indietro
    /// il frame utile *subito* è quello adiacente alla testina (il bordo
    /// vicino della finestra dietro), non quello sul bordo lontano — ma
    /// decodificare un intero segmento dietro in un solo seek (come per
    /// la finestra in avanti) produce i frame nell'ordine sbagliato:
    /// ffmpeg decodifica solo in avanti da `source_start` (lontano) verso
    /// `source_end` (vicino), quindi il frame più utile arriva per
    /// ultimo. Con un budget che copre la finestra in avanti (piccola,
    /// vicino alla fine della clip) più solo il primo blocco della
    /// finestra dietro (`BEHIND_CHUNK_FRAMES`), il frame adiacente alla
    /// testina deve comunque essere in cache, quello sul bordo lontano
    /// no — prova diretta che `chunk_behind_segments_near_to_far`
    /// riordina davvero la priorità di decodifica, non solo sulla carta.
    #[test]
    fn walk_and_fill_decodes_the_behind_window_nearest_frames_first_under_a_tight_budget() {
        // GOP=1 (ogni frame un keyframe, come un proxy): un seek atterra
        // esattamente dove richiesto, così il budget necessario per ogni
        // blocco è prevedibile in frame esatti — con un GOP lungo (video
        // "normale") il seek atterrerebbe al keyframe più vicino
        // *prima* del bersaglio, rendendo il conto qui sotto fragile
        // senza aggiungere nulla alla cosa sotto test (l'ordine di
        // priorità dei blocchi, non quanto costi arrivarci).
        let path =
            make_test_clip_with_short_gop("vv-app-render-ahead-test", "behind_priority.mp4", 4, 1);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Testina a 95: finestra in avanti minuscola (solo [95,99], la
        // clip finisce a 100), finestra dietro normale (2s = 50 frame,
        // [45,94]). Budget per la finestra in avanti (5 frame) più *solo*
        // il primo blocco della finestra dietro (`BEHIND_CHUNK_FRAMES`
        // = 15 frame, [80,94]) — non abbastanza per raggiungere il bordo
        // lontano a 45.
        let frame_bytes = 320 * 240 * 3 / 2;
        let tight_budget = frame_bytes * (5 + 15);
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            95,
            tight_budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(95),
        );

        assert!(
            caches.contains(media_a, 94),
            "il frame adiacente alla testina (bordo vicino della finestra dietro) deve essere \
             tra i primi decodificati, quindi in cache anche con un budget stretto"
        );
        assert!(
            !caches.contains(media_a, 45),
            "il frame sul bordo lontano della finestra dietro non deve essere raggiunto prima \
             di quelli vicini alla testina, con un budget che copre solo il primo blocco"
        );
    }

    /// Regressione: a testina *ferma* (nessun cambio tra due cicli), un
    /// secondo `walk_and_fill` sulla finestra dietro già completamente
    /// riempita non deve toccare il decoder — vedi doc di
    /// `without_already_cached_chunks`. Senza il filtro, l'ordine dal
    /// blocco più vicino al più lontano fa sì che il decoder, a inizio
    /// ciclo, sia sempre posizionato *dietro* al primo blocco richiesto
    /// (si era fermato dove finiva il blocco più lontano del ciclo
    /// prima), quindi ogni blocco verrebbe riseekato e almeno un frame
    /// ributtato via — a ogni singolo ciclo, per sempre, anche senza
    /// alcuno scrub in corso. Un seek reale coinvolge il processo
    /// `ffmpeg`/il container: anche uno solo costa ordini di grandezza
    /// più di un giro di controlli `cached_ranges` in memoria, quindi un
    /// tetto di tempo stretto sul secondo giro distingue in modo
    /// affidabile "non ha toccato il decoder" da "ha rifatto lavoro".
    #[test]
    fn walk_and_fill_does_not_reseek_an_already_complete_behind_window_when_idle() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "behind_idle_stability.mp4",
            4,
            1,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 3 / 2 * 200;

        // Primo giro: riempie per intero sia avanti che dietro (più
        // blocchi, [45,94] a 25fps/2s).
        let first = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            95,
            generous_budget,
            false,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(95),
        );
        assert!(
            first.caught_up,
            "il primo giro deve completare la finestra: {first:?}"
        );

        // 20 giri successivi, stessa testina, nulla cambia: devono
        // completarsi quasi istantaneamente (nessun seek reale, solo
        // controlli `cached_ranges` in memoria). Un singolo giro non è
        // una misura abbastanza stabile (jitter di scheduling del SO
        // dell'ordine di qualche ms può capitare anche senza alcun
        // lavoro reale) — sommare 20 giri indipendenti amplifica il
        // segnale: se anche uno solo tocca il decoder, il costo di un
        // vero seek+decodifica (1-2ms, già misurato altrove in questo
        // file) domina il totale, mentre 20 giri di soli controlli in
        // memoria restano nell'ordine del centinaio di µs.
        let start = std::time::Instant::now();
        for _ in 0..20 {
            let outcome = walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                95,
                generous_budget,
                false,
                false,
                DEFAULT_LOOKAHEAD_SECS,
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(95),
            );
            assert!(outcome.caught_up);
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(20),
            "20 giri a testina ferma non devono toccare il decoder (nessun seek reale): \
             impiegati {elapsed:?} in totale, attesi <20ms"
        );
    }

    /// `lookahead_secs`/`behind_secs` configurati a `0`: la finestra si
    /// riduce al margine minimo (`MIN_MARGIN_FRAMES`), non a zero — anche
    /// con budget generoso e una zona mai vista prima (che con una
    /// finestra normale farebbe scattare sia la finestra in avanti sia
    /// quella di retention, vedi il test sopra).
    #[test]
    fn walk_and_fill_buffers_only_a_minimal_margin_when_configured_to_zero_seconds() {
        let path = make_test_clip("vv-app-render-ahead-test", "no_read_ahead.mp4", 4);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let generous_budget = 320 * 240 * 3 / 2 * 200;

        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            60,
            generous_budget,
            false,
            false, // proxy_enabled: irrilevante per questo test
            0.0,   // lookahead_secs: quello sotto esame
            0.0,   // behind_secs: quello sotto esame
            &AtomicI64::new(60),
        );

        // [56,63], non un intervallo più ampio: con keyint di default
        // (250) e una clip di soli 100 frame, l'unico keyframe è a 0 —
        // raggiungere il frame 63 (= 60 + MIN_MARGIN_FRAMES) richiede di
        // decodificare in sequenza da lì, e quei frame *di transito*
        // restano in cache come sottoprodotto DURANTE il giro (vedi doc
        // di `transit_bytes` in `fill_segments`: serve a far sì che il
        // blocco dietro [56,59] li trovi già pronti invece di
        // riattraversare da capo lo stesso GOP), ma un `reconcile` finale
        // li scarta di nuovo prima che `walk_and_fill` dichiari
        // `caught_up` (vedi il commento lì sul perché: altrimenti la UI,
        // che smette di richiedere repaint su `caught_up`, può restare
        // bloccata a mostrare quel transito come se fosse "buffered" fino
        // al prossimo repaint per altri motivi — bug segnalato
        // dall'utente, la striscia "si ridimensiona" solo muovendo il
        // mouse). Il riuso *tra* i due segmenti di questo stesso giro è
        // comunque già avvenuto prima di questo `reconcile` finale, solo
        // la sua sopravvivenza oltre la fine del giro si perde.
        assert_eq!(
            caches.cached_ranges(media_a),
            vec![(60 - MIN_MARGIN_FRAMES, 60 + MIN_MARGIN_FRAMES - 1)],
            "configurato a zero secondi la finestra non deve estendersi oltre il margine minimo"
        );
        assert!(
            outcome.caught_up,
            "una finestra minima, già coperta, deve risultare caught_up"
        );
    }

    /// Regressione: se il budget non basta a coprire tutta la finestra
    /// di lookahead, il buffer deve comunque partire dalla testina (i
    /// frame più vicini, i più utili da mostrare subito) e non da una
    /// coda arbitraria della finestra — altrimenti l'indicatore mostra
    /// un intervallo che "cade dopo" la testina senza mai coprirla.
    #[test]
    fn walk_and_fill_prioritizes_frames_near_the_playhead_when_the_budget_is_too_small_for_the_full_window()
     {
        let path = make_test_clip("vv-app-render-ahead-test", "small_budget.mp4", 3);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 75,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 75)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget minuscolo: la finestra di lookahead (3s = 75 frame a
        // 25fps) non ci sta tutta nella cache.
        let tiny_budget = 320 * 240 * 4 * 5;
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            tiny_budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(!ranges.is_empty());
        assert_eq!(
            ranges[0].0, 0,
            "il buffer deve partire dalla testina, non da una coda arbitraria: {ranges:?}"
        );
        assert!(
            ranges[0].1 < 74,
            "con un budget così piccolo non deve riuscire a coprire tutta la finestra: {ranges:?}"
        );
    }

    /// REFACTOR_PIPELINE.md §3.1 (reattività): se il target *live* si è
    /// già spostato oltre la soglia di fallback (nessuna osservazione
    /// del GOP ancora fatta per questo media) rispetto a `from_frame`
    /// prima ancora di iniziare, il fill deve accorgersene alla prima
    /// occasione (dopo il primo frame decodificato) e interrompersi
    /// restituendo `true`, invece di continuare a decodificare per tutta
    /// la finestra un prefetch ormai obsoleto.
    #[test]
    fn walk_and_fill_stops_early_and_reports_true_when_the_live_target_has_already_drifted() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "reactivity_drift.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Il target live è già oltre soglia rispetto a from_frame=0 prima
        // ancora che il fill inizi: simula la testina che è saltata
        // altrove mentre questo ciclo stava per partire.
        let drifted_target = AtomicI64::new(DEFAULT_SEEK_THRESHOLD_FRAMES + 200);

        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            100_000_000,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &drifted_target,
        );
        assert!(
            outcome.interrupted,
            "deve segnalare l'interruzione al chiamante (worker_loop) per farlo ripartire subito"
        );
        assert!(
            !outcome.caught_up,
            "interrotto: non ha potuto verificare se la finestra fosse coperta"
        );

        let ranges = caches.cached_ranges(media_a);
        let decoded_frames: FrameIdx = ranges.iter().map(|&(s, e)| e - s + 1).sum();
        assert!(
            decoded_frames < 10,
            "deve fermarsi dopo pochissimi frame, non decodificare l'intera finestra ormai obsoleta: ranges={ranges:?}"
        );
    }

    /// Regressione per il bug segnalato dall'utente: due clip sulla
    /// timeline che condividono lo stesso media (un unico file tagliato
    /// in più pezzi, comunissimo) generano due `MediaSegment` per lo
    /// stesso `media_id` nella stessa finestra, con `source_start`
    /// diversi. Chiamare `evict_before` con il `source_start` del
    /// *singolo* segmento in elaborazione (come si faceva prima)
    /// scartava, elaborando il secondo segmento, tutto ciò che il primo
    /// aveva appena decodificato — "il buffer si ricalcola da capo
    /// invalidando i frame successivi" segnalato dall'utente,
    /// riproducibile ad ogni taglio tra due pezzi dello stesso file.
    #[test]
    fn walk_and_fill_does_not_invalidate_one_segment_while_processing_another_segment_of_the_same_media()
     {
        let path =
            make_test_clip_with_short_gop("vv-app-render-ahead-test", "same_media_cut.mp4", 20, 25);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        // Taglio a timeline_start=60 tra due pezzi dello stesso file:
        // il primo usa il sorgente [0,60), il secondo riparte da un
        // punto molto più avanti nel sorgente [200,300) — esattamente
        // come tagliare via una parte centrale dello stesso file.
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip_trimmed(1, media_a, 0, 0, 60),
                media_clip_trimmed(2, media_a, 60, 200, 100),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 100_000_000;

        // La finestra di lookahead (3s = 75 frame a 25fps) da 40
        // attraversa il taglio a 60, includendo un pezzo di entrambe le
        // clip nello stesso ciclo.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            40,
            budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(40),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 40 && e >= 59),
            "il tratto della prima clip [40,59] non deve essere sfrattato dall'elaborazione della seconda: ranges={ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(s, e)| s <= 200 && e >= 200),
            "la seconda clip deve comunque essere bufferizzata, non solo attraversata: ranges={ranges:?}"
        );
    }

    /// Regressione per il bug segnalato dall'utente ("il caching non deve
    /// essere per clip ma per timeline"): il test precedente usa un
    /// budget enorme (100MB) che non mette mai sotto pressione la
    /// capacità reale della cache, quindi non lo scopre. Con un budget
    /// stretto, due segmenti dello stesso media in questa finestra (un
    /// taglio con in mezzo una parte scartata: sorgenti lontani tra
    /// loro) chiedevano *insieme* più frame di quanti la `FrameCache`
    /// condivisa potesse contenerne — il secondo segmento elaborato
    /// (`[200,254]`) sfrattava per limite di capacità (LRU ordinaria,
    /// non `evict_before`) tutto ciò che il primo (`[40,59]`) aveva
    /// appena decodificato nello stesso identico ciclo, anche se
    /// `evict_before` da solo l'avrebbe protetto. Visto dall'utente:
    /// ogni clip sembra bufferizzare "per conto suo", a spese delle
    /// altre — da cui "il caching sembra per-clip, non per-timeline".
    #[test]
    fn walk_and_fill_does_not_let_one_segment_of_a_media_evict_another_via_capacity_when_the_budget_is_tight()
     {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "same_media_cut_tight_budget.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        // Stesso taglio del test precedente: [0,60) poi [200,300).
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip_trimmed(1, media_a, 0, 0, 60),
                media_clip_trimmed(2, media_a, 60, 200, 100),
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Capacità ~60 frame YUV420 (width*height*3/2 byte/frame):
        // meno di quanto i due segmenti insieme chiederebbero (~20 + ~55),
        // ma più di quanto ciascuno chiede da solo — costringe la
        // condivisione della stessa cache a contare davvero.
        let budget = 60 * 320 * 240 * 3 / 2;

        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            40,
            budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(40),
        );

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 40 && e >= 59),
            "il tratto della prima clip [40,59] non deve sparire per colpa del secondo segmento nello stesso ciclo: ranges={ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(s, _)| s <= 200),
            "la seconda clip deve comunque ricevere una fetta della capacità condivisa: ranges={ranges:?}"
        );
        assert!(
            !outcome.caught_up,
            "budget saturo prima di finire la finestra: non è \"caught up\", c'è ancora lavoro per il prossimo ciclo"
        );
    }

    /// Regressione per il bug segnalato dall'utente: con la testina
    /// ferma subito prima di un taglio (basta tagliare una clip e
    /// posizionare la testina appena prima del punto di taglio: la
    /// finestra di lookahead include comunque un pezzo di entrambe le
    /// metà, due segmenti dello stesso media), ogni ciclo di poll
    /// rielabora gli stessi due segmenti nello stesso ordine: il secondo
    /// segmento non deve far sembrare "tornato indietro" il primo e
    /// scatenare un seek reale a ogni ciclo pur restando fermi (vedi doc
    /// di `position_decoder` su `went_backward`). Verificato passando
    /// `false` (testina ferma) a entrambi i segmenti in entrambi i cicli.
    ///
    /// Gap tra i due segmenti (10 e 25) scelto apposta sotto
    /// `DEFAULT_SEEK_THRESHOLD_FRAMES`: qui `position_decoder` viene
    /// chiamato direttamente, senza mai decodificare un frame reale, quindi
    /// nessuna osservazione del GOP avviene mai e la soglia resta al
    /// fallback per tutto il test — un gap più ampio farebbe scattare
    /// legittimamente il ramo "troppo avanti", mascherando la cosa che
    /// questo test vuole isolare (la contaminazione tra segmenti dello
    /// stesso media, non quella soglia).
    #[test]
    fn position_decoder_does_not_reseek_across_cycles_when_the_same_media_appears_in_two_segments()
    {
        let path = make_test_clip("vv-app-render-ahead-test", "same_media_two_segments.mp4", 3);
        let (media_a, _) = two_media_ids();
        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();

        // Ciclo 1: due segmenti dello stesso media nella stessa finestra
        // (come ai due lati di un taglio), source_start 10 e poi 25.
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 10, false, false, false),
            Positioned::Opened
        );
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 25, false, false, false),
            Positioned::Reused,
            "nello stesso ciclo il secondo segmento non deve mai richiedere un seek: il decoder è già lì"
        );

        // Ciclo 2, testina ferma (`went_backward=false` per entrambi):
        // stessi due segmenti. Rielaborare il *primo* segmento (10) non
        // deve sembrare "tornato indietro" solo perché l'ultima chiamata
        // vista nel ciclo precedente era per il segmento successivo (25).
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 10, false, false, false),
            Positioned::Reused,
            "testina ferma: rielaborare il primo segmento non deve scatenare un seek reale"
        );
        assert_eq!(
            position_decoder(&caches, &mut open, media_a, &path, 25, false, false, false),
            Positioned::Reused
        );
    }

    /// Regressione (ex-A2, REFACTOR_PIPELINE.md): con la vecchia
    /// architettura (una `FrameCache` per media, capacità fissata al
    /// momento della creazione) questo test verificava che la capacità
    /// di `media_b` si allargasse quando `media_a` usciva dalla finestra
    /// — un problema che con la `SharedFrameCache` a budget globale (§2)
    /// non può più presentarsi *per costruzione*: non esiste più una
    /// capacità per-media da tenere sincronizzata, il budget è uno solo
    /// e sempre quello reale. Verifica quindi l'equivalente diretto: con
    /// meno media a contendersi il budget, `media_b` arriva a
    /// bufferizzare *più* frame (non più capacità, ma copertura reale).
    #[test]
    fn walk_and_fill_buffers_more_of_a_media_once_fewer_distinct_media_share_the_budget() {
        let path_a = make_test_clip("vv-app-render-ahead-test", "resize_a.mp4", 2);
        let path_b = make_test_clip("vv-app-render-ahead-test", "resize_b.mp4", 15);

        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path: path_a,
            meta: MediaMeta {
                duration_frames: 40,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let media_b = project.media_pool.insert(MediaItem {
            path: path_b,
            meta: MediaMeta {
                duration_frames: 375,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![
                media_clip(1, media_a, 0, 40),   // [0,40)
                media_clip(2, media_b, 40, 400), // [40,440)
            ],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget che a 320x240 (307_200 B/frame) basta per ~60 frame
        // totali: con due media a contendersi la finestra ce ne stanno
        // pochi a testa, con uno solo molti di più.
        let total_budget = 60 * 320 * 240 * 4;

        let frames_cached = |ranges: &[(FrameIdx, FrameIdx)]| -> FrameIdx {
            ranges.iter().map(|&(s, e)| e - s + 1).sum()
        };

        // Primo ciclo: la finestra di lookahead (3s = 75 frame) attraversa
        // il taglio a 40, quindi media_a e media_b sono entrambi nella
        // finestra e condividono lo stesso budget globale.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            0,
            total_budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(0),
        );
        let frames_b_shared = frames_cached(&caches.cached_ranges(media_b));

        // Secondo ciclo: il target è ben oltre il taglio, solo media_b è
        // nella finestra — reconcile scarta media_a (Tier A), quindi
        // media_b ha l'intero budget globale per sé, senza bisogno di
        // nessuna capacità da "ridimensionare verso l'alto" a parte.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            200,
            total_budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(200),
        );
        let ranges = caches.cached_ranges(media_b);
        let frames_b_alone = frames_cached(&ranges);
        assert!(
            frames_b_alone > frames_b_shared,
            "con un solo media nella finestra deve arrivare a bufferizzarne di più, non restare fermo alla quota di quando la condivideva: {frames_b_shared} -> {frames_b_alone}"
        );

        // source_start per il secondo ciclo: clip b ha source_in=0,
        // timeline_start=40, quindi 200-40=160.
        assert!(
            ranges.iter().any(|&(s, _)| s <= 160),
            "con più budget disponibile il buffer deve poter partire dalla nuova testina, non da una coda arbitraria più avanti: {ranges:?}"
        );
    }

    /// Regressione: dopo uno scrub molto indietro rispetto a dove il
    /// worker aveva già bufferizzato in avanti, il buffer deve
    /// raggiungere anche la nuova posizione — il decoder può solo
    /// decodificare in avanti, quindi senza un riapertura esplicita
    /// resterebbe bloccato oltre il nuovo target per sempre.
    #[test]
    fn render_ahead_catches_up_after_a_large_backward_seek() {
        let path = make_test_clip("vv-app-render-ahead-test", "backward_seek.mp4", 4);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
        render_ahead.set_target(80);

        let start = std::time::Instant::now();
        loop {
            if !render_ahead.cached_ranges_for(media_a).is_empty() {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout in avanti"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Scrub indietro oltre la soglia di seek: il decoder che ha
        // bufferizzato attorno a 80 non può proseguire in avanti per
        // raggiungere 0.
        render_ahead.set_target(0);
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if ranges.iter().any(|&(s, _)| s == 0) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout indietro: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Regressione per il bug segnalato dall'utente: uno scrub
    /// all'indietro deve rigenerare il buffer per la nuova posizione,
    /// non restare bloccato sul frame in cache più vicino. Lo scrub qui
    /// (70 frame) è scelto apposta *oltre* la finestra di retention
    /// dietro la testina (`DEFAULT_BEHIND_SECS`, 2s = 50 frame a 25fps): un
    /// vero scrub oltre quella finestra deve ancora comportarsi come
    /// prima di quella finestra — rigenerare da zero — perché qui non
    /// c'è nulla da riusare. Uno scrub *dentro* la finestra invece non
    /// deve rigenerare nulla per costruzione (vedi
    /// `walk_and_fill_does_not_redecode_the_already_buffered_tail_after_a_small_backward_seek`,
    /// che verifica esattamente quello). Prima del fix originale di
    /// questa regressione, restava bloccato sul frame in cache più
    /// vicino perché `position_decoder` considerava "abbastanza avanti"
    /// qualunque target ancora dietro a `next_frame` più di una soglia —
    /// ma `walk_and_fill` scarta ad ogni ciclo tutto ciò che è fuori
    /// dalla finestra corrente, quindi anche un piccolo passo indietro
    /// oltre la finestra di retention cade in territorio già scartato e
    /// irraggiungibile decodificando solo in avanti.
    #[test]
    fn render_ahead_catches_up_after_a_backward_seek_beyond_the_retention_window() {
        let path = make_test_clip("vv-app-render-ahead-test", "small_backward_seek.mp4", 6);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 150,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 150)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(
            project,
            timeline_id,
            100_000_000,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
        );
        render_ahead.set_target(100);

        // Attendi non solo che il buffer copra 100, ma che i cicli di
        // poll successivi abbiano anche già scartato ciò che è rimasto
        // fuori dalla finestra (avanti+dietro) di 100 — compreso 30, che
        // è 70 frame dietro, oltre i 50 della finestra di retention.
        // Altrimenti il test passerebbe per caso, perché il primo
        // riempimento (che decodifica dal keyframe più vicino, qui
        // l'inizio del file) include già 30 prima ancora che venga
        // scartato.
        let covers = |ranges: &[(FrameIdx, FrameIdx)], f: FrameIdx| {
            ranges.iter().any(|&(s, e)| s <= f && f <= e)
        };
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if covers(&ranges, 100) && !covers(&ranges, 30) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout in avanti: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Scrub indietro di 70 frame (oltre la finestra di retention di
        // 50): deve comunque rigenerare il buffer per la nuova
        // posizione, non restare bloccato sul frame più vicino già in
        // cache. Copertura di 30 (non "un range che parte esattamente
        // lì"): il seek atterra sul keyframe più vicino a 30, che può
        // essere anche prima di 30 stesso.
        render_ahead.set_target(30);
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if covers(&ranges, 30) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout su scrub indietro: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Regressione per il bug segnalato dall'utente: playback a scatti
    /// anche a 1x con `lookahead_secs`/`behind_secs` a `0`, perché
    /// `set_target` aggiornava solo un atomico letto dal worker al più ogni
    /// `POLL_INTERVAL` (50ms) — un tetto reale a ~20 frame/sec a
    /// prescindere da quanto la decodifica fosse veloce (succedeva
    /// anche coi proxy).
    ///
    /// Verifica la sola sveglia immediata (`Command::Wake`), isolata dal
    /// margine minimo: un salto isolato per volta, con una scadenza
    /// di 40ms *dopo* aver aspettato che il worker si stabilizzi sul
    /// target precedente. Senza sveglia immediata l'attesa sarebbe
    /// uniforme tra 0 e 50ms: ognuno degli 11 salti resta sotto i 40ms per
    /// caso all'80%, tutti insieme ~9%. La scadenza non è più stretta
    /// perché sotto il carico degli altri test in parallelo anche la
    /// decodifica del frame può sforare.
    #[test]
    fn render_ahead_reacts_to_each_target_change_faster_than_the_old_poll_interval() {
        let path = make_test_clip("vv-app-render-ahead-test", "wake_on_change.mp4", 2);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 50,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 50)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let render_ahead = RenderAhead::spawn(project, timeline_id, 100_000_000, false, 0.0, 0.0);

        let wait_for = |frame: FrameIdx| {
            let start = std::time::Instant::now();
            loop {
                if render_ahead.get_frame(media_a, frame).is_some() {
                    return start.elapsed();
                }
                assert!(
                    start.elapsed() < Duration::from_millis(40),
                    "frame {frame} non pronto entro una scadenza compatibile con la sveglia immediata"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };

        // Primo target: nessuna scadenza stretta, il worker deve solo
        // avviarsi (apertura del decoder inclusa).
        render_ahead.set_target(0);
        loop {
            if render_ahead.get_frame(media_a, 0).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // Da qui in poi, ogni salto isolato ha 40ms per essere pronto.
        for target in (8..=48).step_by(4) {
            render_ahead.set_target(target);
            wait_for(target);
        }
    }

    /// Regressione generale: dopo un po' di playback in avanti su più
    /// cicli, uno scrub all'indietro verso una posizione più recente del
    /// primissimo target mai visto deve comunque far ricalcolare il
    /// buffer per la nuova posizione. Verifica che `went_backward`,
    /// calcolato una volta per ciclo (qui simulato come farebbe
    /// `worker_loop`), regga una sequenza realistica di cicli, non solo un
    /// singolo salto indietro isolato.
    #[test]
    fn walk_and_fill_catches_up_after_a_backward_seek_above_the_historical_minimum() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "backward_above_historical_min.mp4",
            20,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget stretto: la finestra intera non ci sta in cache, quindi
        // avanzando `evict_before` scarta davvero i frame dietro la
        // testina invece di lasciarli semplicemente ancora presenti per
        // caso.
        let budget = 43_000_000;

        // Playback in avanti su più cicli: `went_backward` è sempre
        // `false` (ogni target è >= al precedente), esattamente come lo
        // calcolerebbe `worker_loop` confrontando `from` col ciclo
        // prima.
        let mut prev = None;
        for from in [0, 50, 100, 150, 200] {
            let went_backward = prev.is_some_and(|p| from < p);
            prev = Some(from);
            walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                from,
                budget,
                went_backward,
                false,                  // proxy_enabled: irrilevante per questo test
                DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(from),
            );
        }

        // A questo punto i frame intorno a 0 sono sicuramente sfrattati
        // (evict_before ha scartato tutto ciò che è dietro alla testina
        // ad ogni ciclo, l'ultimo dei quali è 200).
        let ranges_before = caches.cached_ranges(media_a);
        assert!(
            !ranges_before.iter().any(|&(s, e)| s <= 80 && e >= 80),
            "80 non deve essere già in cache per coincidenza, altrimenti il test non prova nulla: {ranges_before:?}"
        );

        // Scrub indietro a 80: più indietro della testina attuale (200),
        // ma più avanti del target più vecchio mai visto (0).
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            80,
            budget,
            true,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(80),
        );

        let ranges_after = caches.cached_ranges(media_a);
        assert!(
            ranges_after.iter().any(|&(s, e)| s <= 80 && e >= 80),
            "lo scrub indietro a 80 deve far ricalcolare il buffer per la nuova posizione: {ranges_after:?}"
        );
    }

    /// Verifica l'ottimizzazione richiesta: dopo un piccolo scrub
    /// all'indietro, la porzione già bufferizzata che ricade ancora
    /// nella *nuova* finestra (avanti + dietro, vedi `behind_secs`) non
    /// deve essere ridecodificata — anzi, il decoder non va toccato per
    /// niente (`SharedFrameCache::covers`, controllato *prima* di interpellarlo:
    /// vedi la sua doc su un bug reale, confermato in produzione, causato
    /// dal fidarsi della posizione che il decoder *crede* di avere invece
    /// di chiedere alla cache). Con la finestra di retention dietro la
    /// testina, lo scrub di 30 frame qui sotto ricade interamente
    /// *dentro* quella finestra (50 frame): il tratto [250,269] non viene
    /// nemmeno scartato da `reconcile`, quindi l'intero segmento richiesto
    /// risulta già coperto e viene saltato di netto — `OpenDecoder::
    /// next_frame` deve restare esattamente dov'era prima di questa
    /// chiamata, prova diretta che nessun seek/decodifica è avvenuto.
    ///
    /// Nota (REFACTOR_PIPELINE.md §2, Tier A): con la `SharedFrameCache`
    /// a budget globale, `reconcile` scarta anche ciò che è *oltre*
    /// l'orizzonte della nuova finestra (qui: oltre 344, dato che la
    /// nuova testina è 270) — a differenza della vecchia `evict_before`,
    /// che scartava solo ciò che era dietro e lasciava intatto tutto ciò
    /// che era avanti, qualunque fosse l'orizzonte. È voluto: il budget
    /// della finestra è sempre esattamente quello della finestra
    /// corrente, non un accumulo indefinito di code storiche. Quindi qui
    /// si verifica solo che [250,344] (l'intersezione tra vecchia coda e
    /// nuova finestra allargata dalla retention) sia raggiungibile senza
    /// ridecodificarla — non che tutta la vecchia coda fino a 374
    /// sopravviva.
    #[test]
    fn walk_and_fill_does_not_redecode_the_already_buffered_tail_after_a_small_backward_seek() {
        let path = make_test_clip("vv-app-render-ahead-test", "reconnect.mp4", 20);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 43_000_000; // capacità ~139 frame

        // Bufferizza attorno a 300: con keyint=250 (default libx264) il
        // decoder riparte dal keyframe 250 e riempie fino al limite di
        // capacità.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            300,
            budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(300),
        );
        let filled_up_to = open.get(&media_a).unwrap().next_frame - 1;
        assert!(
            filled_up_to > 350,
            "il primo riempimento deve aver bufferizzato ben oltre 300 (fino all'orizzonte di lookahead): {filled_up_to}"
        );

        // Scrub indietro di soli 30 frame: sotto la vecchia soglia di
        // 120, ma comunque un vero spostamento all'indietro (deve
        // riaprire/riseekare, `went_backward=true`).
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            270,
            budget,
            true,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(270),
        );

        let next_frame_after = open.get(&media_a).unwrap().next_frame;
        assert_eq!(
            next_frame_after,
            filled_up_to + 1,
            "il segmento richiesto era già interamente in cache: il decoder non doveva essere \
             toccato affatto, next_frame deve restare dov'era: next_frame={next_frame_after}"
        );

        // L'intersezione tra la vecchia coda e la nuova finestra
        // allargata dalla retention ([250,344]) deve essere raggiungibile
        // come un range contiguo, senza buchi dovuti a una ridecodifica
        // sprecata. Il fatto che `filled_up_to` (374) sia più avanti
        // dell'orizzonte della nuova finestra è atteso: quella parte è
        // stata scartata dal Tier A di `reconcile` perché non più nella
        // finestra corrente (vedi nota sopra), non perché la
        // riconnessione abbia fallito.
        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 250 && e >= 344),
            "l'intersezione [250,344] tra vecchia coda e nuova finestra deve restare un range contiguo: ranges={ranges:?} filled_up_to={filled_up_to}"
        );
    }

    /// Regressione per il loop infinito segnalato dall'utente e
    /// diagnosticato con `VV_DEBUG_RENDER_AHEAD` su un file reale
    /// (1080p60fps, GOP lungo, proxy disattivo): un piccolo scrub
    /// all'indietro riaggancia il decoder in anticipo (come nel test
    /// sopra) lasciando `next_frame` fermo *prima* di `source_start` —
    /// abbastanza indietro da superare la soglia adattiva. Prima del
    /// fix, ogni ciclo successivo con la testina *ferma* alla stessa
    /// posizione vedeva comunque `segment_start > next_frame + soglia`
    /// (la posizione del decoder non viene mai aggiornata da un ciclo
    /// che lo salta), quindi riseekava, ridecodificava lo stesso tratto
    /// già in cache fino a riagganciarsi allo stesso punto di prima — un
    /// loop stabile e infinito, mai autolimitantesi. `SharedFrameCache::covers`
    /// lo previene chiedendo alla cache *prima* di guardare la posizione
    /// (presunta) del decoder.
    #[test]
    fn walk_and_fill_does_not_loop_forever_after_reconnecting_early_from_a_backward_seek() {
        let path = make_test_clip("vv-app-render-ahead-test", "reconnect_loop.mp4", 20);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 43_000_000; // capacità ~139 frame, come sopra

        // Stesso setup del test sopra: riempimento iniziale a 300, poi
        // un piccolo scrub indietro a 290 che riaggancia in anticipo.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            300,
            budget,
            false,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(300),
        );
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            290,
            budget,
            true,
            false,
            DEFAULT_LOOKAHEAD_SECS,
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(290),
        );

        // 20 cicli successivi, testina ferma a 290 (nessuno scrub): nel
        // bug originale ognuno riseekava e ridecodificava da capo,
        // dominando il tempo totale (seek+decodifica reali, non solo
        // controlli in memoria) — stessa soglia/logica del test di
        // stabilità a riposo sopra.
        let start = std::time::Instant::now();
        for _ in 0..20 {
            walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                290,
                budget,
                false,
                false,
                DEFAULT_LOOKAHEAD_SECS,
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(290),
            );
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(20),
            "20 cicli a testina ferma dopo un riaggancio anticipato non devono riseekare/\
             ridecodificare a ripetizione: impiegati {elapsed:?} in totale, attesi <20ms"
        );
    }

    /// Regressione per un bug reale, confermato dall'utente con un log
    /// diagnostico su un file 1080p60fps: il controllo di riaggancio
    /// dentro `fill_segments` verificava due punti (`next_frame` e
    /// `segment.source_end`) con due `contains` *indipendenti* — se
    /// entrambi capitano per caso in due isole di cache separate (un
    /// buco tra loro, lasciato da un fill precedente saturo di budget),
    /// il controllo passava comunque, facendo credere che il segmento
    /// fosse già coperto quando in realtà c'era un buco proprio nel
    /// mezzo mai raggiunto né prima né dopo — permanente, perché il
    /// decoder si fermava lì convinto di aver finito.
    #[test]
    fn fill_segments_bridges_the_gap_between_two_disconnected_cached_islands() {
        // GOP=10 esplicito: keyframe a 0,10,20,... — serve solo poterne
        // prevedere uno vicino all'inizio del segmento richiesto, nessun
        // altro requisito sulla distanza tra le due isole sotto.
        let path =
            make_test_clip_with_short_gop("vv-app-render-ahead-test", "bridge_gap.mp4", 4, 10);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        // `fill_segments` non ha bisogno di una timeline: lavora già a
        // livello di segmento risolto.

        let caches = SharedFrameCache::new();
        // Due isole disconnesse, un buco vero in mezzo ([16,39], mai
        // toccato da niente finora) — come lascerebbe un fill precedente
        // saturo di budget.
        for idx in 5..=15 {
            caches.insert(media_a, idx, Arc::new(dummy_frame()));
        }
        for idx in 40..=50 {
            caches.insert(media_a, idx, Arc::new(dummy_frame()));
        }

        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 5,
            source_end: 50,
            timeline_start: 5,
            rate: Rational::one(),
        };
        let ctx = FillContext {
            project: &project,
            caches: &caches,
            went_backward: false,
            cache_budget_bytes: usize::MAX,
            from_frame: 5,
            proxy_enabled: false,
            target: &AtomicI64::new(5),
        };
        let _ = fill_segments(std::slice::from_ref(&segment), &ctx, &mut open);

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 5 && e >= 50),
            "il buco [16,39] tra le due isole deve essere colmato, non lasciato scoperto per \
             sempre da un riaggancio prematuro: ranges={ranges:?}"
        );
    }

    /// Regressione per lo stesso bug reale confermato dall'utente: anche
    /// dopo aver corretto il riaggancio prematuro sopra, un budget
    /// stretto poteva comunque impedire di colmare un buco lontano dal
    /// keyframe più vicino — perché i frame di puro transito (decodificati
    /// solo per attraversare un GOP lungo verso il segmento richiesto,
    /// mai parte di nessuna finestra voluta) venivano inseriti in cache e
    /// contati contro il budget come tutto il resto, potendo saturarlo
    /// prima ancora di raggiungere il tratto realmente richiesto. Qui un
    /// budget che basta per il segmento richiesto ma non per anche tutto
    /// il transito che lo precede deve comunque riuscire a colmarlo.
    #[test]
    fn fill_segments_does_not_let_transit_frames_exhaust_the_budget_before_the_wanted_range() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "transit_budget.mp4",
            4,
            250, // GOP lungo: nessun keyframe tra 0 e il segmento richiesto
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget che basta solo per il segmento richiesto (80,90], 11
        // frame, non per anche gli 80 frame di puro transito da
        // decodificare per raggiungerlo dal keyframe a 0.
        let frame_bytes = 320 * 240 * 3 / 2;
        let tight_budget = frame_bytes * 11;
        let segment = MediaSegment {
            media_id: media_a,
            source_start: 80,
            source_end: 90,
            timeline_start: 80,
            rate: Rational::one(),
        };
        let ctx = FillContext {
            project: &project,
            caches: &caches,
            went_backward: false,
            cache_budget_bytes: tight_budget,
            from_frame: 80,
            proxy_enabled: false,
            target: &AtomicI64::new(80),
        };
        let _ = fill_segments(std::slice::from_ref(&segment), &ctx, &mut open);

        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 80 && e >= 90),
            "il segmento richiesto [80,90] deve essere raggiunto, non lasciato scoperto perché \
             il budget si è esaurito sul transito prima di arrivarci: ranges={ranges:?}"
        );
    }

    /// Regressione per un bug reale, confermato dall'utente con un log
    /// diagnostico durante il playback normale su un file 1080p60fps
    /// reale con GOP=250: un primo tentativo di questo controllo
    /// stimava il transito necessario dal GOP osservato e saltava il
    /// segmento se quella stima da sola eccedeva il budget — violando
    /// la stessa regola che il controllo di budget sul frame *voluto*
    /// rispetta apposta (il transito non conta mai contro il budget del
    /// proprio tratto). L'effetto reale: il segmento *in avanti* (non
    /// solo quelli dietro) restava bloccato per centinaia di frame
    /// consecutivi ogni volta che la stima del transito appariva
    /// grande, anche con ampio budget libero — playback che si blocca
    /// per secondi ogni volta che la testina attraversa un confine di
    /// GOP. Qui si verifica che un decoder "esperto" (che conosce già
    /// GOP e ultimo keyframe, quindi stimerebbe un transito enorme per
    /// un segmento lontano) NON impedisca comunque di riempire un
    /// segmento vicino alla testina quando c'è budget di sobra — solo
    /// la disponibilità di spazio per il tratto voluto conta, mai una
    /// stima di quanto transito serva per arrivarci.
    #[test]
    fn fill_segments_does_not_block_a_reachable_segment_just_because_its_transit_would_be_large() {
        let path = make_test_clip_with_short_gop(
            "vv-app-render-ahead-test",
            "transit_estimate_does_not_block.mp4",
            4,
            25,
        );
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path: path.clone(),
            meta: MediaMeta {
                duration_frames: 100,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Decoder "esperto": conosce già GOP=25 e un keyframe a 0. Il
        // segmento sotto (24,34) richiede un transito *reale* di 24
        // frame (quasi un intero GOP) per essere raggiunto dal keyframe
        // più vicino — genuino, non frutto di un ancoraggio vecchio: se
        // il controllo lo stimasse e lo confrontasse col budget stretto
        // sotto (che basta comunque per il tratto voluto, 11 frame),
        // bloccherebbe il segmento nonostante sia in realtà
        // raggiungibile.
        let frame_bytes = 320 * 240 * 3 / 2;
        let mut od = OpenDecoder::fresh(Decoder::open(&path).unwrap(), path, false);
        od.record_keyframe_landing(0);
        od.record_keyframe_landing(25);
        open.insert(media_a, od);

        let segment = MediaSegment {
            media_id: media_a,
            source_start: 24,
            source_end: 34,
            timeline_start: 24,
            rate: Rational::one(),
        };
        // Basta per il tratto voluto (11 frame) con ampio margine, ma
        // meno del transito stimato (24 frame): con budget=frame_bytes*20
        // il vecchio controllo (stima transito >= spazio libero, anche
        // se quello spazio non serve affatto al transito) bloccava
        // comunque.
        let budget_enough_for_the_wanted_range_but_not_the_full_transit = frame_bytes * 20;
        let ctx = FillContext {
            project: &project,
            caches: &caches,
            went_backward: true,
            cache_budget_bytes: budget_enough_for_the_wanted_range_but_not_the_full_transit,
            from_frame: 24,
            proxy_enabled: false,
            target: &AtomicI64::new(24),
        };
        let outcome = fill_segments(std::slice::from_ref(&segment), &ctx, &mut open);

        assert_eq!(
            outcome,
            ControlFlow::Continue(()),
            "il tratto voluto ha budget a sufficienza: non deve essere saltato solo perché il \
             transito per arrivarci è stimato grande"
        );
        assert!(
            caches
                .cached_ranges(media_a)
                .iter()
                .any(|&(s, e)| s <= 24 && e >= 34),
            "il segmento [24,34] deve essere in cache"
        );
    }

    /// Regressione per il bug segnalato dall'utente e confermato dal log
    /// diagnostico reale: quando la testina avanza a piccoli passi (mai
    /// abbastanza da superare la soglia di seek e forzare un seek
    /// reale) il decoder resta comodamente avanti e continua da dove si
    /// trovava — corretto e voluto (vedi `position_decoder`) — ma la
    /// cache veniva sfrattata dalla sola LRU standard, che rimuove i più
    /// vecchi solo quando *arrivano* nuovi frame, non quando la *testina
    /// si sposta*: il fronte del buffer restava quindi bloccato molto
    /// indietro rispetto alla testina per un tempo indefinito, mentre la
    /// coda si allungava di pochi frame ad ogni ciclo — esattamente lo
    /// scarto fisso "il buffer inizia sempre qualche frame dopo la
    /// testina" segnalato dall'utente (confermato con un budget stretto
    /// che costringe a superare la capacità ad ogni ciclo). Con la
    /// finestra di retention dietro la testina, il buffer copre anche un
    /// tratto *prima* di ciascun target: la verifica giusta ora è che la
    /// testina sia coperta (non più in un vuoto), non che un range parta
    /// esattamente lì.
    #[test]
    fn walk_and_fill_keeps_the_buffer_front_at_the_playhead_even_without_a_real_reseek() {
        let path = make_test_clip("vv-app-render-ahead-test", "front_tracks_target.mp4", 20);

        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 500,
                fps: Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget stretto: ogni avanzamento di 10 frame aggiunge più
        // frame di quanti la cache possa contenere senza sfrattarne,
        // costringendo lo sfratto ad agire ad ogni ciclo.
        let budget = 43_000_000;

        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            10,
            budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(10),
        );

        let mut target = 300;
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            target,
            budget,
            false,
            false,                  // proxy_enabled: irrilevante per questo test
            DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
            DEFAULT_BEHIND_SECS,
            &AtomicI64::new(target),
        );
        for _ in 0..15 {
            target += 10;
            walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                &mut open_behind,
                target,
                budget,
                false,
                false,                  // proxy_enabled: irrilevante per questo test
                DEFAULT_LOOKAHEAD_SECS, // read_ahead: irrilevante per questo test
                DEFAULT_BEHIND_SECS,
                &AtomicI64::new(target),
            );
            let ranges = caches.cached_ranges(media_a);
            assert!(
                ranges.iter().any(|&(s, e)| s <= target && target <= e),
                "il buffer deve coprire la testina (target={target}): {ranges:?}"
            );
        }
    }
}
