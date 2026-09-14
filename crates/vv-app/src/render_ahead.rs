//! Buffer video a livello di *timeline*, non di singola clip: un thread
//! dedicato cammina in avanti dal playhead per `LOOKAHEAD_SECS`,
//! attraversando quante clip servono (tagli netti, vuoti, stesso media o
//! diverso — nessun caso speciale), e riempie una `SharedFrameCache`
//! *unica*, condivisa da tutti i media della finestra, a budget globale
//! in byte con sfratto per priorità-distanza-dalla-testina (vedi
//! `vv_media::cache` e REFACTOR_PIPELINE.md §2 — sostituisce una
//! generazione precedente di questo modulo che usava N cache
//! indipendenti con budget diviso, la cui divisione arbitraria era
//! radice di più di un bug). Sostituisce a sua volta il sistema di
//! preload "per clip" ancora precedente (`GapPlayback`/`NextPreload`/
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
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use vv_core::{ClipSource, FrameIdx, MediaId, Project, Timeline, TimelineId};
use vv_media::{Decoder, FrameRgba, SharedFrameCache, WantedRange};

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

/// Log diagnostico opzionale su stderr, attivo solo con la variabile
/// d'ambiente `VV_DEBUG_RENDER_AHEAD=1`: per capire da rapporti utente
/// cosa succede davvero su una macchina/file che non riesco a
/// riprodurre qui (apertura/seek di un decoder, stato della finestra ad
/// ogni ciclo di poll) senza appesantire l'uso normale.
fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("VV_DEBUG_RENDER_AHEAD").is_ok())
}

/// Vedi il doc del modulo. Uno per `VibeVideoApp` (non uno per clip: la
/// differenza chiave rispetto al sistema precedente).
pub struct RenderAhead {
    caches: Arc<SharedFrameCache>,
    target: Arc<AtomicI64>,
    cache_budget_bytes: Arc<AtomicUsize>,
    tx: mpsc::Sender<Command>,
    handle: Option<JoinHandle<()>>,
}

impl RenderAhead {
    pub fn spawn(project: Project, timeline_id: TimelineId, cache_budget_bytes: usize) -> Self {
        let caches = Arc::new(SharedFrameCache::new());
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
        self.caches.get(media_id, source_frame)
    }

    /// Intervalli (in frame *sorgente*) attualmente in cache per un
    /// media — per l'indicatore visivo "buffered" sulla timeline (il
    /// chiamante li traduce in spazio timeline per la clip in questione,
    /// vedi `map_source_ranges_to_timeline` in main.rs).
    pub fn cached_ranges_for(&self, media_id: MediaId) -> Vec<(FrameIdx, FrameIdx)> {
        self.caches.cached_ranges(media_id)
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
/// (vedi `position_decoder`). Uno per media (non uno slot condiviso): se
/// la finestra di lookahead attraversa un taglio tra due media diversi,
/// entrambi restano posizionati da un ciclo di poll all'altro invece di
/// essere riaperti ogni volta che il segmento "torna" al primo.
struct OpenDecoder {
    decoder: Decoder,
    next_frame: FrameIdx,
}

fn worker_loop(
    rx: mpsc::Receiver<Command>,
    caches: Arc<SharedFrameCache>,
    target: Arc<AtomicI64>,
    cache_budget_bytes: Arc<AtomicUsize>,
    mut project: Project,
    mut timeline_id: TimelineId,
) {
    let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
    // Ultimo `from` visto, confrontato una sola volta per ciclo (non per
    // media/segmento, vedi doc di `walk_and_fill`) per sapere se la
    // testina è tornata indietro dall'ultimo ciclo — qualunque sia
    // l'ampiezza. Aggiornato per semplice assegnazione a ogni ciclo, mai
    // un min/max con lo storico: deve riflettere solo il ciclo
    // *precedente*, altrimenti (bug osservato in precedenza) può restare
    // bloccato su un valore vecchio e smettere di rilevare scrub reali.
    let mut last_from_frame: Option<FrameIdx> = None;
    // `true` quando l'ultimo `walk_and_fill` è stato interrotto perché la
    // testina si è spostata abbastanza da rendere il lavoro in corso
    // obsoleto (vedi `walk_and_fill`): in quel caso non ha senso aspettare
    // fino al prossimo `POLL_INTERVAL`, il target fresco va riletto subito.
    let mut retry_immediately = false;
    loop {
        if retry_immediately {
            match rx.try_recv() {
                Ok(Command::Stop) => return,
                Ok(Command::UpdateProject(p, id)) => {
                    project = *p;
                    timeline_id = id;
                    open.clear();
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(Command::Stop) => return,
                Ok(Command::UpdateProject(p, id)) => {
                    project = *p;
                    timeline_id = id;
                    open.clear();
                }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        // Drena eventuali altri comandi già in coda (non bloccante): solo
        // l'ultimo conta, i precedenti sono superati.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::Stop => return,
                Command::UpdateProject(p, id) => {
                    project = *p;
                    timeline_id = id;
                    open.clear();
                }
            }
        }

        let from = target.load(Ordering::Relaxed);
        let went_backward = last_from_frame.is_some_and(|last| from < last);
        last_from_frame = Some(from);
        let budget = cache_budget_bytes.load(Ordering::Relaxed);
        retry_immediately = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            from,
            budget,
            went_backward,
            &target,
        );
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

/// Un tratto contiguo di timeline coperto da una singola clip Media, in
/// spazio frame *sorgente*, con la posizione timeline corrispondente
/// (`timeline_start`, dove cade `source_start`) — necessaria per
/// costruire i `WantedRange` passati a `SharedFrameCache::reconcile`,
/// che ne ha bisogno per calcolare la distanza dalla testina.
struct MediaSegment {
    media_id: MediaId,
    source_start: FrameIdx,
    source_end: FrameIdx,
    timeline_start: FrameIdx,
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
                        timeline_start: frame,
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

/// Assicura che `open[media_id]` sia un decoder posizionato in modo da
/// poter coprire `segment_start` decodificando in avanti in modo
/// efficiente. Un decoder già aperto per questo media viene riusato —
/// anche se è *oltre* `segment_start`, incluso il caso in cui abbia già
/// superato `segment_end` — perché non c'è nulla che un riapertura
/// potrebbe migliorare in quel caso: se la cache copre già il segmento va
/// tutto bene così, se non lo copre (limite di capacità, non di
/// posizione) rifare lo stesso seek produce esattamente lo stesso
/// risultato all'infinito. Essere avanti rispetto all'inizio del
/// segmento è lo stato **sano e atteso** di un buffer che lavora bene —
/// trattarlo come motivo di un seek è il bug che causava un seek reale
/// (e quindi una nuova decodifica completa della finestra) ogni ciclo di
/// poll, sia quando il decoder era leggermente avanti sia appena il giro
/// precedente si era concluso esattamente al termine del segmento.
///
/// Un seek reale serve in due casi, indipendenti tra loro:
/// - `segment_start` è troppo avanti per continuare a decodificare in
///   sequenza fino a lì (costa meno un seek);
/// - la testina di *timeline* è tornata indietro dal ciclo di poll
///   precedente (`went_backward`, deciso *una sola volta per ciclo* dal
///   chiamante — vedi `worker_loop`/`walk_and_fill` — confrontando il
///   target globale, non per-media/per-segmento) **e** questo decoder è
///   fisicamente già oltre `segment_start`: essere avanti è sano quando
///   la testina è ferma o avanza (il decoder ha bufferizzato bene), ma
///   se la testina è appena tornata indietro non può significare altro
///   che "questo decoder ha superato la nuova posizione e non potrà mai
///   tornarci decodificando solo in avanti" (`walk_and_fill` scarta ad
///   ogni ciclo, via `SharedFrameCache::reconcile`, tutto ciò che è
///   fuori dalla finestra corrente — anche un passo indietro di un solo
///   frame cade subito fuori finestra e viene scartato).
///
/// `went_backward` è deciso una volta per l'intero ciclo (non da uno
/// stato per-media come in una versione precedente di questo codice):
/// un taglio produce più segmenti per lo stesso media nella stessa
/// finestra (la clip prima e quella dopo), e derivare "sono tornato
/// indietro?" da uno stato per-media aggiornato mentre si itera sui
/// segmenti si è dimostrato fragile due volte (vedi git log) — sporcato
/// dall'ordine di elaborazione dei segmenti nello stesso ciclo, poi
/// reso "sticky" da un tentativo di correzione. Con un'unica decisione
/// globale per ciclo, il caso multi-segmento non può più contaminarla:
/// non viene mai letta né scritta prima di sapere se la testina si è
/// davvero mossa.
///
/// Quando serve un seek per lo stesso media già aperto, va fatto sul
/// decoder *esistente* (`seek_to_time`), non riaprendo il file da
/// `Decoder::open`: per un file grande/non ottimizzato per lo
/// streaming, riaprire vuol dire riparsare l'intero container/indice
/// ogni volta, un costo che può arrivare a secondi — se supera la
/// tolleranza (`SEEK_THRESHOLD_FRAMES`) il target avanza oltre durante
/// l'apertura stessa, scatenandone un'altra al giro successivo, in un
/// loop che non recupera mai (osservato: riproduzione a scatti, un
/// frame ogni pochi secondi). `Decoder::open` va usato solo per la
/// primissima apertura di un media (nessun decoder ancora in `open`) o
/// per uno diverso da quello aperto. Ritorna `Positioned::Failed` se
/// l'apertura del media fallisce (il chiamante salta quel segmento);
/// altrimenti riporta cosa è stato fatto per arrivarci — usato dai test
/// per verificare che un seek reale scatti solo quando davvero serve,
/// non ad ogni ciclo.
fn position_decoder(
    open: &mut HashMap<MediaId, OpenDecoder>,
    media_id: MediaId,
    path: &Path,
    segment_start: FrameIdx,
    went_backward: bool,
) -> Positioned {
    if let Some(o) = open.get_mut(&media_id) {
        let needs_seek = segment_start > o.next_frame + SEEK_THRESHOLD_FRAMES
            || (went_backward && segment_start < o.next_frame);
        if !needs_seek {
            return Positioned::Reused;
        }
        // Stesso media già aperto, serve solo tornare/saltare a un altro
        // punto: un `seek_to_time` sul decoder esistente riusa il file
        // già aperto e il container/indice già parsato — aprire di
        // nuovo da `Decoder::open` per un semplice seek è il bug che
        // rendeva ogni riposizionamento costoso quanto la primissima
        // apertura del file (per un file grande, anche secondi): se
        // quel costo supera la tolleranza (`SEEK_THRESHOLD_FRAMES`), il
        // target avanza oltre *durante* l'apertura stessa, scatenando
        // un'altra apertura completa al giro successivo — un loop che
        // non recupera mai (osservato: "1 frame ogni pochi secondi").
        let secs = segment_start as f64 / o.decoder.fps().as_f64().max(1e-9);
        let debug_start = debug_enabled().then(std::time::Instant::now);
        let _ = o.decoder.seek_to_time(secs);
        if let Some(t) = debug_start {
            eprintln!(
                "[render_ahead] seek (decoder riusato) media={media_id:?} target={segment_start} elapsed={:?}",
                t.elapsed()
            );
        }
        // Placeholder: il prossimo `next_frame()` restituisce l'idx
        // *reale* del keyframe da cui riparte (può essere <
        // segment_start), che aggiorna subito questo campo nel loop di
        // decodifica sotto.
        o.next_frame = 0;
        return Positioned::Seeked;
    }
    // Nessun decoder aperto per questo media: qui l'apertura reale è
    // inevitabile (prima volta, o media diverso da quello aperto finora).
    let debug_start = debug_enabled().then(std::time::Instant::now);
    let Ok(mut decoder) = Decoder::open(path) else {
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
    open.insert(media_id, OpenDecoder { decoder, next_frame: 0 });
    Positioned::Opened
}

/// Cammina `LOOKAHEAD_SECS` avanti da `from_frame` e riempie la
/// `SharedFrameCache` (REFACTOR_PIPELINE.md §2) in ordine di priorità:
/// i segmenti restituiti da `collect_media_segments` sono già ordinati
/// dal più vicino alla testina al più lontano (si cammina la timeline in
/// avanti da `from_frame`), quindi elaborarli in quest'ordine — fermandosi
/// non appena il budget globale è saturo — significa che il frame che
/// serve ORA (sotto la testina, qualunque sia il suo media) è sempre il
/// primo a essere bufferizzato, senza bisogno di un vero scheduler
/// multi-thread (REFACTOR_PIPELINE.md §3.4, non ancora fatto).
///
/// `went_backward`: se la testina di timeline è tornata indietro dal
/// ciclo di poll precedente, decisa una sola volta dal chiamante
/// (confrontando `from_frame` globale, non per-media/per-segmento —
/// vedi `position_decoder`) prima di iterare sui segmenti.
///
/// Ritorna `true` se il fill è stato interrotto in anticipo perché la
/// testina *live* (`target`) si è spostata abbastanza, mentre si
/// decodificava, da rendere obsoleto il lavoro rimasto in questo ciclo —
/// in quel caso il chiamante (`worker_loop`) non aspetta il prossimo
/// `POLL_INTERVAL`, rilegge subito il target fresco e ricomincia: un
/// prefetch lontano non deve mai far aspettare la testina che si sposta
/// nel frattempo (REFACTOR_PIPELINE.md §3.1).
fn walk_and_fill(
    project: &Project,
    timeline_id: TimelineId,
    caches: &SharedFrameCache,
    open: &mut HashMap<MediaId, OpenDecoder>,
    from_frame: FrameIdx,
    cache_budget_bytes: usize,
    went_backward: bool,
    target: &AtomicI64,
) -> bool {
    let Some(timeline) = project.timelines.get(timeline_id) else {
        return false;
    };
    let fps = timeline.fps.as_f64().max(1e-9);
    let lookahead_frames = ((LOOKAHEAD_SECS * fps).round() as FrameIdx).max(1);
    let end_frame = from_frame + lookahead_frames;

    let segments = collect_media_segments(timeline, from_frame, end_frame);
    if segments.is_empty() {
        return false;
    }
    let distinct_media: HashSet<MediaId> = segments.iter().map(|s| s.media_id).collect();
    open.retain(|id, _| distinct_media.contains(id));

    let window: Vec<WantedRange> = segments
        .iter()
        .map(|s| WantedRange {
            media_id: s.media_id,
            source_start: s.source_start,
            source_end: s.source_end,
            timeline_start: s.timeline_start,
        })
        .collect();
    // Un solo pass di riconciliazione (vedi doc di `SharedFrameCache::
    // reconcile`): scarta ciò che è uscito dalla finestra (qualunque sia
    // il motivo — media non più presente, dietro la testina, oltre
    // l'orizzonte) e, se il budget globale non basta per tutto ciò che è
    // rimasto, sfratta il contenuto più lontano dalla testina. Sostituisce
    // insieme retain/evict_before/sfratto-per-capacità di prima.
    caches.reconcile(from_frame, &window, cache_budget_bytes);

    'segments: for segment in &segments {
        let Some(path) = project
            .media_pool
            .get(segment.media_id)
            .map(|m| m.path.clone())
        else {
            continue;
        };
        if position_decoder(
            open,
            segment.media_id,
            &path,
            segment.source_start,
            went_backward,
        ) == Positioned::Failed
        {
            continue;
        }

        // Diventa `true` dopo il primo frame realmente decodificato in
        // questo giro: prima di allora `od.next_frame` può essere solo
        // il placeholder `0` impostato da `position_decoder` dopo un
        // (ri)apertura/seek, non ancora corretto al vero indice del
        // keyframe da cui il decoder riparte — controllare la cache
        // prima di quel momento potrebbe far fermare il ciclo per un
        // falso positivo (0 per coincidenza già in cache) senza aver mai
        // scoperto la vera posizione del decoder.
        let mut resumed = false;
        loop {
            let od = open.get_mut(&segment.media_id).unwrap();
            if od.next_frame > segment.source_end {
                break;
            }
            // Ci siamo ricongiunti con una porzione già bufferizzata (non
            // sfrattata perché dentro alla finestra corrente, vedi
            // `SharedFrameCache::reconcile`) che arriva *fino alla fine
            // di questo segmento*: da qui in avanti dovrebbe già esserci
            // tutto, quindi continuare a decodificare sarebbe lavoro
            // sprecato — un piccolo scrub all'indietro deve ridecodificare
            // solo il nuovo tratto scoperto prima del punto di
            // riconnessione, non l'intera finestra. Controllare *anche*
            // `segment.source_end` (non solo il prossimo frame) evita un
            // falso positivo quando si sta solo attraversando un'isola di
            // cache lasciata da un *altro* segmento dello stesso media in
            // questa stessa finestra (un taglio tra due pezzi non
            // contigui dello stesso file): fermarsi lì lascerebbe
            // scoperta la vera destinazione di questo segmento, più
            // avanti.
            if resumed
                && caches.contains(segment.media_id, od.next_frame)
                && caches.contains(segment.media_id, segment.source_end)
            {
                break;
            }
            // Budget globale saturo: i segmenti restanti (questo incluso,
            // da qui in poi) sono per costruzione più lontani dalla
            // testina di tutto ciò che è già in cache (fill in ordine di
            // priorità, vedi doc della funzione) — non c'è nulla da
            // guadagnare continuando, esce dall'intero giro sui segmenti,
            // non solo da questo.
            if caches.bytes_used() >= cache_budget_bytes {
                break 'segments;
            }
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    // `insert` sovrascrive innocuamente se `idx` è già
                    // presente (decoder ripartito da un keyframe
                    // precedente al punto richiesto): evita solo un ramo
                    // che avanzi `next_frame` senza consumare davvero un
                    // frame dal decoder (in passato causa di un
                    // disallineamento tra la posizione tracciata e quella
                    // reale).
                    caches.insert(segment.media_id, idx, Arc::new(frame));
                    od.next_frame = idx + 1;
                    resumed = true;
                }
                _ => break,
            }
            // Rilettura economica (atomica) del target *live*, non solo
            // quello letto a inizio ciclo: se nel frattempo la testina si
            // è spostata abbastanza da rendere questo prefetch obsoleto,
            // interrompe subito invece di finire di decodificare un
            // segmento che non serve più — il chiamante (`worker_loop`)
            // ricomincia immediatamente con il target fresco, senza
            // aspettare fino al prossimo `POLL_INTERVAL`. Soglia condivisa
            // con `position_decoder` (`SEEK_THRESHOLD_FRAMES`): sotto
            // quella distanza il lavoro in corso è ancora utile (drift
            // normale di riproduzione), sopra è uno scrub/salto che rende
            // la finestra corrente stale.
            let live = target.load(Ordering::Relaxed);
            if (live - from_frame).abs() > SEEK_THRESHOLD_FRAMES {
                return true;
            }
        }

        if debug_enabled() {
            let final_next_frame = open.get(&segment.media_id).unwrap().next_frame;
            eprintln!(
                "[render_ahead] media={:?} target_frame={from_frame} segment=[{},{}] next_frame_after={final_next_frame} bytes_used={} cached_ranges={:?}",
                segment.media_id,
                segment.source_start,
                segment.source_end,
                caches.bytes_used(),
                caches.cached_ranges(segment.media_id)
            );
        }
    }
    false
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
                "-g",
                &gop.to_string(),
                "-keyint_min",
                &gop.to_string(),
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
        Clip {
            id: ClipId(id),
            source: ClipSource::Media(media_id),
            source_in,
            source_out: source_in + len,
            timeline_start,
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
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
        }]));

        let render_ahead = RenderAhead::spawn(project, timeline_id, 100_000_000);
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

    /// Regressione: un seek reale per un media già aperto deve riusare
    /// il decoder esistente (`seek_to_time`), non buttarlo via per
    /// riaprire il file da zero — per un file grande/non ottimizzato per
    /// lo streaming, riaprire vuol dire riparsare l'intero indice ogni
    /// volta (anche secondi), e se quel costo eccede la tolleranza il
    /// target avanza oltre durante l'apertura stessa, scatenandone
    /// un'altra al giro successivo: un loop che non recupera mai
    /// (osservato: un frame ogni pochi secondi). Verificato passando un
    /// path inesistente al secondo giro: se il decoder venisse
    /// riaperto invece di riusato, questa chiamata fallirebbe.
    #[test]
    fn position_decoder_reuses_the_open_decoder_for_a_real_seek_instead_of_reopening_the_file() {
        let path = make_test_clip("vv-app-render-ahead-test", "reuse.mp4", 3);
        let bogus_path = std::path::PathBuf::from("/nonexistent/reuse.mp4");
        let (media_a, _) = two_media_ids();

        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 0, false),
            Positioned::Opened
        );

        assert_eq!(
            position_decoder(&mut open, media_a, &bogus_path, 1000, false),
            Positioned::Seeked,
            "il path bogus non deve impedire il riuso del decoder già aperto"
        );
        assert_eq!(open.get(&media_a).unwrap().next_frame, 0);
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

        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 0, false),
            Positioned::Opened
        );

        // Decodifica qualche frame in avanti "a mano", come farebbe
        // walk_and_fill, per simulare un decoder già bufferizzato oltre
        // il target attuale.
        for _ in 0..20 {
            let od = open.get_mut(&media_a).unwrap();
            match od.decoder.next_frame() {
                Ok(Some((idx, _))) => od.next_frame = idx + 1,
                _ => break,
            }
        }
        let advanced_next_frame = open.get(&media_a).unwrap().next_frame;
        assert!(advanced_next_frame > 0, "il decoder deve aver avanzato");

        // Un ciclo successivo con il target ancora dietro alla posizione
        // del decoder — lo stato normale durante il playback in avanti —
        // non deve riaprire/riazzerare il decoder.
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 0, false),
            Positioned::Reused
        );
        assert_eq!(
            open.get(&media_a).unwrap().next_frame,
            advanced_next_frame,
            "non deve aver riaperto il decoder mentre è ancora utilmente avanti"
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 75)],
            muted: false,
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget minuscolo: la finestra di lookahead (3s = 75 frame a
        // 25fps) non ci sta tutta nella cache.
        let tiny_budget = 320 * 240 * 4 * 5;
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            0,
            tiny_budget,
            false,
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
    /// già spostato oltre `SEEK_THRESHOLD_FRAMES` rispetto a `from_frame`
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Il target live è già oltre soglia rispetto a from_frame=0 prima
        // ancora che il fill inizi: simula la testina che è saltata
        // altrove mentre questo ciclo stava per partire.
        let drifted_target = AtomicI64::new(SEEK_THRESHOLD_FRAMES + 200);

        let interrupted = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            0,
            100_000_000,
            false,
            &drifted_target,
        );
        assert!(
            interrupted,
            "deve segnalare l'interruzione al chiamante (worker_loop) per farlo ripartire subito"
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
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
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 100_000_000;

        // La finestra di lookahead (3s = 75 frame a 25fps) da 40
        // attraversa il taglio a 60, includendo un pezzo di entrambe le
        // clip nello stesso ciclo.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            40,
            budget,
            false,
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
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
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Capacità ~50 frame: meno di quanto i due segmenti insieme
        // chiederebbero (~20 + ~55), ma più di quanto ciascuno chiede da
        // solo — costringe la condivisione della stessa cache a contare
        // davvero.
        let budget = 50 * 320 * 240 * 4;

        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            40,
            budget,
            false,
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
    }

    /// Regressione per il bug segnalato dall'utente: con la testina
    /// ferma subito prima di un taglio (basta tagliare una clip e
    /// posizionare la testina appena prima del punto di taglio: la
    /// finestra di lookahead include comunque un pezzo di entrambe le
    /// metà, due segmenti dello stesso media), ogni ciclo di poll
    /// rielabora gli stessi due segmenti nello stesso ordine. In una
    /// versione precedente di questo codice, `requested_start` (stato
    /// *per media*, aggiornato dentro il loop sui segmenti) veniva
    /// sovrascritto con il `segment_start` dell'*ultimo* segmento
    /// processato: al ciclo successivo, rielaborando il *primo* segmento
    /// (source_start più basso), il confronto lo leggeva sempre come
    /// "tornato indietro" — scatenando un seek reale a ogni singolo
    /// ciclo pur restando fermi. Con `went_backward` deciso una volta
    /// sola per ciclo (non per segmento, vedi doc di `position_decoder`)
    /// il caso multi-segmento non può proprio più presentarsi: verificato
    /// qui passando esplicitamente `false` (testina ferma) a entrambi i
    /// segmenti in entrambi i cicli.
    #[test]
    fn position_decoder_does_not_reseek_across_cycles_when_the_same_media_appears_in_two_segments()
     {
        let path = make_test_clip("vv-app-render-ahead-test", "same_media_two_segments.mp4", 3);
        let (media_a, _) = two_media_ids();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();

        // Ciclo 1: due segmenti dello stesso media nella stessa finestra
        // (come ai due lati di un taglio), source_start 10 e poi 50.
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 10, false),
            Positioned::Opened
        );
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 50, false),
            Positioned::Reused,
            "nello stesso ciclo il secondo segmento non deve mai richiedere un seek: il decoder è già lì"
        );

        // Ciclo 2, testina ferma (`went_backward=false` per entrambi):
        // stessi due segmenti. Rielaborare il *primo* segmento (10) non
        // deve sembrare "tornato indietro" solo perché l'ultima chiamata
        // vista nel ciclo precedente era per il segmento successivo (50).
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 10, false),
            Positioned::Reused,
            "testina ferma: rielaborare il primo segmento non deve scatenare un seek reale"
        );
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 50, false),
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
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
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
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
            0,
            total_budget,
            false,
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
            200,
            total_budget,
            false,
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
        }]));

        let render_ahead = RenderAhead::spawn(project, timeline_id, 100_000_000);
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
    /// all'indietro *piccolo* (qui 30 frame, ben sotto
    /// `SEEK_THRESHOLD_FRAMES`=120) deve rigenerare il buffer per la
    /// nuova posizione tanto quanto uno grande. Prima del fix, restava
    /// bloccato sul frame in cache più vicino perché `position_decoder`
    /// considerava "abbastanza avanti" qualunque target ancora dietro
    /// a `next_frame` più di una soglia — ma `walk_and_fill` scarta ad
    /// ogni ciclo tutto ciò che è dietro alla testina corrente
    /// (`evict_before`), quindi anche un piccolo passo indietro cade in
    /// territorio già scartato e irraggiungibile decodificando solo in
    /// avanti.
    #[test]
    fn render_ahead_catches_up_after_a_small_backward_seek_within_the_old_threshold() {
        let path = make_test_clip("vv-app-render-ahead-test", "small_backward_seek.mp4", 4);
        let mut project = Project::default();
        let media_a = project.media_pool.insert(MediaItem {
            path,
            meta: MediaMeta {
                duration_frames: 100,
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
            clips: vec![media_clip(1, media_a, 0, 100)],
            muted: false,
        }]));

        let render_ahead = RenderAhead::spawn(project, timeline_id, 100_000_000);
        render_ahead.set_target(80);

        // Attendi non solo che il buffer copra 80, ma che i cicli di
        // poll successivi abbiano anche già scartato (`evict_before`)
        // ciò che è rimasto dietro a 80 (compreso 50) — altrimenti il
        // test passerebbe per caso, perché il primo riempimento (che
        // decodifica dal keyframe più vicino, qui l'inizio del file)
        // include già 50 prima ancora che venga scartato.
        let covers = |ranges: &[(FrameIdx, FrameIdx)], f: FrameIdx| {
            ranges.iter().any(|&(s, e)| s <= f && f <= e)
        };
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if covers(&ranges, 80) && !covers(&ranges, 50) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout in avanti: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Scrub indietro di soli 30 frame (sotto la vecchia soglia di
        // 120): deve comunque rigenerare il buffer per la nuova
        // posizione, non restare bloccato sul frame più vicino già in
        // cache.
        render_ahead.set_target(50);
        let start = std::time::Instant::now();
        loop {
            let ranges = render_ahead.cached_ranges_for(media_a);
            if ranges.iter().any(|&(s, _)| s == 50) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout su scrub piccolo indietro: ranges={ranges:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Regressione generale: dopo un po' di playback in avanti su più
    /// cicli, uno scrub all'indietro verso una posizione più recente del
    /// primissimo target mai visto deve comunque far ricalcolare il
    /// buffer per la nuova posizione. Ha già scoperto due bug diversi in
    /// due iterazioni di questo codice: prima un `requested_start`
    /// per-media che restava "sticky" dopo un fix mal fatto (un `.min()`
    /// invece di una sovrascrittura), poi — nella versione attuale —
    /// verifica che il `went_backward` calcolato una volta per ciclo
    /// (qui simulato esplicitamente dal test, come farebbe
    /// `worker_loop`) funzioni correttamente su una sequenza realistica
    /// di cicli, non solo su un singolo salto indietro isolato.
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
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
                from,
                budget,
                went_backward,
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
            80,
            budget,
            true,
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
    /// nella *nuova* finestra non deve essere ridecodificata — solo il
    /// tratto scoperto tra il nuovo target e il punto di riconnessione
    /// con la cache esistente. Verificato osservando
    /// `OpenDecoder::next_frame` dopo il seek: deve fermarsi al punto di
    /// riconnessione (270), non continuare a ridecodificare quel che è
    /// già lì.
    ///
    /// Nota (REFACTOR_PIPELINE.md §2, Tier A): con la `SharedFrameCache`
    /// a budget globale, `reconcile` scarta anche ciò che è *oltre*
    /// l'orizzonte della nuova finestra (qui: oltre 344, dato che la
    /// nuova testina è 270) — a differenza della vecchia `evict_before`,
    /// che scartava solo ciò che era dietro e lasciava intatto tutto ciò
    /// che era avanti, qualunque fosse l'orizzonte. È voluto: il budget
    /// della finestra è sempre esattamente quello della finestra
    /// corrente, non un accumulo indefinito di code storiche. Quindi qui
    /// si verifica solo che [270,344] (l'intersezione tra vecchia coda e
    /// nuova finestra) sia raggiungibile senza ridecodificarla — non che
    /// tutta la vecchia coda fino a 374 sopravviva.
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        let budget = 43_000_000; // capacità ~139 frame

        // Bufferizza attorno a 300: con keyint=250 (default libx264) il
        // decoder riparte dal keyframe 250 e riempie fino al limite di
        // capacità.
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            300,
            budget,
            false,
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
            270,
            budget,
            true,
            &AtomicI64::new(270),
        );

        let next_frame_after = open.get(&media_a).unwrap().next_frame;
        assert_eq!(
            next_frame_after, 270,
            "deve fermarsi appena si ricongiunge con il buffer esistente (270), non ridecodificare fino al nuovo capped_source_end: next_frame={next_frame_after}"
        );

        // L'intersezione tra la vecchia coda e la nuova finestra
        // ([270,344]) deve essere raggiungibile come un range contiguo,
        // senza buchi dovuti a una ridecodifica sprecata. Il fatto che
        // `filled_up_to` (374) sia più avanti dell'orizzonte della nuova
        // finestra è atteso: quella parte è stata scartata dal Tier A di
        // `reconcile` perché non più nella finestra corrente (vedi nota
        // sopra), non perché la riconnessione abbia fallito.
        let ranges = caches.cached_ranges(media_a);
        assert!(
            ranges.iter().any(|&(s, e)| s <= 270 && e >= 344),
            "l'intersezione [270,344] tra vecchia coda e nuova finestra deve restare un range contiguo: ranges={ranges:?} filled_up_to={filled_up_to}"
        );
    }

    /// Regressione per il bug segnalato dall'utente e confermato dal log
    /// diagnostico reale: quando la testina avanza a piccoli passi (mai
    /// abbastanza da superare `SEEK_THRESHOLD_FRAMES` e forzare un seek
    /// reale) il decoder resta comodamente avanti e continua da dove si
    /// trovava — corretto e voluto (vedi `position_decoder`) — ma la
    /// cache veniva sfrattata dalla sola LRU standard, che rimuove i più
    /// vecchi solo quando *arrivano* nuovi frame, non quando la *testina
    /// si sposta*: il fronte del buffer restava quindi bloccato molto
    /// indietro rispetto alla testina per un tempo indefinito, mentre la
    /// coda si allungava di pochi frame ad ogni ciclo — esattamente lo
    /// scarto fisso "il buffer inizia sempre qualche frame dopo la
    /// testina" segnalato dall'utente (confermato con un budget stretto
    /// che costringe a superare la capacità ad ogni ciclo).
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 500)],
            muted: false,
        }]));

        let caches = SharedFrameCache::new();
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Budget stretto: ogni avanzamento di 10 frame aggiunge più
        // frame di quanti la cache possa contenere senza sfrattarne,
        // costringendo lo sfratto ad agire ad ogni ciclo.
        let budget = 43_000_000;

        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            10,
            budget,
            false,
            &AtomicI64::new(10),
        );

        let mut target = 300;
        walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            target,
            budget,
            false,
            &AtomicI64::new(target),
        );
        for _ in 0..15 {
            target += 10;
            walk_and_fill(
                &project,
                timeline_id,
                &caches,
                &mut open,
                target,
                budget,
                false,
                &AtomicI64::new(target),
            );
            let ranges = caches.cached_ranges(media_a);
            assert!(
                ranges.iter().any(|&(s, _)| s == target),
                "il buffer deve iniziare esattamente alla testina (target={target}): {ranges:?}"
            );
        }
    }

}
