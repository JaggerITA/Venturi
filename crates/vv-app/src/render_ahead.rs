//! Buffer video a livello di *timeline*, non di singola clip: un thread
//! dedicato cammina in avanti dal playhead per `lookahead_secs`,
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
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use vv_core::{ClipSource, FrameIdx, MediaId, Project, Timeline, TimelineId};
use vv_media::{Decoder, FrameYuv420, SharedFrameCache, WantedRange};

/// Valore di default/iniziale per `RenderAhead::set_lookahead_secs` —
/// quanti secondi di timeline tenere bufferizzati avanti dal playhead,
/// attraversando quante clip servono per coprirli. Configurabile
/// dall'utente (menu Playback > Proxy), non più un valore fisso.
pub const DEFAULT_LOOKAHEAD_SECS: f64 = 3.0;

/// Valore di default/iniziale per `RenderAhead::set_behind_secs` —
/// quanti secondi di timeline tenere bufferizzati anche *dietro* la
/// testina, oltre alla finestra in avanti sopra — deliberatamente molto
/// più piccola: la priorità resta sempre in avanti (vedi l'ordine di
/// fill in `walk_and_fill`, dietro riempito solo con quel che resta del
/// budget). Serve solo a rendere economico uno scrub avanti-indietro
/// ravvicinato (es. confrontare due punti vicini, o un piccolo
/// tentennamento della mano) senza forzare una ridecodifica completa a
/// ogni piccolo passo indietro — non è un buffer di "rewind" per uno
/// scrub lontano, quello resta correttamente costoso quanto un seek in
/// avanti verso una zona mai visitata.
pub const DEFAULT_BEHIND_SECS: f64 = 2.0;

/// Pavimento (in frame) applicato a `lookahead_secs`/`behind_secs`
/// qualunque sia il valore configurato dall'utente, anche `0` — non
/// zero: un margine letteralmente nullo (un solo frame, "decodifica
/// esattamente quel che serve ora") è strutturalmente fragile anche
/// svegliando il worker subito a ogni cambio di target (`Command::Wake`),
/// perché resta comunque un design senza alcun cuscinetto contro la
/// normale variabilità di timing (contesa CPU, scheduling del SO) — bug
/// segnalato dall'utente: playback a scatti anche a 1x, indipendente dal
/// proxy. Volutamente piccolo (pochi frame, non i secondi di default):
/// serve solo da rete di sicurezza per chi configura un anticipo troppo
/// aggressivo, non da anticipo vero e proprio.
const MIN_MARGIN_FRAMES: FrameIdx = 4;

/// Dimensione (in frame) dei blocchi in cui viene spezzata la finestra
/// *dietro* la testina prima di decodificarla (vedi
/// `chunk_behind_segments_near_to_far`): un segmento dietro copre
/// `[source_start, source_end]` con `source_end` adiacente alla testina
/// e `source_start` sul bordo lontano — decodificarlo in un solo seek,
/// come per la finestra in avanti, produrrebbe i frame più vicini alla
/// testina *per ultimi* (ffmpeg decodifica solo in avanti, non può
/// "andare indietro" da `source_end`), l'opposto di quel che serve
/// durante uno scrub all'indietro (segnalato dall'utente: il frame utile
/// *subito* è quello adiacente alla testina, non quello più lontano).
/// Spezzare in blocchi piccoli e riseekare a ognuno, dal più vicino al
/// più lontano, riordina la priorità di decodifica senza cambiare quella
/// della finestra in avanti (dove `source_start` È già la testina, un
/// solo seek è già ottimo). Non frame-per-frame: ogni blocco costa un
/// seek reale, quasi gratis su un proxy tutto-intra ma non su un
/// sorgente long-GOP — abbastanza piccolo da sentirsi "immediato" (una
/// manciata di frame), abbastanza grande da non moltiplicare i seek
/// senza motivo.
const BEHIND_CHUNK_FRAMES: FrameIdx = 15;

fn store_secs(atomic: &AtomicU64, secs: f64) {
    atomic.store(secs.max(0.0).to_bits(), Ordering::Relaxed);
}

fn load_secs(atomic: &AtomicU64) -> f64 {
    f64::from_bits(atomic.load(Ordering::Relaxed))
}

/// Intervallo di poll del thread: ogni ciclo rivaluta il target corrente
/// e completa quel che manca fino all'orizzonte di lookahead — una volta
/// raggiunto, i cicli successivi trovano tutto già in cache e tornano
/// subito, quindi un intervallo breve non ha un costo significativo.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Soglia di fallback per "oltre quanti frame di distanza in avanti
/// conviene un seek reale invece di continuare a decodificare in
/// sequenza", finché non c'è ancora nessuna osservazione reale del GOP
/// per quel media (vedi `OpenDecoder::seek_threshold_frames` — un numero
/// fisso uguale per un 4K long-GOP e un 1080p intra-friendly può sbagliare
/// di un ordine di grandezza in entrambe le direzioni, REFACTOR_PIPELINE.md
/// §1 A3). Basso e conservativo di proposito: sbagliare per eccesso (un
/// seek in più del necessario) costa poco, un seek riusa sempre il
/// decoder già aperto (`seek_to_time`), mai una riapertura.
const DEFAULT_SEEK_THRESHOLD_FRAMES: FrameIdx = 30;

/// Limite superiore per la stima del GOP osservato (vedi
/// `OpenDecoder::record_keyframe_landing`): due atterraggi da seek
/// consecutivi possono capitare per caso a più GOP di distanza (es. due
/// seek lontani, senza che nel mezzo sia mai capitato un atterraggio più
/// ravvicinato) — la stima è comunque un minimo che si stringe nel tempo,
/// questo limite serve solo a non lasciarla esplodere prima che arrivi
/// un'osservazione più stretta.
const MAX_SEEK_THRESHOLD_FRAMES: FrameIdx = 300;

/// Soglia usata al posto di quella imparata/di fallback quando il
/// decoder è aperto su un proxy (REFACTOR_PIPELINE.md proxy): i proxy
/// sono generati apposta interamente da keyframe (`g=1 keyint_min=1`,
/// vedi `vv_media::proxy::generate_proxy`), quindi *ogni* frame è
/// seekabile a costo pressoché nullo (osservato: seek riusati in
/// microsecondi, non millisecondi) — non c'è alcun GOP da imparare a
/// runtime, e usare comunque la stima adattiva (pensata per il sorgente
/// reale, dove un seek deve ripartire da un keyframe lontano) è
/// controproducente: durante uno scrub veloce e monotono i salti
/// osservati tra due seek reali sono sempre ampi (mai un atterraggio
/// ravvicinato che stringa la stima), quindi `estimated_gop` resta
/// bloccato su valori grandi (decine/centinaia di frame) e
/// `position_decoder` preferisce continuare a decodificare in sequenza
/// invece di seekare — proprio l'inverso di quel che conviene per
/// contenuto all-intra. Diagnosticato con un test di scrub aggressivo
/// (`VV_DEBUG_RENDER_AHEAD=1`): ogni ciclo del worker restava bloccato
/// 15-60ms a decodificare 30-75 frame "di troppo" prima di accorgersi
/// che la testina era già altrove, più del tempo tra due tick di uno
/// scrub veloce (quindi il worker non riusciva mai a recuperare mentre
/// lo scrub proseguiva). Piccola ma non nulla (non `0`): un frame di
/// margine evita un seek quando la testina è già esattamente lì.
const PROXY_SEEK_THRESHOLD_FRAMES: FrameIdx = 1;

enum Command {
    UpdateProject(Box<Project>, TimelineId),
    /// Toggle "usa proxy" (REFACTOR_PIPELINE.md proxy): passa dal
    /// canale comandi, non da un atomico come `target`/`cache_budget_bytes`,
    /// perché il worker deve reagire alla *transizione* — svuotare la
    /// cache condivisa (vedi `SharedFrameCache::clear`, i frame già
    /// cachati sotto le stesse chiavi potrebbero venire dalla
    /// risoluzione sbagliata) e i decoder aperti — non solo leggere un
    /// valore aggiornato al prossimo ciclo.
    SetProxyEnabled(bool),
    /// Nessun payload: fa uscire subito il worker dall'attesa su
    /// `recv_timeout` invece di aspettare fino a `POLL_INTERVAL` — vedi
    /// `RenderAhead::set_target`. Non richiede alcuna gestione speciale
    /// nei punti in cui i comandi vengono letti: riceverlo e basta fa
    /// proseguire il loop, che rilegge `target` come farebbe comunque
    /// al timeout naturale.
    Wake,
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

/// Handle `Arc` condivisi tra `RenderAhead` (thread UI) e il worker
/// (thread dedicato) — raggruppati per non far crescere il numero di
/// argomenti di `worker_loop` a ogni nuovo stato condiviso.
struct SharedState {
    caches: Arc<SharedFrameCache>,
    target: Arc<AtomicI64>,
    cache_budget_bytes: Arc<AtomicUsize>,
    caught_up: Arc<AtomicBool>,
    lookahead_secs: Arc<AtomicU64>,
    behind_secs: Arc<AtomicU64>,
}

/// Vedi il doc del modulo. Uno per `VibeVideoApp` (non uno per clip: la
/// differenza chiave rispetto al sistema precedente).
pub struct RenderAhead {
    caches: Arc<SharedFrameCache>,
    target: Arc<AtomicI64>,
    cache_budget_bytes: Arc<AtomicUsize>,
    /// `true` quando l'ultimo ciclo del worker ha trovato l'intera
    /// finestra di lookahead già in cache — vedi `WalkOutcome::caught_up`
    /// e `is_caught_up`. Parte da `false`: prima che il worker abbia
    /// completato almeno un ciclo non si sa ancora se c'è lavoro da fare.
    caught_up: Arc<AtomicBool>,
    /// Quanti secondi avanti/dietro la testina bufferizzare
    /// (`DEFAULT_LOOKAHEAD_SECS`/`DEFAULT_BEHIND_SECS` all'avvio,
    /// configurabile dall'utente — menu Playback > Proxy) — vedi
    /// `set_lookahead_secs`/`set_behind_secs`. Un semplice atomico come
    /// `target`, non un `Command`: a differenza del toggle proxy non
    /// serve reagire alla transizione con un effetto collaterale
    /// sincrono (svuotare la cache) — una finestra che si restringe
    /// lascia comunque scartare il resto al prossimo `reconcile`, una
    /// che si allarga lo riempie di nuovo da sé. Memorizzati come bit di
    /// un `f64` in un `AtomicU64` (`store_secs`/`load_secs`): niente
    /// atomic float nella std, e la precisione di un `f32`/frazione di
    /// frame non serve qui.
    lookahead_secs: Arc<AtomicU64>,
    behind_secs: Arc<AtomicU64>,
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
        let caches = Arc::new(SharedFrameCache::new());
        let target = Arc::new(AtomicI64::new(0));
        let budget = Arc::new(AtomicUsize::new(cache_budget_bytes));
        let caught_up = Arc::new(AtomicBool::new(false));
        let lookahead = Arc::new(AtomicU64::new(0));
        store_secs(&lookahead, lookahead_secs);
        let behind = Arc::new(AtomicU64::new(0));
        store_secs(&behind, behind_secs);
        let (tx, rx) = mpsc::channel();

        let thread_shared = SharedState {
            caches: caches.clone(),
            target: target.clone(),
            cache_budget_bytes: budget.clone(),
            caught_up: caught_up.clone(),
            lookahead_secs: lookahead.clone(),
            behind_secs: behind.clone(),
        };
        let handle = std::thread::spawn(move || {
            worker_loop(rx, thread_shared, project, timeline_id, proxy_enabled);
        });

        Self {
            caches,
            target,
            cache_budget_bytes: budget,
            caught_up,
            lookahead_secs: lookahead,
            behind_secs: behind,
            tx,
            handle: Some(handle),
        }
    }

    /// Aggiorna solo il target (playhead di timeline, in frame): chiamata
    /// economica da fare a ogni frame UI durante lo scrub/playback, sia
    /// per un avanzamento continuo sia per un salto — il worker rivaluta
    /// da zero la finestra a ogni ciclo, quindi non serve distinguere i
    /// due casi (a differenza di `DecodeAhead::set_target`/`seek`). Se il
    /// target è realmente cambiato (non la chiamata ridondante che
    /// `sync_render_ahead` fa comunque a ogni frame UI):
    ///
    /// - segna subito "non ancora bufferizzato": il worker lo
    ///   confermerà/correggerà al suo prossimo ciclo, ma senza questo
    ///   aggiornamento *immediato* la UI potrebbe leggere
    ///   `is_caught_up()` ancora `true` (stantio, da prima del cambio)
    ///   nell'unico repaint che segue subito l'interazione e smettere
    ///   di richiederne altri;
    /// - manda `Command::Wake` per far reagire il worker subito invece
    ///   di aspettare fino a `POLL_INTERVAL` (50ms) — con una finestra
    ///   ampia (`lookahead_secs` alto) quei 50ms sono invisibili, ma con
    ///   una finestra stretta (`lookahead_secs`/`behind_secs` bassi,
    ///   pavimentati a `MIN_MARGIN_FRAMES`) diventavano un tetto reale
    ///   alla fluidità del playback: il worker non produceva più di un
    ///   frame nuovo ogni 50ms, sotto qualunque framerate video comune
    ///   (bug segnalato dall'utente, playback a scatti anche a 1x con
    ///   una finestra ridotta al margine minimo, indipendente dal proxy).
    pub fn set_target(&self, frame: FrameIdx) {
        let previous = self.target.swap(frame, Ordering::Relaxed);
        if previous != frame {
            self.caught_up.store(false, Ordering::Relaxed);
            let _ = self.tx.send(Command::Wake);
        }
    }

    pub fn set_cache_budget_bytes(&self, bytes: usize) {
        self.cache_budget_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Quanti secondi di timeline bufferizzare in avanti dal playhead
    /// (menu Playback > Proxy) — vedi doc di `DEFAULT_LOOKAHEAD_SECS`.
    /// Qualunque valore, anche `0`, resta comunque pavimentato a
    /// `MIN_MARGIN_FRAMES` da `walk_and_fill`: vedi la sua doc sul
    /// perché un margine letteralmente nullo è strutturalmente fragile
    /// anche con la sveglia immediata (`set_target` → `Command::Wake`).
    /// Segna subito "non ancora bufferizzato": la finestra sta per
    /// cambiare dimensione, l'utente deve vedere la UI reagire.
    pub fn set_lookahead_secs(&self, secs: f64) {
        store_secs(&self.lookahead_secs, secs);
        self.caught_up.store(false, Ordering::Relaxed);
    }

    /// Quanti secondi di timeline bufferizzare anche *dietro* la testina
    /// (menu Playback > Proxy) — vedi doc di `DEFAULT_BEHIND_SECS`.
    pub fn set_behind_secs(&self, secs: f64) {
        store_secs(&self.behind_secs, secs);
        self.caught_up.store(false, Ordering::Relaxed);
    }

    /// Da chiamare dopo ogni comando che cambia la disposizione delle
    /// clip (ogni `history.do_command`): il worker lavora su una propria
    /// copia del progetto, non condivisa con la UI (`Project` è già
    /// `Clone`, stesso principio dello snapshot per l'export). Segna
    /// subito "non ancora bufferizzato" — stesso motivo di `set_target`,
    /// qui incondizionato perché ogni chiamata rappresenta per
    /// costruzione un cambio reale (il chiamante la fa solo a
    /// generazione della history diversa dall'ultima notificata, vedi
    /// `sync_render_ahead`).
    pub fn update_project(&self, project: &Project, timeline_id: TimelineId) {
        self.caught_up.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::UpdateProject(
            Box::new(project.clone()),
            timeline_id,
        ));
    }

    /// Toggle "usa proxy" (REFACTOR_PIPELINE.md proxy): il worker
    /// svuota la cache condivisa e riapre da zero ogni decoder sul path
    /// giusto per il nuovo stato — vedi doc di `Command::SetProxyEnabled`.
    pub fn set_proxy_enabled(&self, enabled: bool) {
        self.caught_up.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Command::SetProxyEnabled(enabled));
    }

    /// Il frame decodificato per `(media_id, source_frame)`, se già in
    /// cache.
    pub fn get_frame(&self, media_id: MediaId, source_frame: FrameIdx) -> Option<Arc<FrameYuv420>> {
        self.caches.get(media_id, source_frame)
    }

    /// `false` finché il worker ha ancora lavoro da fare per soddisfare
    /// la finestra di lookahead corrente (budget saturo, o semplicemente
    /// non ha ancora finito di decodificarla) — la UI lo usa per sapere
    /// se vale la pena richiedere un altro repaint pur di mostrare
    /// l'indicatore "buffered" avanzare, invece di aspettare che qualcos'
    /// altro lo faccia comunque (vedi il repaint in `VibeVideoApp::ui`).
    pub fn is_caught_up(&self) -> bool {
        self.caught_up.load(Ordering::Relaxed)
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

/// L'anteprima procura il frame dalla `SharedFrameCache` già riempita
/// in background dal worker — non bloccante, mai un errore reale (un
/// media che non decodifica viene semplicemente saltato dal worker, non
/// propagato qui: vedi `position_decoder`), quindi sempre `Ok`.
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

/// Decoder tenuto aperto per un media, con la posizione (frame sorgente)
/// che produrrà al prossimo `next_frame()`: permette di decidere se
/// conviene continuare a decodificare in sequenza o fare un seek reale
/// (vedi `position_decoder`). Uno per media (non uno slot condiviso): se
/// la finestra di lookahead attraversa un taglio tra due media diversi,
/// entrambi restano posizionati da un ciclo di poll all'altro invece di
/// essere riaperti ogni volta che il segmento "torna" al primo.
struct OpenDecoder {
    decoder: Decoder,
    /// Il path da cui questo decoder è stato aperto — confrontato a ogni
    /// ciclo (`position_decoder`) contro quello appena risolto per lo
    /// stesso media: se diverso (un proxy è appena diventato disponibile
    /// in background, o il toggle "usa proxy" è cambiato — REFACTOR_PIPELINE.md
    /// proxy), il decoder aperto sul path vecchio non ha alcun senso da
    /// riusare/seekare, va riaperto da zero sul nuovo indipendentemente
    /// da `needs_seek`.
    resolved_path: std::path::PathBuf,
    /// `true` quando `resolved_path` è un proxy (REFACTOR_PIPELINE.md
    /// proxy) — vedi `PROXY_SEEK_THRESHOLD_FRAMES` sul perché bypassa la
    /// stima adattiva del GOP invece di limitarsi a inizializzarla.
    is_all_intra: bool,
    next_frame: FrameIdx,
    /// `true` subito dopo un seek reale o la primissima apertura: il
    /// prossimo frame decodificato è per costruzione un keyframe (un
    /// seek/apertura atterra sempre lì, mai su un frame intermedio) —
    /// usato per imparare il GOP reale del media (vedi
    /// `record_keyframe_landing`), non solo per correggere il
    /// placeholder di `next_frame`. Azzerato non appena quel frame viene
    /// consumato, così non si scambia un frame qualunque nel mezzo del
    /// decode sequenziale per un keyframe.
    just_repositioned: bool,
    /// Ultimo atterraggio da seek osservato, e la stima del GOP derivata
    /// dalla distanza tra atterraggi consecutivi (vedi
    /// `record_keyframe_landing`). `None` finché non c'è ancora almeno
    /// un'osservazione — `seek_threshold_frames` usa un fallback in quel
    /// caso.
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

    /// Soglia oltre cui conviene un seek reale invece di continuare a
    /// decodificare in sequenza (REFACTOR_PIPELINE.md §3.3): circa un GOP
    /// del media *osservato*, non un numero fisso uguale per tutti i
    /// media — con un fallback conservativo finché non c'è ancora
    /// un'osservazione reale. Per un proxy (`is_all_intra`) niente stima
    /// da imparare: vedi doc di `PROXY_SEEK_THRESHOLD_FRAMES`.
    fn seek_threshold_frames(&self) -> FrameIdx {
        if self.is_all_intra {
            return PROXY_SEEK_THRESHOLD_FRAMES;
        }
        self.estimated_gop.unwrap_or(DEFAULT_SEEK_THRESHOLD_FRAMES)
    }

    /// Da chiamare con l'indice del primo frame decodificato dopo un
    /// seek/apertura reale (`just_repositioned`, azzerato dal
    /// chiamante subito dopo): aggiorna la stima del GOP dalla distanza
    /// rispetto all'ultimo atterraggio osservato. La stima è un
    /// *minimo* (mai più ampia di quella attuale, tranne alla prima
    /// osservazione) — un singolo salto accidentale di più GOP alla
    /// volta sovrastimerebbe, il minimo resta un limite superiore sicuro
    /// alla vera dimensione del GOP e si stringe man mano che arrivano
    /// atterraggi più ravvicinati; `MAX_SEEK_THRESHOLD_FRAMES` evita che
    /// nel frattempo resti sproporzionata.
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
        self.last_keyframe_landed = Some(idx);
    }
}

fn worker_loop(
    rx: mpsc::Receiver<Command>,
    shared: SharedState,
    mut project: Project,
    mut timeline_id: TimelineId,
    mut proxy_enabled: bool,
) {
    let SharedState {
        caches,
        target,
        cache_budget_bytes,
        caught_up,
        lookahead_secs,
        behind_secs,
    } = shared;
    let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
    // Decoder aperti per la finestra *dietro* la testina, separati da
    // `open`: vedi doc di `walk_and_fill` sul perché condividere lo
    // stesso decoder tra le due direzioni non funzionerebbe.
    let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
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
                    open_behind.clear();
                }
                Ok(Command::SetProxyEnabled(v)) => {
                    proxy_enabled = v;
                    caches.clear();
                    open.clear();
                    open_behind.clear();
                }
                Ok(Command::Wake) => {}
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
                    open_behind.clear();
                }
                Ok(Command::SetProxyEnabled(v)) => {
                    proxy_enabled = v;
                    caches.clear();
                    open.clear();
                    open_behind.clear();
                }
                Ok(Command::Wake) => {}
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
                    open_behind.clear();
                }
                Command::SetProxyEnabled(v) => {
                    proxy_enabled = v;
                    caches.clear();
                    open.clear();
                    open_behind.clear();
                }
                Command::Wake => {}
            }
        }

        let from = target.load(Ordering::Relaxed);
        let went_backward = last_from_frame.is_some_and(|last| from < last);
        last_from_frame = Some(from);
        let budget = cache_budget_bytes.load(Ordering::Relaxed);
        let lookahead = load_secs(&lookahead_secs);
        let behind = load_secs(&behind_secs);
        let outcome = walk_and_fill(
            &project,
            timeline_id,
            &caches,
            &mut open,
            &mut open_behind,
            from,
            budget,
            went_backward,
            proxy_enabled,
            lookahead,
            behind,
            &target,
        );
        retry_immediately = outcome.interrupted;
        caught_up.store(outcome.caught_up, Ordering::Relaxed);
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
/// clip Media attraversata sulla track video *attiva* (la più in alto tra
/// quelle video che coprono ciascun punto, `Timeline::active_video_clip_at`
/// — REFACTOR_PIPELINE.md B4: con più track video, quella "in cima" può
/// cambiare da un tratto all'altro, il loop lo scopre da sé rivalutando
/// ogni volta che avanza, nessun caso speciale in più): cammina quante clip
/// servono nella stessa passata, saltando vuoti (nessuna clip attiva,
/// quindi nessun segmento) e clip SolidColor (nessun decode necessario)
/// senza bisogno di casi speciali. Funzione pura, testabile senza ffmpeg.
fn collect_media_segments(
    timeline: &Timeline,
    from_frame: FrameIdx,
    end_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut segments = Vec::new();
    let mut frame = from_frame;
    while frame < end_frame {
        let next_frame = match timeline.active_video_clip_at(frame) {
            Some((_, clip)) => {
                let segment_end_timeline = clip.timeline_end().min(end_frame);
                if let ClipSource::Media(media_id) = &clip.source {
                    let media_id = *media_id;
                    // Mappatura clip→frame-sorgente condivisa con l'export
                    // (`vv_core::Clip::source_frame_at`, vedi doc lì per il
                    // perché — REFACTOR_PIPELINE.md B1).
                    let source_start = clip.source_frame_at(frame);
                    let source_end =
                        (clip.source_frame_at(segment_end_timeline) - 1).max(source_start);
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
                .next_video_clip_start_from(frame)
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

/// Simmetrico a `collect_media_segments`, ma all'indietro: divide
/// `[start_frame, from_frame)` in segmenti camminando la timeline verso
/// sinistra invece che verso destra — stessa identica logica (nessun
/// caso speciale per direzione: clip attiva -> segmento fino al suo
/// inizio, vuoto -> salta al bordo della clip precedente), solo con
/// `Timeline::previous_video_clip_end_before` al posto di
/// `next_video_clip_start_from`. La decodifica vera e propria di questi
/// segmenti resta comunque in avanti nello spazio *sorgente* (ffmpeg
/// decodifica solo in avanti): "all'indietro" qui si riferisce solo a
/// *dove* cade il segmento nello spazio timeline, non a come viene
/// prodotto — vedi il loop di fill in `walk_and_fill`, identico per i
/// segmenti in avanti e quelli dietro.
fn collect_media_segments_behind(
    timeline: &Timeline,
    from_frame: FrameIdx,
    start_frame: FrameIdx,
) -> Vec<MediaSegment> {
    let mut segments = Vec::new();
    let mut frame = from_frame;
    while frame > start_frame {
        let next_frame = match timeline.active_video_clip_at(frame - 1) {
            Some((_, clip)) => {
                let segment_start_timeline = clip.timeline_start.max(start_frame);
                if let ClipSource::Media(media_id) = &clip.source {
                    let media_id = *media_id;
                    let source_start = clip.source_frame_at(segment_start_timeline);
                    let source_end = (clip.source_frame_at(frame) - 1).max(source_start);
                    segments.push(MediaSegment {
                        media_id,
                        source_start,
                        source_end,
                        timeline_start: segment_start_timeline,
                    });
                }
                segment_start_timeline
            }
            None => timeline
                .previous_video_clip_end_before(frame)
                .map(|e| e.max(start_frame))
                .unwrap_or(start_frame),
        };
        if next_frame >= frame {
            break; // sicurezza: non dovrebbe succedere, evita un loop infinito
        }
        frame = next_frame;
    }
    segments
}

/// Spezza ogni segmento dietro la testina (`collect_media_segments_behind`,
/// già ordinati dal più vicino al più lontano) in blocchi da al più
/// `BEHIND_CHUNK_FRAMES`, ordinati anch'essi dal più vicino alla testina
/// al più lontano — vedi la doc di `BEHIND_CHUNK_FRAMES` sul perché.
/// `source_end` di un segmento è sempre il bordo adiacente alla testina
/// (`collect_media_segments_behind`, sia per il primo segmento che per
/// quelli oltre un taglio), quindi si parte da lì e si procede a ritroso
/// verso `source_start`. Mappatura `timeline_start` per offset (stesso
/// principio di `Clip::source_frame_at`: un clip non a velocità variabile
/// ha una corrispondenza 1:1 tra spostamento in spazio sorgente e in
/// spazio timeline, quindi il bordo sorgente di un blocco e il suo
/// corrispondente in timeline si spostano della stessa quantità rispetto
/// al segmento originale).
fn chunk_behind_segments_near_to_far(segments: &[MediaSegment]) -> Vec<MediaSegment> {
    let mut chunks = Vec::new();
    for segment in segments {
        let mut chunk_end = segment.source_end;
        loop {
            let chunk_start = (chunk_end - BEHIND_CHUNK_FRAMES + 1).max(segment.source_start);
            let offset = chunk_start - segment.source_start;
            chunks.push(MediaSegment {
                media_id: segment.media_id,
                source_start: chunk_start,
                source_end: chunk_end,
                timeline_start: segment.timeline_start + offset,
            });
            if chunk_start == segment.source_start {
                break;
            }
            chunk_end = chunk_start - 1;
        }
    }
    chunks
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
/// tolleranza (`OpenDecoder::seek_threshold_frames`) il target avanza oltre durante
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
    is_all_intra: bool,
) -> Positioned {
    // Un proxy appena diventato disponibile (o il toggle "usa proxy"
    // cambiato) fa risolvere un path diverso per lo stesso media: il
    // decoder aperto sul path vecchio va scartato, non riposizionato —
    // vedi doc di `OpenDecoder::resolved_path`.
    if open.get(&media_id).is_some_and(|o| o.resolved_path != path) {
        open.remove(&media_id);
    }
    if let Some(o) = open.get_mut(&media_id) {
        let needs_seek = segment_start > o.next_frame + o.seek_threshold_frames()
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
        // quel costo supera la soglia (`seek_threshold_frames`), il
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
        // decodifica sotto — che userà anche `just_repositioned` per
        // imparare il GOP reale da questo atterraggio (vedi
        // `OpenDecoder::record_keyframe_landing`).
        o.next_frame = 0;
        o.just_repositioned = true;
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
    open.insert(
        media_id,
        OpenDecoder::fresh(decoder, path.to_path_buf(), is_all_intra),
    );
    Positioned::Opened
}

/// Cammina `lookahead_secs` avanti da `from_frame` e riempie la
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
/// Esito di un ciclo di `walk_and_fill`, letto da `worker_loop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalkOutcome {
    /// La testina *live* (`target`) si è spostata abbastanza, mentre si
    /// decodificava, da rendere obsoleto il lavoro rimasto in questo
    /// ciclo — in quel caso il chiamante non aspetta il prossimo
    /// `POLL_INTERVAL`, rilegge subito il target fresco e ricomincia: un
    /// prefetch lontano non deve mai far aspettare la testina che si
    /// sposta nel frattempo (REFACTOR_PIPELINE.md §3.1).
    interrupted: bool,
    /// La finestra di lookahead richiesta in questo ciclo è ora
    /// interamente in cache (o non c'era nulla da bufferizzare):
    /// `false` se il budget globale ha fermato il fill a metà, o se
    /// `interrupted` ha troncato il giro — in entrambi i casi c'è ancora
    /// lavoro potenzialmente utile da fare al prossimo ciclo. Esposto
    /// alla UI via `RenderAhead::is_caught_up`: senza questo segnale, la
    /// UI non ha modo di sapere quando può smettere di richiedere
    /// repaint continui in attesa di vedere avanzare l'indicatore
    /// "buffered" (bug osservato: restava fermo finché non arrivava un
    /// repaint per qualche altro motivo, es. muovere il mouse — il
    /// worker bufferizza comunque, a un ritmo suo indipendente dalla UI,
    /// ma senza repaint quel progresso non veniva mai ridisegnato).
    caught_up: bool,
}

impl WalkOutcome {
    const SETTLED: Self = Self {
        interrupted: false,
        caught_up: true,
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
    // `lookahead_secs`/`behind_secs` sono configurabili dall'utente
    // (menu Playback > Proxy, anche `0`) ma restano sempre pavimentati a
    // `MIN_MARGIN_FRAMES` — vedi la sua doc sul perché un margine
    // letteralmente nullo è strutturalmente fragile anche con la
    // sveglia immediata su cambio target. Nessun caso speciale in più
    // nel resto della funzione, sono ancora `collect_media_segments`/
    // `collect_media_segments_behind` a decidere cosa c'è da fare, solo
    // con una finestra diversa in ingresso.
    let lookahead_frames = ((lookahead_secs * fps).round() as FrameIdx).max(MIN_MARGIN_FRAMES);
    let behind_frames = ((behind_secs * fps).round() as FrameIdx).max(MIN_MARGIN_FRAMES);
    let end_frame = from_frame + lookahead_frames;
    let start_frame = (from_frame - behind_frames).max(0);

    let forward_segments = collect_media_segments(timeline, from_frame, end_frame);
    let behind_segments = collect_media_segments_behind(timeline, from_frame, start_frame);
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
        })
        .collect();
    // Un solo pass di riconciliazione (vedi doc di `SharedFrameCache::
    // reconcile`): scarta ciò che è uscito da *entrambe* le finestre
    // (qualunque sia il motivo — media non più presente, oltre l'una o
    // l'altra estremità) e, se il budget globale condiviso non basta per
    // tutto ciò che è rimasto, sfratta il contenuto più lontano dalla
    // testina (che sia in avanti o dietro, la distanza è simmetrica).
    // Sostituisce insieme retain/evict_before/sfratto-per-capacità di
    // prima.
    caches.reconcile(from_frame, &window, cache_budget_bytes);

    let ctx = FillContext {
        project,
        caches,
        went_backward,
        cache_budget_bytes,
        from_frame,
        proxy_enabled,
        target,
    };
    // In avanti prima, sempre: il frame che serve ORA per non fermare la
    // riproduzione ha sempre priorità sul buffer dietro la testina, che è
    // solo una comodità per uno scrub avanti-indietro ravvicinato (vedi
    // doc di `DEFAULT_BEHIND_SECS`) — se il budget si esaurisce già qui, dietro
    // non riceve nulla in questo ciclo, correttamente. Qui `source_start`
    // di ogni segmento È già la testina (o il bordo del blocco
    // precedente), quindi un solo seek per segmento decodifica già dal
    // più vicino al più lontano — nessun bisogno di spezzarla in blocchi
    // come sotto.
    if let ControlFlow::Break(outcome) = fill_segments(&forward_segments, &ctx, open) {
        return outcome;
    }
    // Decoder *separato* da quello della finestra in avanti (vedi
    // `open_behind`, mappa a parte passata dal chiamante): usare lo
    // stesso decoder per entrambe le direzioni lo lascerebbe posizionato
    // in avanti dopo il fill sopra, facendo scattare
    // `od.next_frame > segment.source_end` qui sotto per *qualunque*
    // segmento dietro la testina dello stesso media — saltandolo sempre
    // in silenzio, anche la primissima volta che quella zona va davvero
    // decodificata (non è mai stato un problema di posizionamento, è che
    // le due finestre vogliono il decoder in due punti diversi nello
    // stesso momento).
    //
    // Spezzata in blocchi dal più vicino al più lontano
    // (`chunk_behind_segments_near_to_far`, vedi doc di
    // `BEHIND_CHUNK_FRAMES`): un blocco lontano ha sempre `source_start`
    // *dietro* a dove il decoder è appena arrivato (il blocco appena
    // prima, più vicino) — un seek reale ci vuole sempre, a prescindere
    // da quanto la testina *globale* si sia mossa (`went_backward` del
    // ciclo intero non c'entra qui), quindi `went_backward: true` forzato
    // solo per questa chiamata: `position_decoder` lo richiede per
    // riconoscere "questo decoder ha superato la nuova posizione" quando
    // la distanza rientra comunque nella soglia adattiva (vedi la sua
    // doc). Il primo blocco (il più vicino) resta comunque `Reused`
    // quando il decoder era già lì da un ciclo precedente: la condizione
    // in `position_decoder` scatta solo se il decoder è *già oltre*
    // l'inizio del blocco richiesto, mai per un decoder ancora dietro.
    let behind_chunks = chunk_behind_segments_near_to_far(&behind_segments);
    let behind_ctx = FillContext {
        went_backward: true,
        ..ctx
    };
    if let ControlFlow::Break(outcome) = fill_segments(&behind_chunks, &behind_ctx, open_behind) {
        return outcome;
    }
    WalkOutcome::SETTLED
}

/// Parametri di `fill_segments` che non cambiano tra la chiamata per la
/// finestra in avanti e quella per la finestra dietro la testina —
/// raggruppati per non far crescere il numero di argomenti della
/// funzione a ogni nuovo parametro condiviso.
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

/// Nucleo del fill condiviso da finestra in avanti e finestra dietro la
/// testina (vedi doc di `walk_and_fill`): decodifica quanto serve per
/// coprire `segments`, usando/aggiornando i decoder aperti in `open`,
/// fermandosi se il budget globale è saturo o se la testina *live* si è
/// spostata troppo per rendere utile continuare. `ControlFlow::Continue`
/// se ha processato tutti i segmenti senza interruzioni; `Break` porta
/// già l'esito finale che `walk_and_fill` deve restituire al suo
/// chiamante.
fn fill_segments(
    segments: &[MediaSegment],
    ctx: &FillContext,
    open: &mut HashMap<MediaId, OpenDecoder>,
) -> ControlFlow<WalkOutcome> {
    for segment in segments {
        let Some(item) = ctx.project.media_pool.get(segment.media_id) else {
            continue;
        };
        // Proxy solo se il toggle è attivo *e* quello per questo media
        // è già pronto (REFACTOR_PIPELINE.md proxy) — il caso "toggle
        // attivo ma proxy non ancora generato in background" ricade sul
        // sorgente originale senza bisogno di un caso a parte: appena
        // `generate_proxy` finisce (thread separato, vedi `main.rs`),
        // il prossimo ciclo lo trova su disco e ci passa da sé (il
        // confronto path in `position_decoder` se ne accorge).
        let is_proxy = ctx.proxy_enabled && vv_media::proxy::proxy_exists(item.content_hash);
        let path = if is_proxy {
            vv_media::proxy::proxy_path_for(item.content_hash)
        } else {
            item.path.clone()
        };
        if position_decoder(
            open,
            segment.media_id,
            &path,
            segment.source_start,
            ctx.went_backward,
            is_proxy,
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
                && ctx.caches.contains(segment.media_id, od.next_frame)
                && ctx.caches.contains(segment.media_id, segment.source_end)
            {
                break;
            }
            // Budget globale saturo: i segmenti restanti (questo incluso,
            // da qui in poi) sono per costruzione più lontani dalla
            // testina di tutto ciò che è già in cache (fill in ordine di
            // priorità, vedi doc della funzione) — non c'è nulla da
            // guadagnare continuando, esce dall'intero giro sui segmenti,
            // non solo da questo.
            if ctx.caches.bytes_used() >= ctx.cache_budget_bytes {
                return ControlFlow::Break(WalkOutcome {
                    interrupted: false,
                    caught_up: false,
                });
            }
            let mut threshold_frames = od.seek_threshold_frames();
            match od.decoder.next_frame() {
                Ok(Some((idx, frame))) => {
                    // Primo frame dopo un seek/apertura reale: è per
                    // costruzione un keyframe, registra l'atterraggio per
                    // affinare la stima del GOP di questo media (vedi
                    // `OpenDecoder::record_keyframe_landing`) prima che il
                    // resto del loop possa scambiarlo per un frame
                    // qualunque nel mezzo del decode sequenziale.
                    if od.just_repositioned {
                        od.record_keyframe_landing(idx);
                        od.just_repositioned = false;
                        threshold_frames = od.seek_threshold_frames();
                    }
                    // `insert` sovrascrive innocuamente se `idx` è già
                    // presente (decoder ripartito da un keyframe
                    // precedente al punto richiesto): evita solo un ramo
                    // che avanzi `next_frame` senza consumare davvero un
                    // frame dal decoder (in passato causa di un
                    // disallineamento tra la posizione tracciata e quella
                    // reale).
                    ctx.caches.insert(segment.media_id, idx, Arc::new(frame));
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
            // con `position_decoder` (`OpenDecoder::seek_threshold_frames`,
            // adattiva sul GOP osservato — REFACTOR_PIPELINE.md §3.3):
            // sotto quella distanza il lavoro in corso è ancora utile
            // (drift normale di riproduzione), sopra è uno scrub/salto
            // che rende la finestra corrente stale.
            let live = ctx.target.load(Ordering::Relaxed);
            if (live - ctx.from_frame).abs() > threshold_frames {
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
        };

        let chunks = chunk_behind_segments_near_to_far(&[segment]);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].source_start, 40);
        assert_eq!(chunks[0].source_end, 44);
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

        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 0, false, false),
            Positioned::Opened
        );

        assert_eq!(
            position_decoder(&mut open, media_a, &path, 1000, false, false),
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

        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&mut open, media_a, &path_a, 0, false, false),
            Positioned::Opened
        );

        // Stessa posizione richiesta (0): senza il controllo sul path
        // risolto, `needs_seek` sarebbe `false` (0 non è "troppo avanti"
        // rispetto a un decoder appena aperto) e la chiamata
        // restituirebbe `Reused` — riusando un decoder che punta al file
        // sbagliato.
        assert_eq!(
            position_decoder(&mut open, media_a, &path_b, 0, false, false),
            Positioned::Opened,
            "il path è cambiato: deve riaprire sul nuovo, non riusare il decoder del vecchio"
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

        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 0, false, false),
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
            position_decoder(&mut open, media_a, &path, 0, false, false),
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

        // [0,63], non [(60,63)]: con keyint di default (250) e una clip
        // di soli 100 frame, l'unico keyframe è a 0 — raggiungere il
        // frame 63 (= 60 + MIN_MARGIN_FRAMES) richiede comunque di
        // decodificare in sequenza da lì (nessun modo di saltare i
        // frame intermedi), e quei frame restano in cache perché
        // genuinamente già decodificati. Quel che conta è che la
        // finestra *non vada oltre* 63 (con `lookahead_secs` normale
        // arriverebbe fino a 99, la fine della clip — vedi il test
        // sopra): configurato a zero secondi, resta comunque il margine
        // minimo, non letteralmente nulla.
        assert_eq!(
            caches.cached_ranges(media_a),
            vec![(0, 60 + MIN_MARGIN_FRAMES - 1)],
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
        let mut open_behind: HashMap<MediaId, OpenDecoder> = HashMap::new();
        // Capacità ~60 frame YUV420 (width*height*3/2 byte/frame, non
        // più i 4 byte/pixel RGBA da prima di REFACTOR_PIPELINE.md B3):
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
    ///
    /// Gap tra i due segmenti (10 e 25, non 10 e 50 come in una versione
    /// precedente di questo test) scelto apposta sotto
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
        let mut open: HashMap<MediaId, OpenDecoder> = HashMap::new();

        // Ciclo 1: due segmenti dello stesso media nella stessa finestra
        // (come ai due lati di un taglio), source_start 10 e poi 25.
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 10, false, false),
            Positioned::Opened
        );
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 25, false, false),
            Positioned::Reused,
            "nello stesso ciclo il secondo segmento non deve mai richiedere un seek: il decoder è già lì"
        );

        // Ciclo 2, testina ferma (`went_backward=false` per entrambi):
        // stessi due segmenti. Rielaborare il *primo* segmento (10) non
        // deve sembrare "tornato indietro" solo perché l'ultima chiamata
        // vista nel ciclo precedente era per il segmento successivo (25).
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 10, false, false),
            Positioned::Reused,
            "testina ferma: rielaborare il primo segmento non deve scatenare un seek reale"
        );
        assert_eq!(
            position_decoder(&mut open, media_a, &path, 25, false, false),
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 150)],
            muted: false,
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
    /// stretta (25ms, metà del vecchio `POLL_INTERVAL`) *dopo* aver
    /// aspettato che il worker si stabilizzi sul target precedente —
    /// senza sveglia immediata il worker rilegge `target` solo a un
    /// prossimo ciclo di poll fisso, che cade in un istante scorrelato
    /// da quando `set_target` è stato chiamato: il tempo di attesa
    /// sarebbe distribuito uniformemente tra 0 e 50ms, quindi ripetuto
    /// su più salti indipendenti la probabilità che *tutti* restino
    /// sotto i 25ms per puro caso crolla rapidamente (verificato:
    /// disabilitando temporaneamente l'invio di `Command::Wake` questo
    /// test fallisce in modo riproducibile).
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        });
        let timeline_id = project.timelines.insert(timeline_with(vec![Track {
            kind: TrackKind::Video,
            clips: vec![media_clip(1, media_a, 0, 50)],
            muted: false,
        }]));

        let render_ahead = RenderAhead::spawn(project, timeline_id, 100_000_000, false, 0.0, 0.0);

        let wait_for = |frame: FrameIdx| {
            let start = std::time::Instant::now();
            loop {
                if render_ahead.get_frame(media_a, frame).is_some() {
                    return start.elapsed();
                }
                assert!(
                    start.elapsed() < Duration::from_millis(25),
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

        // Da qui in poi, ogni salto isolato ha 25ms per essere pronto.
        for target in [10, 20, 30, 40] {
            render_ahead.set_target(target);
            wait_for(target);
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
    /// deve essere ridecodificata — solo il singolo frame inevitabile
    /// (il keyframe su cui il seek atterra: senza deciderlo *ancora*
    /// dopo il seek, vedi doc di `position_decoder`). Con la finestra di
    /// retention dietro la testina, lo scrub di 30 frame qui sotto
    /// ricade interamente *dentro* quella finestra (50 frame): il
    /// tratto [250,269] non viene nemmeno scartato da `reconcile`, la
    /// riconnessione scatta appena il decoder atterra sul keyframe 250 e
    /// decodifica quel singolo frame — non deve proseguire fino a 270
    /// come nella versione senza retention. Verificato osservando
    /// `OpenDecoder::next_frame` dopo il seek: deve fermarsi a 251
    /// (keyframe 250 decodificato + 1), non continuare a ridecodificare
    /// quel che è già lì.
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
            next_frame_after, 251,
            "deve fermarsi appena si ricongiunge con il buffer esistente (251, subito dopo il keyframe 250 su cui il seek atterra), non ridecodificare oltre: next_frame={next_frame_after}"
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
