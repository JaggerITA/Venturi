//! Finestra egui: media pool (sinistra), viewer (centro), timeline
//! multi-traccia (basso), toolbar con play/pause/seek per l'anteprima.
//!
//! Il viewer mostra, per ogni frame, la clip Media attiva sulla track
//! video più in alto tra quelle che ne hanno una in quel punto
//! (`vv_core::Timeline::active_video_clip_at` — con più track video
//! sovrapposte, N tracce reali, non solo la track 0 fissa,
//! REFACTOR_PIPELINE.md B4), con crop/zoom/gain (milestone 5, valori
//! statici) applicati tramite il compositor GPU di `vv-render`, texture
//! GPU passata direttamente a egui-wgpu senza round-trip CPU
//! (REFACTOR_PIPELINE.md B2). Non è ancora un vero accumulo alpha
//! multi-layer: senza un'opacità per-clip ancora modellata, "compositing
//! bottom->top" collassa in "la track più in alto che copre quel punto
//! vince" — vedi doc di `active_video_clip_at`.

mod export;
mod frame_provider;
mod mix_buffers;
mod proxy_worker;
mod render_ahead;
mod timeline_audio;
mod timeline_ui;
mod waveform_worker;

use eframe::wgpu;
use frame_provider::FrameProvider;
use timeline_audio::TimelineAudio;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use vv_core::{ClipId, FrameIdx, MediaId, TimelineId, Track, TrackKind};

/// Default di `VibeVideoApp::cache_budget_bytes`: ~151 frame (~6s) di
/// margine a 1080p, ~38 (~1,5s) a 4K, ~340 (~13,6s) a 720p — vedi doc del
/// campo per il perché è un budget di memoria e non un conteggio fisso di
/// frame.
const DEFAULT_CACHE_BUDGET_BYTES: usize = 1_200_000_000;

/// Un'"unità" da rimuovere con un solo `RippleDeleteAllTracks` in
/// `ripple_delete_selected`: la clip primaria (track, id), il suo
/// `timeline_start` (per ordinare le unità da destra a sinistra) e le
/// clip da rimuovere con lei senza shiftarle (la sua gemella collegata,
/// se c'è).
type RippleUnit = ((usize, ClipId), FrameIdx, Vec<(usize, ClipId)>);

/// Dopo quanto (s) una freccia tenuta premuta smette di fare il passo
/// singolo e inizia a scorrere a `ARROW_HOLD_SPEED`.
const ARROW_HOLD_DELAY_SECS: f64 = 0.3;
const ARROW_HOLD_SPEED: f64 = 0.5;

/// Freccia sinistra/destra tenuta premuta (`VibeVideoApp::arrow_hold`).
struct ArrowHold {
    direction: FrameIdx,
    pressed_at: f64,
    start_frame: FrameIdx,
}

/// Posizione della testina con una freccia premuta da `elapsed` secondi:
/// un frame subito, poi scorrimento continuo dopo `ARROW_HOLD_DELAY_SECS`.
fn arrow_hold_target(hold: &ArrowHold, elapsed: f64, fps: f64) -> FrameIdx {
    let scrolled = ((elapsed - ARROW_HOLD_DELAY_SECS).max(0.0) * fps * ARROW_HOLD_SPEED) as FrameIdx;
    (hold.start_frame + hold.direction * (1 + scrolled)).max(0)
}

/// Le due velocità di riproduzione accelerata raggiungibili col tasto "a"
/// (vedi `VibeVideoApp::handle_fast_playback_key`) — non un `f64` libero:
/// solo questi due fattori vengono mai richiesti a `vv_audio::stretch_samples`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpeedTier {
    X2,
    X4,
    X8,
}

impl SpeedTier {
    fn tempo(self) -> f64 {
        match self {
            SpeedTier::X2 => 2.0,
            SpeedTier::X4 => 4.0,
            SpeedTier::X8 => 8.0,
        }
    }

    /// Il tier raggiunto premendo "a" un'altra volta rispetto a questo —
    /// vedi `VibeVideoApp::handle_fast_playback_key`. Resta a X8 oltre.
    fn next(self) -> Self {
        match self {
            SpeedTier::X2 => SpeedTier::X4,
            SpeedTier::X4 | SpeedTier::X8 => SpeedTier::X8,
        }
    }

    fn from_multiplier(speed: f64) -> Option<Self> {
        if (speed - 2.0).abs() < 1e-9 {
            Some(SpeedTier::X2)
        } else if (speed - 4.0).abs() < 1e-9 {
            Some(SpeedTier::X4)
        } else if (speed - 8.0).abs() < 1e-9 {
            Some(SpeedTier::X8)
        } else {
            None
        }
    }
}

/// Snapshot dei campi della clip selezionata che servono al pannello
/// proprietà, valutati al `source_frame` corrente. Una struct invece di
/// una tupla perché i campi hanno continuato a crescere con ogni nuova
/// proprietà keyframeable (transform, gain, ora color).
struct ClipPanelInfo {
    start: FrameIdx,
    len: FrameIdx,
    is_solid_color: bool,
    transform_constant: bool,
    transform_kf_here: bool,
    transform: vv_core::Transform,
    gain_constant: bool,
    gain_kf_here: bool,
    gain: f32,
    color_constant: bool,
    color_kf_here: bool,
    color: vv_core::Rgba,
}

/// Azione differita sugli effetti di una clip, raccolta durante il disegno
/// del pannello proprietà (che prende in prestito `self` immutabilmente) e
/// applicata subito dopo — stesso schema di `timeline_ui::PendingAction`.
enum PendingEffectChange {
    SetTransformDefault(usize, ClipId, vv_core::Transform),
    SetGainDefault(usize, ClipId, f32),
    SetColorDefault(usize, ClipId, vv_core::Rgba),
    UpsertTransformKeyframe(usize, ClipId, FrameIdx, vv_core::Transform),
    UpsertGainKeyframe(usize, ClipId, FrameIdx, f32),
    UpsertColorKeyframe(usize, ClipId, FrameIdx, vv_core::Rgba),
    RemoveTransformKeyframe(usize, ClipId, FrameIdx),
    RemoveGainKeyframe(usize, ClipId, FrameIdx),
    RemoveColorKeyframe(usize, ClipId, FrameIdx),
}

/// Quale rappresentazione di texture del viewer è quella corrente — vedi
/// doc di `VibeVideoApp::last_viewer_frame_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewerFrameKind {
    SolidColor,
    Video,
}

struct VibeVideoApp {
    project: vv_core::Project,
    history: vv_core::History,
    timeline_id: Option<TimelineId>,
    timeline_state: timeline_ui::TimelineState,
    import_error: Option<String>,

    preview_meta: Option<vv_core::MediaMeta>,
    preview_error: Option<String>,
    /// Texture per il frame di colore solido (clip SolidColor, o vuoto):
    /// gestita da egui (`ctx.load_texture`/`TextureHandle::set`) — non fa
    /// parte del round-trip GPU eliminato dal path video sotto, è
    /// un'immagine sintetica generata su CPU, niente da guadagnare a
    /// tenerla sulla GPU (REFACTOR_PIPELINE.md B2).
    frame_texture: Option<egui::TextureHandle>,
    /// Id stabile della texture video registrata in `egui-wgpu`
    /// (REFACTOR_PIPELINE.md B2): creato una volta al primo frame video
    /// (`register_native_texture`), poi solo aggiornato in-place
    /// (`update_egui_texture_from_wgpu_texture`) — mai una nuova
    /// registrazione a ogni frame, che perderebbe il riferimento alla
    /// precedente (bind group + texture GPU, mai liberata) invece di
    /// riusarlo. `None` se l'app non ha un device wgpu condiviso con
    /// egui (`egui_render_state`, es. nei test) — in quel caso il video
    /// non può essere mostrato zero-copy.
    video_texture_id: Option<egui::TextureId>,
    video_display_size: Option<egui::Vec2>,
    /// Quale delle due rappresentazioni sopra (`frame_texture` per il
    /// colore solido, `video_texture_id`+`video_display_size` per il
    /// video) è quella da mostrare adesso — nessuna delle due viene
    /// azzerata quando non si aggiorna in un dato frame (per continuare a
    /// mostrare l'ultimo frame valido invece di un flash a vuoto, vedi
    /// `preview_media`), quindi il viewer deve sapere quale delle due è
    /// la più recente.
    last_viewer_frame_kind: Option<ViewerFrameKind>,
    /// Device/queue/renderer condivisi con `egui-wgpu`, se l'app è stata
    /// avviata da `main()` con un contesto eframe reale (sempre, tranne
    /// nei test che costruiscono `VibeVideoApp` con `Default` senza una
    /// finestra) — necessari per registrare/aggiornare
    /// `video_texture_id` (REFACTOR_PIPELINE.md B2). Anche
    /// `self.compositor` viene costruito condividendo questo stesso
    /// device quando presente (vedi `main()`), altrimenti resta il
    /// device headless indipendente di prima.
    egui_render_state: Option<eframe::egui_wgpu::RenderState>,
    /// Decode-ahead video per l'anteprima "grezza" di un media dal media
    /// pool (`browsing_media`), non legata a nessuna clip/posizione di
    /// timeline a cui `render_ahead` potrebbe agganciarsi. Il video delle
    /// clip *sulla* timeline viene invece da `render_ahead`, sempre.
    browsing_decode_ahead: Option<vv_media::DecodeAhead>,

    /// Budget di memoria (byte) per la cache dei frame decodificati di
    /// *ogni* decode-ahead aperto (vedi doc di `DecodeAhead::spawn`):
    /// configurabile dall'utente nel menu "Visualizza" invece di una
    /// costante fissa nel codice, perché quanta RAM vale la pena dedicare
    /// a un margine di riproduzione fluida contro OOM dipende
    /// dall'hardware/uso dell'utente, non da una scelta valida per tutti.
    cache_budget_bytes: usize,

    /// Toggle "usa proxy" (menu "Visualizza", REFACTOR_PIPELINE.md
    /// proxy): quando attivo, l'anteprima/editing decodifica dal proxy
    /// tutto-intra a bassa risoluzione invece che dal sorgente, se già
    /// generato — elimina il costo "cammina dal keyframe più vicino" che
    /// rende lo scrub veloce impossibile su sorgenti long-GOP (misurato:
    /// 0 frame esatti disponibili durante uno scrub veloce a 1080p,
    /// senza proxy). L'export ignora sempre questo toggle: usa solo i
    /// sorgenti originali, mai il proxy. Attivo di default; da
    /// disattivare per lavori che richiedono la qualità piena (color
    /// grading — non ancora implementato — o verificare dettagli fini).
    /// Non persiste tra un riavvio e l'altro, come ogni altra
    /// impostazione in vibevideo oggi.
    proxy_enabled: bool,
    /// Genera in background il proxy di ogni media importato (vedi
    /// `vv_media::proxy`): sempre attivo indipendentemente da
    /// `proxy_enabled`, così un proxy è già pronto appena l'utente
    /// riattiva il toggle, invece di aspettare la prima volta che serve
    /// davvero. `None` finché non è mai stato importato nulla (spawnato
    /// alla prima `import_media`, non subito: niente thread in più per
    /// una sessione che non importa mai media).
    proxy_worker: Option<proxy_worker::ProxyWorker>,
    /// Genera in background la waveform (picchi audio) di ogni media
    /// importato con audio (vedi `vv_media::waveform`): sempre attivo,
    /// come il proxy, così la waveform è già pronta appena la timeline
    /// la disegna. `None` finché non è mai stato importato nulla (stesso
    /// principio di `proxy_worker`).
    waveform_worker: Option<waveform_worker::WaveformWorker>,
    /// Waveform audio già caricata in memoria, a chiave `content_hash` del
    /// media: la timeline la legge a ogni frame per disegnare la waveform
    /// delle clip audio, e il primo disegno di un media la carica dal
    /// file di cache (`vv_media::waveform::load_waveform`) se il worker
    /// l'ha già generata. In memoria (non riletta da disco a ogni frame)
    /// perché la timeline si ridisegna a ogni repaint e un `read` di un
    /// file da qualche MB a ogni frame sarebbe un I/O inutile; il file
    /// resta la fonte di verità (sopravvive al riavvio), la mappa è solo
    /// una cache della sessione. Il `Waveform` porta anche la durata della
    /// traccia audio: il disegno mappa i bin della clip sulla stessa base
    /// temporale dei picchi (vedi `draw_clip_waveform`). Chiave
    /// `(content_hash, stream_index)`: un media può avere più stream audio
    /// (vedi `Clip::audio_stream_index`), ciascuno con la propria waveform.
    waveform_cache: HashMap<(u64, usize), vv_media::Waveform>,
    /// Quanti secondi di timeline bufferizzare in anticipo avanti/dietro
    /// la testina (menu Playback > Proxy, dove vive anche il toggle
    /// proxy) — vedi doc di `render_ahead::DEFAULT_LOOKAHEAD_SECS`/
    /// `DEFAULT_BEHIND_SECS`. Configurabile perché il bilanciamento
    /// giusto dipende da quanto è pesante il sorgente/proxy e da quanta
    /// RAM l'utente vuole dedicarci — un valore fisso per tutti
    /// avrebbe sempre sbagliato in una direzione o nell'altra. Restano
    /// comunque pavimentati a `render_ahead::MIN_MARGIN_FRAMES` anche se
    /// l'utente li porta a `0` (vedi la sua doc sul perché un margine
    /// letteralmente nullo è strutturalmente fragile). Non persistono
    /// tra un riavvio e l'altro, come ogni altra impostazione in
    /// vibevideo oggi.
    lookahead_secs: f64,
    behind_secs: f64,

    /// Clip la cui anteprima è attualmente mostrata: guida sia il player
    /// (quale media riprodurre) sia il transform/gain applicati (milestone
    /// 5). `None` se non c'è ancora una clip selezionata.
    active_clip: Option<(usize, ClipId)>,
    compositor: vv_render::Compositor,

    /// Ultimo `timeline_state.playhead` già gestito da
    /// `ensure_active_clip_matches_playhead`: usato per distinguere "il
    /// player sta scrivendo il playhead lui stesso durante il playback"
    /// (nessun seek da fare, lo fa già avanzare la decodifica) da "l'utente
    /// ha trascinato il playhead da fermo" (serve un seek esplicito).
    last_synced_playhead: FrameIdx,

    /// Media aperto tramite il pulsante "Anteprima" del media pool (non
    /// ancora/non necessariamente sulla timeline): mentre è `Some`, il
    /// viewer mostra quel media al posto di quello guidato dal playhead, e
    /// `active_clip` resta `None` (nessun transform/gain di una clip si
    /// applica a un'anteprima "grezza"). Si esce da questa modalità
    /// interagendo con la timeline (selezione o playhead).
    browsing_media: Option<MediaId>,

    /// Buffer video a livello di timeline: bufferizza N secondi avanti
    /// dal playhead attraversando quante clip servono (vedi doc del
    /// modulo `render_ahead`), invece di un preload per singola clip.
    /// `None` finché non esiste ancora una timeline (stesso principio di
    /// `timeline_id`), spawnato la prima volta in `ensure_timeline`.
    render_ahead: Option<render_ahead::RenderAhead>,
    /// `history.generation()` all'ultima notifica a `render_ahead` di una
    /// nuova disposizione delle clip: un confronto di interi a ogni frame
    /// UI basta a sapere se serve rimandargli una copia del progetto,
    /// senza dover clonare/diffare `Project` a ogni frame per scoprirlo.
    render_ahead_generation: u64,

    /// Mixer delle track audio e clock del playback della timeline.
    /// `None` finché non serve (nei test si apre solo se usato).
    timeline_audio: Option<TimelineAudio>,

    /// Moltiplicatore di velocità (1/2/4/8x) impostato dal tasto "a"; la
    /// barra spaziatrice mette in pausa e lo riporta a 1x.
    playback_speed: f64,

    /// "Selection follows playhead": attiva di default, disattivabile
    /// dalle impostazioni. Quando attiva, spostare il playhead (scrub o
    /// click sul righello) o tagliare/eliminare seleziona automaticamente
    /// la clip sulla track video sotto al playhead — comodo per fare più
    /// tagli/ripple-delete in rapida successione senza dover ricliccare
    /// ogni volta la clip.
    selection_follows_playhead: bool,

    /// Audio durante lo scrub (menu Timeline): attivo di default.
    scrub_audio: bool,

    arrow_hold: Option<ArrowHold>,

    /// Il pannello proprietà (a destra del viewer) è visibile? Attivo di
    /// default; l'utente può nasconderlo (✕ nel pannello, o la checkbox in
    /// toolbar) e farlo ricomparire al bisogno. Da non confondere con "non
    /// c'è nulla di selezionato": in quel caso il pannello resta visibile
    /// ma mostra informazioni sulla timeline invece che su una clip.
    properties_panel_open: bool,

    /// Calamita (toggle nella barra sotto il player): attiva di default,
    /// come nella maggior parte degli NLE. Quando attiva, trascinare una
    /// clip sulla timeline (o piazzarne una nuova dal media pool) scatta
    /// sui bordi delle clip vicine entro una piccola soglia in pixel — vedi
    /// `timeline_ui::snap_frame`.
    snapping_enabled: bool,

    /// Export in corso (milestone 9), se c'è: `None` quando nessun export
    /// è attivo. Il thread lavora su uno snapshot di `Project` clonato al
    /// click di "Esporta", non sul progetto live — vedi `export.rs`.
    export: Option<ExportUiState>,

    /// File del progetto corrente (milestone 10), se già salvato/aperto
    /// almeno una volta: "Salva" scrive lì direttamente, altrimenti si
    /// comporta come "Salva con nome...".
    current_project_path: Option<PathBuf>,
    /// Ultimo errore di salvataggio/apertura progetto, mostrato in
    /// toolbar accanto ai pulsanti — separato da `import_error` (quello è
    /// per l'import media, contesto diverso).
    project_error: Option<String>,

    /// Audiometer (toggle in Visualizza): una fascia stretta a destra
    /// della timeline con il livello del player attivo. Attivo di
    /// default, come nella maggior parte degli NLE.
    audiometer_enabled: bool,
    /// Valori (sinistra, destra) mostrati dal meter stereo, con un
    /// decadimento applicato qui (non nel callback audio): il picco letto
    /// da `TimelineAudio::peak_linear_stereo` è istantaneo, senza smorzamento
    /// scenderebbe a zero non appena il buffer corrente non contiene
    /// picchi, con un effetto "a scatti" invece di due barre che scendono
    /// dolcemente.
    audiometer_level: (f32, f32),
}

/// Stato UI di un export in corso: progresso/cancellazione condivisi col
/// thread che sta effettivamente esportando (`export::export_timeline`),
/// più l'handle per recuperarne l'esito a fine corsa.
struct ExportUiState {
    progress: std::sync::Arc<Mutex<export::ExportProgress>>,
    cancel: std::sync::Arc<AtomicBool>,
    handle: std::thread::JoinHandle<Result<(), String>>,
}

impl Default for VibeVideoApp {
    fn default() -> Self {
        Self {
            project: vv_core::Project::default(),
            history: vv_core::History::default(),
            timeline_id: None,
            timeline_state: timeline_ui::TimelineState::default(),
            import_error: None,
            preview_meta: None,
            preview_error: None,
            frame_texture: None,
            video_texture_id: None,
            video_display_size: None,
            last_viewer_frame_kind: None,
            egui_render_state: None,
            browsing_decode_ahead: None,
            cache_budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
            proxy_enabled: true,
            proxy_worker: None,
            waveform_worker: None,
            waveform_cache: HashMap::new(),
            lookahead_secs: render_ahead::DEFAULT_LOOKAHEAD_SECS,
            behind_secs: render_ahead::DEFAULT_BEHIND_SECS,
            active_clip: None,
            compositor: vv_render::Compositor::new_headless(),
            last_synced_playhead: 0,
            browsing_media: None,
            render_ahead: None,
            render_ahead_generation: 0,
            timeline_audio: None,
            playback_speed: 1.0,
            selection_follows_playhead: true,
            scrub_audio: true,
            arrow_hold: None,
            properties_panel_open: true,
            snapping_enabled: true,
            export: None,
            current_project_path: None,
            project_error: None,
            audiometer_enabled: true,
            audiometer_level: (0.0, 0.0),
        }
    }
}

impl VibeVideoApp {
    fn import_media(&mut self, path: PathBuf) {
        match vv_media::probe(&path) {
            Ok(meta) => {
                self.import_error = None;
                self.ensure_timeline_for(&meta);
                // Fingerprint economico (path+dimensione+mtime, non i
                // byte del file: vedi doc di `content_fingerprint`),
                // chiave dei proxy e di qualunque altra cache derivata
                // dal contenuto — `0` solo se il file è già sparito tra
                // l'import e qui (raro, non impedisce comunque
                // l'import: un `content_hash` sbagliato al più fa
                // rigenerare un proxy che poteva essere riusato).
                let content_hash = vv_media::content_fingerprint(&path).unwrap_or(0);
                // Letti *prima* dell'insert (che muove `meta`): servono
                // per decidere se accodare la waveform (solo i media con
                // audio) e quanti picchi generare (proporzionali alla
                // durata).
                let has_audio = meta.has_audio;
                let num_peaks =
                    vv_media::recommended_num_peaks(meta.duration_frames as f64 / meta.fps.as_f64());
                // Un media può avere più stream audio (vedi doc di
                // `Clip::audio_stream_index`): una waveform per ciascuno,
                // non solo per il primo. Se il (ri)probe fallisce, ricade
                // su un solo stream — comportamento di prima.
                let num_audio_streams = if has_audio {
                    vv_media::audio_streams(&path).map(|s| s.len().max(1)).unwrap_or(1)
                } else {
                    0
                };
                let media_id = self.project.media_pool.insert(vv_core::MediaItem {
                    path: path.clone(),
                    meta,
                    content_hash,
                });
                // Sempre accodato, a prescindere da `proxy_enabled`: il
                // toggle controlla solo se l'anteprima *usa* il proxy
                // già pronto, non se viene generato — così è già lì
                // quando/se l'utente lo riattiva, invece di aspettare la
                // prima volta che serve davvero.
                self.proxy_worker
                    .get_or_insert_with(proxy_worker::ProxyWorker::spawn)
                    .enqueue(path.clone(), content_hash);
                // Waveform solo per i media con audio: la timeline la
                // disegna solo sulle clip audio, e un media senza audio
                // non ne avrebbe mai una da disegnare (il worker
                // restituirebbe `None` comunque, ma non serve nemmeno
                // aprire il file).
                for stream_index in 0..num_audio_streams {
                    self.waveform_worker
                        .get_or_insert_with(waveform_worker::WaveformWorker::spawn)
                        .enqueue(path.clone(), content_hash, stream_index, num_peaks);
                }
                self.preview_media(media_id);
            }
            Err(e) => self.import_error = Some(e.to_string()),
        }
    }

    // Apre il file dialog e importa il file scelto (usato dal pulsante
    // toolbar e dalla shortcut Ctrl+I).
    fn import_media_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("video", &["mp4", "mov", "mkv", "avi"])
            .pick_file()
        {
            self.import_media(path);
        }
    }

    /// Salva nel file corrente (`current_project_path`), o come "salva con
    /// nome" se il progetto non è ancora stato salvato/aperto.
    fn save_project(&mut self) {
        match self.current_project_path.clone() {
            Some(path) => self.save_project_to(&path),
            None => self.save_project_as(),
        }
    }

    /// Apre sempre il file dialog di salvataggio, anche se il progetto ha
    /// già un file corrente (usato dal pulsante "Salva con nome..." e da
    /// Ctrl+Shift+S).
    fn save_project_as(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name("progetto.vvproj")
            .add_filter("progetto vibevideo", &["vvproj"])
            .save_file()
        {
            self.save_project_to(&path);
        }
    }

    fn save_project_to(&mut self, path: &Path) {
        match vv_core::save_project(&self.project, path) {
            Ok(()) => {
                self.current_project_path = Some(path.to_path_buf());
                self.project_error = None;
            }
            Err(e) => self.project_error = Some(format!("Salvataggio fallito: {e}")),
        }
    }

    fn open_project_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("progetto vibevideo", &["vvproj"])
            .pick_file()
        {
            self.load_project_from(path);
        }
    }

    /// Sostituisce il progetto corrente con quello caricato da `path`:
    /// azzera tutto lo stato UI/di sessione legato al *vecchio* progetto
    /// (selezione, playhead, history, player/anteprima) — sarebbe
    /// incoerente riferito al nuovo. `timeline_id` diventa la prima (e di
    /// norma unica, con l'UI attuale) timeline del progetto caricato.
    fn load_project_from(&mut self, path: PathBuf) {
        match vv_core::load_project(&path) {
            Ok(project) => {
                self.timeline_id = project.timelines.keys().next();
                self.project = project;
                self.history = vv_core::History::default();
                self.timeline_state = timeline_ui::TimelineState::default();
                self.import_error = None;
                self.preview_meta = None;
                self.preview_error = None;
                self.frame_texture = None;
                self.last_viewer_frame_kind = None;
                self.browsing_decode_ahead = None;
                self.active_clip = None;
                self.last_synced_playhead = 0;
                self.browsing_media = None;
                if let Some(audio) = &mut self.timeline_audio {
                    audio.pause();
                    audio.invalidate();
                }
                self.reset_playback_speed_to_normal();
                if let Some(fps) = self.timeline_id.map(|id| self.project.timelines[id].fps.as_f64()) {
                    self.timeline_audio().seek_frame(0, fps);
                }
                self.current_project_path = Some(path);
                self.project_error = None;
                // Il progetto è stato sostituito senza passare da
                // `history.do_command` (che è stata appena azzerata):
                // `sync_render_ahead` non se ne accorgerebbe da sola
                // confrontando la generazione, quindi lo si notifica
                // esplicitamente qui.
                if let Some(timeline_id) = self.timeline_id {
                    self.spawn_render_ahead_if_needed(timeline_id);
                    if let Some(render_ahead) = &self.render_ahead {
                        render_ahead.update_project(&self.project, timeline_id);
                    }
                }
                self.render_ahead_generation = self.history.generation();
            }
            Err(e) => self.project_error = Some(format!("Apertura fallita: {e}")),
        }
    }

    /// Apre il dialog di salvataggio e, se l'utente conferma, avvia
    /// l'export su un thread dedicato: clona `self.project` (l'export
    /// lavora su questo snapshot, non sul progetto live — continuare a
    /// editare durante l'export non lo tocca) e gira
    /// `export::export_timeline` in background, aggiornando `self.export`
    /// con progresso/cancellazione condivisi (vedi il pannello di
    /// progresso in `update`).
    fn start_export(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        // Guardia contro un secondo export avviato mentre il primo è
        // ancora in corso: il pulsante in toolbar è già disabilitato in
        // quel caso (`add_enabled`), ma la shortcut Ctrl+Shift+E la
        // bypasserebbe senza questo controllo qui.
        if self.export.is_some() {
            return;
        }
        let Some(output_path) = rfd::FileDialog::new()
            .set_file_name("export.mp4")
            .add_filter("mp4", &["mp4"])
            .save_file()
        else {
            return;
        };

        let project = self.project.clone();
        let progress = std::sync::Arc::new(Mutex::new(export::ExportProgress::default()));
        let cancel = std::sync::Arc::new(AtomicBool::new(false));

        let thread_progress = progress.clone();
        let thread_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            let result = export::export_timeline(
                &project,
                timeline_id,
                &output_path,
                &thread_progress,
                &thread_cancel,
            );
            // `export_timeline` aggiorna `progress.done` solo sul percorso
            // di successo: qui si copre anche l'errore/l'annullamento, così
            // la UI (che legge solo `progress`, non fa join per sapere se è
            // finito) vede sempre uno stato coerente.
            if let Err(e) = &result {
                let mut p = thread_progress.lock().unwrap();
                p.error = Some(e.clone());
                p.done = true;
            }
            result
        });

        self.export = Some(ExportUiState {
            progress,
            cancel,
            handle,
        });
    }

    /// Piccola finestra di progresso mentre un export è in corso: barra
    /// (letta da `ExportUiState::progress`, condiviso col thread di
    /// export), "Annulla" finché non è finito, "Chiudi" quando lo è
    /// (successo o errore, mostrato). No-op se nessun export è in corso.
    fn show_export_progress(&mut self, ui: &mut egui::Ui) {
        let Some(state) = &self.export else {
            return;
        };

        let (current, total, done, error) = {
            let p = state.progress.lock().unwrap();
            (p.current_frame, p.total_frames, p.done, p.error.clone())
        };

        let mut should_close = false;
        egui::Window::new("Export")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                let fraction = if total > 0 {
                    (current as f32 / total as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .text(format!("{current}/{total} frame"))
                        .animate(!done),
                );
                if let Some(err) = &error {
                    ui.colored_label(egui::Color32::RED, err);
                } else if done {
                    ui.label("Export completato.");
                }
                ui.horizontal(|ui| {
                    if !done && ui.button("Annulla").clicked() {
                        state.cancel.store(true, Ordering::Relaxed);
                    }
                    if done && ui.button("Chiudi").clicked() {
                        should_close = true;
                    }
                });
            });

        // Repaint continuo mentre è in corso, altrimenti la barra non
        // avanzerebbe finché non arriva un altro input (stesso principio
        // del repaint continuo durante il playback, più sotto in questo
        // stesso metodo `update`).
        if !done {
            ui.ctx().request_repaint();
        }

        if should_close && let Some(state) = self.export.take() {
            let _ = state.handle.join();
        }
    }

    /// Due barre verticali (sinistra/destra) col livello del player
    /// attivo, disegnate in tutto lo spazio disponibile in `ui` (chi
    /// chiama ne ha già ritagliato una fascia stretta, vedi il pannello
    /// "audiometer" annidato in quello "timeline"). Non una misura
    /// professionale: solo il picco assoluto per canale dell'ultimo
    /// buffer audio (`TimelineAudio::peak_linear_stereo`), con un decadimento
    /// applicato qui frame per frame perché il valore istantaneo da solo
    /// farebbe scendere le barre a scatti invece che dolcemente.
    fn draw_audiometer(&mut self, ui: &mut egui::Ui) {
        const DECAY: f32 = 0.85;
        let (raw_l, raw_r) = self
            .timeline_audio
            .as_ref()
            .map(TimelineAudio::peak_linear_stereo)
            .unwrap_or((0.0, 0.0));
        let (level_l, level_r) = &mut self.audiometer_level;
        *level_l = raw_l.max(*level_l * DECAY);
        *level_r = raw_r.max(*level_r * DECAY);
        let (level_l, level_r) = (level_l.clamp(0.0, 1.0), level_r.clamp(0.0, 1.0));

        let rect = ui.available_rect_before_wrap();
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, egui::Color32::from_gray(20));

        let margin = 4.0;
        let gap = 2.0;
        let full_bar_rect = rect.shrink(margin);
        let bar_width = (full_bar_rect.width() - gap) / 2.0;
        let left_rect = egui::Rect::from_min_size(
            full_bar_rect.left_top(),
            egui::vec2(bar_width, full_bar_rect.height()),
        );
        let right_rect = egui::Rect::from_min_size(
            full_bar_rect.left_top() + egui::vec2(bar_width + gap, 0.0),
            egui::vec2(bar_width, full_bar_rect.height()),
        );

        for (bar_rect, level) in [(left_rect, level_l), (right_rect, level_r)] {
            painter.rect_filled(bar_rect, 2.0, egui::Color32::from_gray(10));
            if level > 0.0 {
                let fill_height = bar_rect.height() * level;
                let fill_rect = egui::Rect::from_min_max(
                    egui::pos2(bar_rect.left(), bar_rect.bottom() - fill_height),
                    bar_rect.right_bottom(),
                );
                // Verde fino al 70%, giallo fino al 90%, rosso oltre
                // (vicino al clipping) — stessa convenzione di un VU-meter
                // comune.
                let color = if level > 0.9 {
                    egui::Color32::from_rgb(220, 50, 50)
                } else if level > 0.7 {
                    egui::Color32::from_rgb(230, 200, 50)
                } else {
                    egui::Color32::from_rgb(60, 200, 90)
                };
                painter.rect_filled(fill_rect, 2.0, color);
            }
        }

        // Repaint continuo mentre il livello sta ancora decadendo verso lo
        // zero, altrimenti le barre resterebbero "incollate" all'ultimo
        // valore finché non arriva un altro input (stesso principio del
        // repaint durante il playback/export altrove in questo file).
        if level_l > 0.001 || level_r > 0.001 {
            ui.ctx().request_repaint();
        }
    }

    /// Anteprima "grezza" di un media dal media pool, non legata alla
    /// timeline: mostra il primo frame da un decode-ahead dedicato.
    fn preview_media(&mut self, media_id: MediaId) {
        // `frame_texture` resta: niente flash a vuoto durante il cambio media.
        self.browsing_decode_ahead = None;
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let (path, meta) = (item.path.clone(), item.meta.clone());
        if self.is_timeline_playing() {
            self.timeline_audio().pause();
        }
        self.reset_playback_speed_to_normal();
        self.preview_meta = Some(meta);
        self.preview_error = None;
        match vv_media::DecodeAhead::spawn(path, self.cache_budget_bytes, 60) {
            Ok(decode_ahead) => self.browsing_decode_ahead = Some(decode_ahead),
            Err(e) => self.preview_error = Some(e.to_string()),
        }
    }

    /// La clip Video attiva (track più in alto tra quelle che ne hanno una
    /// in quel punto, `Timeline::active_video_clip_at`) al frame `frame`,
    /// con la sua track — quella che il player/il viewer devono seguire.
    fn active_video_clip_at(&self, frame: FrameIdx) -> Option<(usize, ClipId)> {
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .active_video_clip_at(frame)
            .map(|(t, c)| (t, c.id))
    }

    /// Se "selection follows playhead" è attivo, allinea la selezione alla
    /// clip video attiva sotto al playhead corrente più le clip collegate
    /// (selezione vuota se il playhead è su un vuoto), scartando qualunque
    /// selezione precedente. No-op se la funzionalità è disattivata.
    fn sync_selection_to_playhead(&mut self) {
        if !self.selection_follows_playhead {
            return;
        }
        let Some((track_index, clip_id)) = self.active_video_clip_at(self.timeline_state.playhead)
        else {
            self.timeline_state.set_single_selection(None);
            return;
        };
        let mut selected = BTreeSet::from([(track_index, clip_id)]);
        if let Some(timeline_id) = self.timeline_id {
            selected.extend(self.group_members(timeline_id, track_index, clip_id));
        }
        self.timeline_state
            .set_selection(selected, Some((track_index, clip_id)));
    }

    /// Allinea la clip video attiva (viewer) al playhead e, se il playhead
    /// è stato spostato da fuori, anche il clock audio. `force_seek`: lo
    /// spostamento viene dall'utente e va seguito anche in riproduzione;
    /// quando è `drive_playback` a muovere il playhead il clock è già lì.
    fn ensure_active_clip_matches_playhead(&mut self, force_seek: bool) {
        if self.browsing_media.is_some() {
            return;
        }
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let playhead = self.timeline_state.playhead;
        self.active_clip = self.active_video_clip_at(playhead);
        if playhead != self.last_synced_playhead
            && (force_seek || !self.is_timeline_playing())
            && let Some(audio) = &mut self.timeline_audio
        {
            audio.seek_frame(playhead, self.project.timelines[timeline_id].fps.as_f64());
        }
        self.last_synced_playhead = playhead;
    }

    fn is_timeline_playing(&self) -> bool {
        self.timeline_audio
            .as_ref()
            .is_some_and(TimelineAudio::is_playing)
    }

    fn timeline_audio(&mut self) -> &mut TimelineAudio {
        self.timeline_audio.get_or_insert_with(TimelineAudio::new)
    }

    fn sync_timeline_audio(&mut self) {
        if let (Some(timeline_id), Some(audio)) = (self.timeline_id, &mut self.timeline_audio) {
            audio.sync(&self.project, timeline_id, self.history.generation());
        }
    }

    fn stop_browsing(&mut self) {
        self.browsing_media = None;
        self.browsing_decode_ahead = None;
        self.preview_meta = None;
    }

    /// Frecce sinistra/destra: un frame indietro/avanti, tenendo premuto
    /// scorre a `ARROW_HOLD_SPEED`. Mette in pausa se si sta riproducendo.
    /// Ritorna `true` finché una freccia è premuta.
    fn step_playhead_with_arrows(&mut self, direction: Option<FrameIdx>, time: f64) -> bool {
        let Some(direction) = direction else {
            self.arrow_hold = None;
            return false;
        };
        let Some(timeline_id) = self.timeline_id else {
            return false;
        };
        if self.arrow_hold.as_ref().is_none_or(|h| h.direction != direction) {
            if self.is_timeline_playing() {
                self.toggle_playback();
            }
            self.arrow_hold = Some(ArrowHold {
                direction,
                pressed_at: time,
                start_frame: self.timeline_state.playhead,
            });
        }
        let hold = self.arrow_hold.as_ref().expect("impostato sopra");
        let fps = self.project.timelines[timeline_id].fps.as_f64();
        let target = arrow_hold_target(hold, time - hold.pressed_at, fps);
        if target != self.timeline_state.playhead {
            self.timeline_state.playhead = target;
            self.ensure_active_clip_matches_playhead(true);
            self.sync_selection_to_playhead();
            self.play_scrub_audio();
        }
        true
    }

    fn play_scrub_audio(&mut self) {
        if !self.scrub_audio || self.browsing_media.is_some() {
            return;
        }
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let fps = self.project.timelines[timeline_id].fps.as_f64();
        let playhead = self.timeline_state.playhead;
        self.timeline_audio();
        self.sync_timeline_audio();
        let audio = self.timeline_audio();
        audio.seek_frame(playhead, fps);
        audio.play_scrub_snippet();
    }

    /// Play/pausa della timeline dal playhead, senza bisogno di selezione:
    /// vuoti inclusi (suonano silenzio), fermo solo oltre la fine del
    /// contenuto.
    fn toggle_playback(&mut self) {
        self.stop_browsing();
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.is_timeline_playing() {
            self.timeline_audio().pause();
            // La barra spaziatrice mette sempre in pausa a 1x: un "a"
            // successivo riparte a velocità normale.
            self.reset_playback_speed_to_normal();
            return;
        }
        let timeline = &self.project.timelines[timeline_id];
        let (fps, end) = (timeline.fps.as_f64(), timeline.total_frames());
        let playhead = self.timeline_state.playhead;
        if playhead >= end {
            return;
        }
        self.timeline_audio();
        self.sync_timeline_audio();
        let audio = self.timeline_audio();
        audio.seek_frame(playhead, fps);
        audio.play();
        self.last_synced_playhead = playhead;
        self.active_clip = self.active_video_clip_at(playhead);
    }

    /// Durante la riproduzione il playhead segue il clock del mixer; a fine
    /// contenuto si ferma.
    fn drive_playback(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if !self.is_timeline_playing() {
            return;
        }
        let timeline = &self.project.timelines[timeline_id];
        let (fps, end) = (timeline.fps.as_f64(), timeline.total_frames());
        let mut frame = self.timeline_audio().position_frame(fps);
        if frame >= end {
            frame = end;
            self.timeline_audio().pause();
            self.reset_playback_speed_to_normal();
            self.timeline_audio().seek_frame(end, fps);
        }
        self.timeline_state.playhead = frame;
        self.active_clip = self.active_video_clip_at(frame);
    }

    /// Tasto "a": da fermo come la barra spaziatrice (1x), in riproduzione
    /// accelera 2x -> 4x -> 8x. Solo la barra spaziatrice mette in pausa.
    fn handle_fast_playback_key(&mut self) {
        if !self.is_timeline_playing() {
            self.toggle_playback();
            return;
        }
        let next_speed = match SpeedTier::from_multiplier(self.playback_speed) {
            Some(tier) => tier.next().tempo(),
            None => SpeedTier::X2.tempo(),
        };
        self.request_playback_speed(next_speed);
    }

    fn reset_playback_speed_to_normal(&mut self) {
        self.request_playback_speed(1.0);
    }

    /// 1x subito; le velocità accelerate quando il loro audio stretchato
    /// è pronto (vedi `TimelineAudio::request_speed`).
    fn request_playback_speed(&mut self, speed: f64) {
        match &mut self.timeline_audio {
            Some(audio) => {
                audio.request_speed(speed);
                self.playback_speed = audio.speed();
            }
            None => self.playback_speed = speed,
        }
    }

    /// Intervalli (in frame di *timeline*) attualmente bufferizzati per
    /// ogni clip Media di ogni track video, per l'indicatore visivo
    /// "buffered" sulla timeline (richiesta: "visualizzare durante la
    /// riproduzione come viene fatto il buffer"). Interroga direttamente
    /// `render_ahead`, che bufferizza a livello di timeline (non più un
    /// caso a parte per la clip attiva/il preload della successiva): una
    /// clip su una track video occlusa da un'altra in quel punto
    /// (`active_video_clip_at`) risulterà correttamente "non
    /// bufferizzata", perché non è lei a essere mostrata né decodificata.
    fn buffered_timeline_ranges(&self) -> Vec<(FrameIdx, FrameIdx)> {
        let Some(timeline_id) = self.timeline_id else {
            return Vec::new();
        };
        let Some(render_ahead) = &self.render_ahead else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        for (_, track) in self.project.timelines[timeline_id].tracks_of_kind(TrackKind::Video) {
            for clip in &track.clips {
                if let vv_core::ClipSource::Media(media_id) = &clip.source {
                    ranges.extend(map_source_ranges_to_timeline(
                        clip,
                        &render_ahead.cached_ranges_for(*media_id),
                    ));
                }
            }
        }
        ranges
    }

    /// Intervalli (in frame di *timeline*) delle clip Media attualmente
    /// servite dal proxy invece che dal sorgente — indicatore visivo
    /// separato da quello "buffered" (colore diverso in
    /// `timeline_ui::show_timeline`): dice *da dove* arriverebbe il
    /// frame quando viene bufferizzato, non se è già pronto ora. Copre
    /// l'intera estensione di ogni clip proxy-backed, non solo la parte
    /// già decodificata: a differenza della cache, "proxy o sorgente"
    /// è deciso dal toggle + dalla disponibilità del file su disco, non
    /// da cosa è già stato effettivamente decodificato finora (vedi
    /// `render_ahead::fill_segments`, la stessa condizione).
    fn proxy_timeline_ranges(&self) -> Vec<(FrameIdx, FrameIdx)> {
        let Some(timeline_id) = self.timeline_id else {
            return Vec::new();
        };
        if !self.proxy_enabled {
            return Vec::new();
        }
        let mut ranges = Vec::new();
        for (_, track) in self.project.timelines[timeline_id].tracks_of_kind(TrackKind::Video) {
            for clip in &track.clips {
                if let vv_core::ClipSource::Media(media_id) = &clip.source
                    && let Some(item) = self.project.media_pool.get(*media_id)
                    && vv_media::proxy::proxy_exists(item.content_hash)
                {
                    ranges.push((clip.timeline_start, clip.timeline_end() - 1));
                }
            }
        }
        ranges
    }

    /// Carica in memoria la waveform (picchi audio) di ogni media audio
    /// presente sulla timeline, se il file di cache esiste ma non è ancora
    /// nella mappa della sessione: la timeline la disegna a ogni frame, e
    /// un `read` di un file da qualche MB a ogni repaint sarebbe un I/O
    /// inutile. Il file resta la fonte di verità (sopravvive al riavvio,
    /// generato dal `waveform_worker`); la mappa è solo una cache della
    /// sessione. Chiamato prima di `show_timeline`, che poi legge i picchi
    /// dalla mappa via la closure `waveform_peaks` che gli passa.
    fn ensure_waveforms_loaded(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        for (_, track) in self.project.timelines[timeline_id].tracks_of_kind(TrackKind::Audio) {
            for clip in &track.clips {
                if let vv_core::ClipSource::Media(media_id) = &clip.source
                    && let Some(item) = self.project.media_pool.get(*media_id)
                    && item.meta.has_audio
                    && !self
                        .waveform_cache
                        .contains_key(&(item.content_hash, clip.audio_stream_index))
                    && vv_media::waveform::waveform_exists(item.content_hash, clip.audio_stream_index)
                {
                    if let Some(peaks) =
                        vv_media::waveform::load_waveform(item.content_hash, clip.audio_stream_index)
                    {
                        self.waveform_cache
                            .insert((item.content_hash, clip.audio_stream_index), peaks);
                    }
                }
            }
        }
    }

    fn ensure_timeline(&mut self) -> TimelineId {
        if let Some(id) = self.timeline_id {
            return id;
        }
        let id = self.project.timelines.insert(vv_core::Timeline {
            name: "Timeline 1".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        self.timeline_id = Some(id);
        self.spawn_render_ahead_if_needed(id);
        id
    }

    /// Come `ensure_timeline`, ma se la timeline va creata al volo (import,
    /// o trascinamento di un media dal media pool sull'area timeline)
    /// eredita framerate e risoluzione da `meta` invece dei default fissi
    /// di `ensure_timeline` (che non ha un media da cui derivarli, es. per
    /// Solid Color).
    fn ensure_timeline_for(&mut self, meta: &vv_core::MediaMeta) -> TimelineId {
        if let Some(id) = self.timeline_id {
            return id;
        }
        let id = self.project.timelines.insert(vv_core::Timeline {
            name: "Timeline 1".into(),
            fps: meta.fps,
            resolution: (meta.width, meta.height),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        self.timeline_id = Some(id);
        self.spawn_render_ahead_if_needed(id);
        id
    }

    /// Spawna `render_ahead` la prima volta che esiste una timeline (una
    /// per sessione: non viene più ricreato dopo, solo aggiornato via
    /// `sync_render_ahead`/`RenderAhead::update_project`).
    fn spawn_render_ahead_if_needed(&mut self, timeline_id: TimelineId) {
        if self.render_ahead.is_none() {
            self.render_ahead = Some(render_ahead::RenderAhead::spawn(
                self.project.clone(),
                timeline_id,
                self.cache_budget_bytes,
                self.proxy_enabled,
                self.lookahead_secs,
                self.behind_secs,
            ));
            self.render_ahead_generation = self.history.generation();
        }
    }

    /// Da chiamare a ogni frame UI: se il progetto è cambiato dall'ultima
    /// volta (confronto economico su `History::generation`, vedi doc del
    /// campo `render_ahead_generation`), manda a `render_ahead` una nuova
    /// copia — non un clone/diff a ogni frame incondizionatamente, solo
    /// quando serve davvero.
    fn sync_render_ahead(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(render_ahead) = &self.render_ahead else {
            return;
        };
        let generation = self.history.generation();
        if generation != self.render_ahead_generation {
            render_ahead.update_project(&self.project, timeline_id);
            self.render_ahead_generation = generation;
        }
        // A velocità >1x il playhead avanza più veloce nel tempo reale:
        // serve più margine bufferizzato in avanti per non superare il
        // prefetch (vedi doc di `effective_lookahead_secs`).
        if self.playback_speed != 1.0 {
            render_ahead.set_lookahead_secs(self.effective_lookahead_secs(timeline_id));
        } else {
            render_ahead.set_lookahead_secs(self.lookahead_secs);
        }
        render_ahead.set_target(self.timeline_state.playhead);
    }

    /// `lookahead_secs` (base configurata dall'utente) scalato per
    /// `self.playback_speed`, clampato a quanto entra nel budget di
    /// memoria configurato (`cache_budget_bytes`) lasciando spazio anche
    /// per `behind_secs` — oltre quel limite scalare ulteriormente
    /// sarebbe comunque vanificato dagli sfratti Tier B di `render_ahead`
    /// (budget insufficiente per la finestra richiesta). Non scende mai
    /// sotto il valore base configurato dall'utente, anche a budget
    /// strettissimo.
    fn effective_lookahead_secs(&self, timeline_id: TimelineId) -> f64 {
        let scaled = self.lookahead_secs * self.playback_speed;
        let timeline = &self.project.timelines[timeline_id];
        let fps = timeline.fps.as_f64().max(1e-9);
        let (w, h) = timeline.resolution;
        let frame_bytes = ((w as usize * h as usize * 3) / 2).max(1);
        let max_frames = self.cache_budget_bytes / frame_bytes;
        let max_secs = (max_frames as f64 / fps - self.behind_secs).max(self.lookahead_secs);
        scaled.min(max_secs)
    }

    /// Crea una clip generatore SolidColor da 5s e la accoda in fondo alla
    /// prima track video. Il colore iniziale è grigio medio, modificabile
    /// subito dal pannello proprietà una volta selezionata.
    fn add_solid_color_clip(&mut self) {
        let timeline_id = self.ensure_timeline();
        let fps = self.project.timelines[timeline_id].fps.as_f64();
        let default_len = (fps * 5.0).round() as FrameIdx;
        let Some(video_track) =
            self.project.timelines[timeline_id].first_track_index(TrackKind::Video)
        else {
            return; // nessuna track video: non dovrebbe succedere, vedi doc di `RemoveTrack`
        };
        let video_start = track_end(&self.project, timeline_id, video_track);

        let effects = vv_core::EffectStack {
            color: Some(vv_core::Keyframed::constant(vv_core::Rgba {
                r: 0.6,
                g: 0.6,
                b: 0.6,
                a: 1.0,
            })),
            ..Default::default()
        };

        let clip = vv_core::Clip {
            id: self.project.alloc_clip_id(),
            source: vv_core::ClipSource::SolidColor,
            source_in: 0,
            source_out: default_len,
            timeline_start: video_start,
            effects,
            linked_group: None,
            audio_stream_index: 0,
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: video_track,
                clip,
            }),
        );
    }

    fn active_clip_effects(&self) -> Option<&vv_core::EffectStack> {
        let (track_index, clip_id) = self.active_clip?;
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .tracks
            .get(track_index)?
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .map(|c| &c.effects)
    }

    /// Il frame video grezzo (RGBA, non ancora composito) da mostrare nel
    /// viewer in questo momento, con la posizione (frame *sorgente*, per
    /// valutare il transform keyframeato) a cui corrisponde — `None` se
    /// non c'è ancora nulla di pronto. Due sorgenti possibili, mai
    /// entrambe insieme (`browsing_media` le distingue):
    /// `browsing_decode_ahead` durante un'anteprima "grezza" dal media
    /// pool, o `render_ahead` (il buffer a livello di timeline) per la
    /// clip Media attiva sulla timeline.
    fn current_video_frame(&mut self) -> Option<(std::sync::Arc<vv_media::FrameYuv420>, FrameIdx)> {
        if self.browsing_media.is_some() {
            let decode_ahead = self.browsing_decode_ahead.as_ref()?;
            let idx = 0;
            decode_ahead.set_target(idx);
            decode_ahead.cache().get(idx).map(|f| (f, idx))
        } else {
            let (track_index, clip_id) = self.active_clip?;
            let timeline_id = self.timeline_id?;
            let clip = self.project.timelines[timeline_id]
                .tracks
                .get(track_index)?
                .clips
                .iter()
                .find(|c| c.id == clip_id)?
                .clone();
            // Stessa interfaccia dell'export per procurare il frame
            // (`FrameProvider`, REFACTOR_PIPELINE.md B1) — qui backed
            // dalla cache di `render_ahead`, non bloccante. Il clamp
            // preserva il comportamento precedente per il breve istante
            // in cui `active_clip` può restare un frame indietro
            // rispetto al playhead appena aggiornato.
            let timeline_frame = self.timeline_state.playhead.max(clip.timeline_start);
            let source_frame = clip.source_frame_at(timeline_frame);
            self.render_ahead
                .as_mut()?
                .frame_for(&self.project, &clip, timeline_frame)
                .ok()?
                .map(|f| (f, source_frame))
        }
    }

    /// Le altre clip del gruppo collegato (`Clip::linked_group`) di una
    /// clip, esclusa lei stessa: vuoto se non è collegata a nulla. Cerca su
    /// tutte le track perché un gruppo può estendersi su più track.
    fn group_members(
        &self,
        timeline_id: TimelineId,
        track_index: usize,
        clip_id: ClipId,
    ) -> Vec<(usize, ClipId)> {
        let Some(group) = self.project.timelines[timeline_id]
            .tracks
            .get(track_index)
            .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
            .and_then(|c| c.linked_group)
        else {
            return Vec::new();
        };
        self.project.timelines[timeline_id]
            .clips_in_group(group)
            .into_iter()
            .filter(|&(_, id)| id != clip_id)
            .collect()
    }

    /// Aggiunge il media in coda a ciascuna track (video e, se presente,
    /// audio separatamente — comportamento storico, usato dai test e da
    /// eventuali altri chiamanti che non hanno una posizione esplicita).
    /// Per il drag&drop con posizionamento preciso vedi
    /// `add_media_to_timeline_at`.
    fn add_media_to_timeline(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let meta = item.meta.clone();
        // Se non esiste ancora una timeline (es. primo drag&drop dal media
        // pool), viene creata al volo ereditando fps/risoluzione da questo
        // media.
        let timeline_id = self.ensure_timeline_for(&meta);
        let video_track = self.project.timelines[timeline_id]
            .first_track_index(TrackKind::Video)
            .unwrap_or(0);
        let video_start = track_end(&self.project, timeline_id, video_track);
        self.insert_media_clip(
            timeline_id,
            media_id,
            &meta,
            video_start,
            timeline_ui::MediaDropTarget::Default,
        );
    }

    /// Come `add_media_to_timeline`, ma piazza la clip a `start` (posizione
    /// e `target` dal drag&drop sulla timeline, vedi `MediaDropTarget`).
    fn add_media_to_timeline_at(
        &mut self,
        media_id: MediaId,
        start: FrameIdx,
        target: timeline_ui::MediaDropTarget,
    ) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let meta = item.meta.clone();
        let timeline_id = self.ensure_timeline_for(&meta);
        self.insert_media_clip(timeline_id, media_id, &meta, start, target);
    }

    /// Inserisce la clip video (e una clip audio per stream, vedi
    /// `Clip::audio_stream_index`) a `start`. `target` (`MediaDropTarget`)
    /// sceglie la track video/audio di destinazione: `Default` è quella di
    /// sempre, `NewVideoTrack`/`NewAudioTrack` ne creano una al volo (solo
    /// se il media ha davvero audio da piazzarci, per `NewAudioTrack`).
    /// Se le track audio non bastano per il numero di stream, le mancanti
    /// vengono comunque create.
    ///
    /// Video e clip audio finiscono nello stesso gruppo collegato
    /// (`Clip::linked_group`).
    fn insert_media_clip(
        &mut self,
        timeline_id: TimelineId,
        media_id: MediaId,
        meta: &vv_core::MediaMeta,
        start: FrameIdx,
        target: timeline_ui::MediaDropTarget,
    ) {
        let video_track = if target == timeline_ui::MediaDropTarget::NewVideoTrack {
            let new_index = self.project.timelines[timeline_id].tracks.len();
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::AddTrack::new(timeline_id, TrackKind::Video)),
            );
            new_index
        } else {
            let Some(video_track) =
                self.project.timelines[timeline_id].first_track_index(TrackKind::Video)
            else {
                return; // nessuna track video: non dovrebbe succedere, vedi doc di `RemoveTrack`
            };
            video_track
        };
        let video_clip_id = self.project.alloc_clip_id();

        let mut audio_track_indices: Vec<usize> = self.project.timelines[timeline_id]
            .tracks_of_kind(TrackKind::Audio)
            .map(|(i, _)| i)
            .collect();

        if target == timeline_ui::MediaDropTarget::NewAudioTrack && meta.has_audio {
            let new_index = self.project.timelines[timeline_id].tracks.len();
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::AddTrack::new(timeline_id, TrackKind::Audio)),
            );
            // In testa: il primo stream deve atterrare sulla track appena
            // creata, non su una già esistente.
            audio_track_indices.insert(0, new_index);
        }

        let num_audio_streams = if meta.has_audio && !audio_track_indices.is_empty() {
            self.project
                .media_pool
                .get(media_id)
                .and_then(|item| vv_media::audio_streams(&item.path).ok())
                .map(|streams| streams.len().max(1))
                .unwrap_or(1) // probe fallito: ricade su un solo stream, comportamento di prima
        } else {
            0
        };

        while audio_track_indices.len() < num_audio_streams {
            let new_index = self.project.timelines[timeline_id].tracks.len();
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::AddTrack::new(timeline_id, TrackKind::Audio)),
            );
            audio_track_indices.push(new_index);
        }

        let audio_clip_ids: Vec<ClipId> = (0..num_audio_streams)
            .map(|_| self.project.alloc_clip_id())
            .collect();

        let video_clip = vv_core::Clip {
            id: video_clip_id,
            source: vv_core::ClipSource::Media(media_id),
            source_in: 0,
            source_out: meta.duration_frames,
            timeline_start: start,
            effects: vv_core::EffectStack::default(),
            linked_group: None, // collegate sotto, tutte insieme, dopo l'inserimento
            audio_stream_index: 0,
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: video_track,
                clip: video_clip,
            }),
        );

        let mut group_targets: Vec<(usize, ClipId)> = vec![(video_track, video_clip_id)];
        for (stream_index, (&track_index, &clip_id)) in
            audio_track_indices.iter().zip(audio_clip_ids.iter()).enumerate()
        {
            let audio_clip = vv_core::Clip {
                id: clip_id,
                source: vv_core::ClipSource::Media(media_id),
                source_in: 0,
                source_out: meta.duration_frames,
                timeline_start: start,
                effects: vv_core::EffectStack::default(),
                linked_group: None,
                audio_stream_index: stream_index,
            };
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::InsertClip {
                    timeline: timeline_id,
                    track_index,
                    clip: audio_clip,
                }),
            );
            group_targets.push((track_index, clip_id));
        }

        if group_targets.len() >= 2 {
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::LinkClips::new(timeline_id, group_targets)),
            );
        }
    }

    /// Normal delete: rimuove *tutte* le clip selezionate, lasciando un
    /// vuoto al loro posto. Le altre track non si muovono. Un solo passo di
    /// history per tutte insieme. L'ordine non conta: `LiftDelete` non
    /// sposta nient'altro. Non serve tirare dentro esplicitamente i gruppi
    /// collegati: selezionare una clip collegata seleziona già tutto il suo
    /// gruppo (vedi `TimelineState::set_selection`/i punti dove si clicca).
    fn delete_selected(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            return;
        }
        let commands: Vec<Box<dyn vv_core::Command>> = self
            .timeline_state
            .selected
            .iter()
            .copied()
            .map(|(track_index, clip_id)| {
                Box::new(vv_core::LiftDelete::new(timeline_id, track_index, clip_id))
                    as Box<dyn vv_core::Command>
            })
            .collect();
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(commands)),
        );
        self.timeline_state.clear_selection();
        self.sync_selection_to_playhead();
    }

    /// Gestisce gli eventi Copy/Paste della tastiera per la clipboard della
    /// timeline. Funzione isolata (non inline in `ui()`) apposta per poter
    /// scrivere un test: `ui()` prende un `eframe::Frame` che non ha un
    /// costruttore pubblico fuori da `eframe`, quindi non è testabile
    /// direttamente, mentre questa può girare dentro un `egui::Context`
    /// "nudo" in `run_ui` (stesso trucco già usato per `show_timeline` in
    /// `timeline_ui.rs`).
    ///
    /// Bug segnalato: Ctrl+V da tastiera "funzionava solo dal menu".
    /// Causa: `egui-winit` genera `Event::Paste` SOLO se la clipboard di
    /// *sistema* contiene già del testo non vuoto (vedi `is_paste_command`
    /// in egui-winit — se la legge vuota, non emette proprio l'evento). La
    /// nostra clipboard vera (`timeline_state.clipboard`) è interna
    /// all'app e non scrive nulla in quella di sistema, quindi Ctrl+V
    /// restava silenziosamente muto ogni volta che la clipboard di sistema
    /// era vuota (sessione appena avviata, mai copiato altro) — un caso
    /// facile da non notare se per caso conteneva già del testo di
    /// qualcos'altro. Il pulsante di menu funzionava comunque perché
    /// chiama `paste_clipboard_at_playhead` direttamente, scavalcando
    /// questo passaggio. Fix: dopo una copia riuscita, scrivere anche un
    /// placeholder nella clipboard di sistema (`ctx.copy_text`), così ce
    /// n'è sempre uno non vuoto quando c'è davvero qualcosa da incollare —
    /// il contenuto della stringa non conta, `Event::Paste` viene gestito
    /// a prescindere dal suo payload.
    fn handle_clipboard_events(&mut self, ui: &egui::Ui, events: &[egui::Event]) {
        for event in events {
            match event {
                egui::Event::Copy => {
                    self.copy_selected_clips();
                    if !self.timeline_state.clipboard.is_empty() {
                        ui.ctx().copy_text("vibevideo:clip".to_owned());
                    }
                }
                egui::Event::Paste(_) => self.paste_clipboard_at_playhead(),
                _ => {}
            }
        }
    }

    /// Copia le clip selezionate in `timeline_state.clipboard`, pronte per
    /// `paste_clipboard_at_playhead`. No-op se non c'è nulla di selezionato.
    /// Non serve tirare dentro esplicitamente i gruppi collegati: selezionare
    /// una clip collegata seleziona già tutto il suo gruppo.
    fn copy_selected_clips(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            return;
        }

        let tl = &self.project.timelines[timeline_id];
        // Il `LinkGroupId` originale non sopravvive al paste (va
        // riallocato), serve solo qui per capire quali entry erano nello
        // stesso gruppo al momento della copia — rimappato sotto in
        // `link_tag`, indici locali all'operazione di copia.
        let mut collected: Vec<(Option<vv_core::LinkGroupId>, timeline_ui::ClipboardEntry)> = self
            .timeline_state
            .selected
            .iter()
            .filter_map(|&(track_index, clip_id)| {
                let clip = tl
                    .tracks
                    .get(track_index)?
                    .clips
                    .iter()
                    .find(|c| c.id == clip_id)?;
                Some((
                    clip.linked_group,
                    timeline_ui::ClipboardEntry {
                        track_index,
                        relative_start: clip.timeline_start,
                        source: clip.source.clone(),
                        source_in: clip.source_in,
                        source_out: clip.source_out,
                        effects: clip.effects.clone(),
                        link_tag: None,
                        audio_stream_index: clip.audio_stream_index,
                    },
                ))
            })
            .collect();

        if collected.is_empty() {
            return;
        }

        let anchor = collected
            .iter()
            .map(|(_, e)| e.relative_start)
            .min()
            .unwrap_or(0);
        for (_, e) in &mut collected {
            e.relative_start -= anchor;
        }

        let mut tag_of: std::collections::HashMap<vv_core::LinkGroupId, u64> =
            std::collections::HashMap::new();
        for (group, e) in &mut collected {
            if let Some(g) = group {
                let next_tag = tag_of.len() as u64;
                e.link_tag = Some(*tag_of.entry(*g).or_insert(next_tag));
            }
        }

        self.timeline_state.clipboard = collected.into_iter().map(|(_, e)| e).collect();
    }

    /// `(timeline_start, timeline_end, source_in)` di una clip, se esiste.
    fn clip_bounds(
        &self,
        timeline_id: TimelineId,
        track_index: usize,
        clip_id: ClipId,
    ) -> Option<(FrameIdx, FrameIdx, FrameIdx)> {
        let clip = self.project.timelines[timeline_id]
            .tracks
            .get(track_index)?
            .clips
            .iter()
            .find(|c| c.id == clip_id)?;
        Some((clip.timeline_start, clip.timeline_end(), clip.source_in))
    }

    /// Applica la modifica necessaria a *una* clip esistente che si
    /// sovrappone a `[new_start, new_end)`: rimossa se completamente
    /// coperta, accorciata da un bordo se sporge solo da un lato, divisa
    /// in due se il nuovo intervallo cade nel suo mezzo (il pezzo
    /// centrale, quello coperto, sparisce — comportamento "overwrite" di
    /// un vero NLE). `old_start`/`old_end`/`source_in` sono lo stato
    /// *attuale* della clip (letto dal chiamante prima di accodare
    /// comandi, mai da uno stato immaginato). Ritorna `Some((id_sinistra,
    /// id_destra))` solo nel caso di uno split, per permettere al
    /// chiamante di ricollegare le due metà alla gemella coinvolta dalla
    /// stessa operazione (vedi `make_room_for_ranges`). I comandi vengono
    /// accodati a `commands`, non eseguiti subito.
    fn resolve_overlap(
        &mut self,
        timeline_id: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        old_start: FrameIdx,
        old_end: FrameIdx,
        source_in: FrameIdx,
        new_start: FrameIdx,
        new_end: FrameIdx,
        commands: &mut Vec<Box<dyn vv_core::Command>>,
    ) -> Option<(ClipId, ClipId)> {
        if old_start >= new_start && old_end <= new_end {
            commands.push(Box::new(vv_core::LiftDelete::new(
                timeline_id,
                track_index,
                clip_id,
            )));
            None
        } else if old_start < new_start && old_end > new_end {
            // Il nuovo intervallo cade nel mezzo: divide la clip in due,
            // poi accorcia la metà destra dal suo bordo sinistro fino a
            // `new_end` (la stessa formula usata sotto per `TrimStart`,
            // applicata alla clip *originale*: vedi nota lì).
            let right_id = self.project.alloc_clip_id();
            commands.push(Box::new(
                vv_core::SplitClip::new(timeline_id, track_index, clip_id, new_start)
                    .with_new_clip_id(right_id),
            ));
            commands.push(Box::new(vv_core::TrimClip::new(
                timeline_id,
                track_index,
                right_id,
                vv_core::TrimEdge::Start,
                source_in + (new_end - old_start),
            )));
            Some((clip_id, right_id))
        } else if old_start < new_start {
            // La coda sporge oltre `new_start`: accorcia il bordo destro
            // (fine) fin lì. `TrimClip::new_value` per il bordo `End` è un
            // `source_out` assoluto, non un frame di timeline — da qui la
            // conversione via `source_in` (il bordo `Start`, invariato,
            // resta il riferimento comune tra spazio timeline e sorgente).
            commands.push(Box::new(vv_core::TrimClip::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::TrimEdge::End,
                source_in + (new_start - old_start),
            )));
            None
        } else {
            // La testa sporge prima di `new_end`: accorcia il bordo
            // sinistro (inizio) fin lì (stessa conversione di cui sopra).
            commands.push(Box::new(vv_core::TrimClip::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::TrimEdge::Start,
                source_in + (new_end - old_start),
            )));
            None
        }
    }

    /// Libera `[start, end)` di ciascuna `(track_index, start, end)` in
    /// `ranges`, per far posto a nuove clip che stanno per essere inserite
    /// lì (paste): le clip già presenti che si sovrappongono vengono
    /// accorciate, divise o rimosse — mai lasciate sovrapposte con la
    /// nuova clip sopra (bug segnalato: "il player continua a riprodurre
    /// la clip sottostante" invece di quella appena incollata, anche se
    /// coperta visivamente).
    ///
    /// Dividere una clip non tocca il `linked_group` della metà sinistra
    /// (`SplitClip`, vedi doc): resta collegata a chiunque altro condivida
    /// il gruppo, split o no. La metà *destra* invece è nuova e parte
    /// scollegata — se altri membri del gruppo sono *anche loro* tra le
    /// track coinvolte in `ranges` (il caso comune: si incolla sempre
    /// l'intero gruppo video+audio insieme, vedi `copy_selected_clips`) e
    /// vengono divisi dallo stesso taglio, le loro metà destre vengono
    /// ricollegate tra loro subito dopo — stesso principio di
    /// `split_at_playhead`. Un membro del gruppo fuori da `ranges` (si
    /// sta incollando solo una parte del gruppo) resta semplicemente
    /// intoccato, ancora nel gruppo originale.
    ///
    /// I comandi vengono accodati a `commands`, non eseguiti subito: il
    /// chiamante li unisce in un'unica `CompositeCommand` insieme
    /// all'inserimento vero e proprio, per un solo passo di undo.
    fn make_room_for_ranges(
        &mut self,
        timeline_id: TimelineId,
        ranges: &[(usize, FrameIdx, FrameIdx)],
        commands: &mut Vec<Box<dyn vv_core::Command>>,
    ) {
        let mut processed: BTreeSet<(usize, ClipId)> = BTreeSet::new();
        let range_tracks: BTreeSet<usize> = ranges.iter().map(|(t, _, _)| *t).collect();

        for &(track_index, new_start, new_end) in ranges {
            if new_start >= new_end {
                continue;
            }
            let overlapping: Vec<(ClipId, FrameIdx, FrameIdx, FrameIdx, Option<vv_core::LinkGroupId>)> =
                self.project.timelines[timeline_id]
                    .tracks
                    .get(track_index)
                    .map(|t| {
                        t.clips
                            .iter()
                            .filter(|c| c.timeline_start < new_end && c.timeline_end() > new_start)
                            .map(|c| {
                                (
                                    c.id,
                                    c.timeline_start,
                                    c.timeline_end(),
                                    c.source_in,
                                    c.linked_group,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();

            for (clip_id, old_start, old_end, source_in, group) in overlapping {
                if !processed.insert((track_index, clip_id)) {
                    continue;
                }
                let split_halves = self.resolve_overlap(
                    timeline_id,
                    track_index,
                    clip_id,
                    old_start,
                    old_end,
                    source_in,
                    new_start,
                    new_end,
                    commands,
                );

                let Some(_group) = group else { continue };
                let mut new_rights: Vec<(usize, ClipId)> = split_halves
                    .map(|(_left, right)| (track_index, right))
                    .into_iter()
                    .collect();

                for (member_track, member_id) in self.group_members(timeline_id, track_index, clip_id) {
                    if !range_tracks.contains(&member_track)
                        || !processed.insert((member_track, member_id))
                    {
                        continue;
                    }
                    let Some((m_start, m_end, m_source_in)) =
                        self.clip_bounds(timeline_id, member_track, member_id)
                    else {
                        continue;
                    };
                    let member_split = self.resolve_overlap(
                        timeline_id,
                        member_track,
                        member_id,
                        m_start,
                        m_end,
                        m_source_in,
                        new_start,
                        new_end,
                        commands,
                    );
                    if let Some((_, right)) = member_split {
                        new_rights.push((member_track, right));
                    }
                }

                if new_rights.len() >= 2 {
                    commands.push(Box::new(vv_core::LinkClips::new(timeline_id, new_rights)));
                }
            }
        }
    }

    /// Incolla `timeline_state.clipboard` (Ctrl+C/Ctrl+V) alla posizione
    /// del playhead, preservando la disposizione relativa se erano state
    /// copiate più clip insieme, e ricollegando tra loro le coppie
    /// collegate copiate insieme. Le clip incollate "vincono" per intero
    /// il tratto che occupano: quel che già c'era lì viene accorciato,
    /// diviso o rimosso da `make_room_for_ranges` invece di restare
    /// sovrapposto sotto (bug segnalato). No-op se non c'è ancora nulla in
    /// clipboard o nessuna timeline.
    fn paste_clipboard_at_playhead(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.clipboard.is_empty() {
            return;
        }
        let playhead = self.timeline_state.playhead;
        let entries = self.timeline_state.clipboard.clone();

        // Pre-alloca gli id delle nuove clip: servono per risolvere i
        // link tra loro (che devono riferire il *nuovo* id della gemella,
        // non quello originale copiato, che potrebbe non esistere più).
        let new_ids: Vec<ClipId> = entries
            .iter()
            .map(|_| self.project.alloc_clip_id())
            .collect();

        let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();

        let ranges: Vec<(usize, FrameIdx, FrameIdx)> = entries
            .iter()
            .map(|entry| {
                (
                    entry.track_index,
                    playhead + entry.relative_start,
                    playhead + entry.relative_start + (entry.source_out - entry.source_in),
                )
            })
            .collect();
        self.make_room_for_ranges(timeline_id, &ranges, &mut commands);

        let mut new_selection = BTreeSet::new();
        for (i, entry) in entries.iter().enumerate() {
            let clip = vv_core::Clip {
                id: new_ids[i],
                source: entry.source.clone(),
                source_in: entry.source_in,
                source_out: entry.source_out,
                timeline_start: playhead + entry.relative_start,
                effects: entry.effects.clone(),
                linked_group: None, // ricollegate sotto, per link_tag
                audio_stream_index: entry.audio_stream_index,
            };
            new_selection.insert((entry.track_index, new_ids[i]));
            commands.push(Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: entry.track_index,
                clip,
            }));
        }

        // Ricollega le entry che condividevano un `link_tag` al momento
        // della copia: un nuovo `LinkClips` (gruppo nuovo) per ogni tag con
        // 2+ entry incollate.
        let mut by_tag: std::collections::HashMap<u64, Vec<(usize, ClipId)>> =
            std::collections::HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            if let Some(tag) = entry.link_tag {
                by_tag
                    .entry(tag)
                    .or_default()
                    .push((entry.track_index, new_ids[i]));
            }
        }
        for targets in by_tag.into_values() {
            if targets.len() >= 2 {
                commands.push(Box::new(vv_core::LinkClips::new(timeline_id, targets)));
            }
        }

        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(commands)),
        );
        let anchor = new_selection.iter().next().copied();
        self.timeline_state.set_selection(new_selection, anchor);
        if let Some(end) = ranges.iter().map(|&(_, _, end)| end).max() {
            self.timeline_state.playhead = end;
            self.ensure_active_clip_matches_playhead(true);
        }
    }

    /// Ripple delete: rimuove *tutte* le clip selezionate (e la gemella
    /// collegata di ciascuna, se c'è) e chiude i gap su *tutte* le track,
    /// mantenendo il sync audio/video (vedi ARCHITECTURE.md § Ripple
    /// delete — comportamento scelto: sempre globale, nessun toggle).
    ///
    /// Ogni clip selezionata (+ gemella, se non già anch'essa selezionata
    /// esplicitamente) è un'"unità" rimossa con un proprio
    /// `RippleDeleteAllTracks`; le unità vengono processate da destra a
    /// sinistra (per `timeline_start` decrescente) così che rimuoverne una
    /// non alteri la posizione — e quindi l'ordinamento già calcolato —
    /// delle altre non ancora processate: stesso principio già usato in
    /// `split_at_playhead` per evitare doppi spostamenti.
    fn ripple_delete_selected(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            // Nessuna clip selezionata: se invece è selezionato un vuoto
            // (click su uno spazio vuoto seguito da una clip, vedi
            // `TimelineState::selected_gap`), il ripple delete lo chiude
            // shiftando tutte le track — stessa meccanica del ripple
            // delete su una clip, ma senza nulla da rimuovere.
            if let Some((_, gap_start, gap_end)) = self.timeline_state.selected_gap {
                self.history.do_command(
                    &mut self.project,
                    Box::new(vv_core::RippleDeleteGap::new(
                        timeline_id,
                        gap_start,
                        gap_end - gap_start,
                    )),
                );
                self.timeline_state.clear_selection();
                self.sync_selection_to_playhead();
            }
            return;
        }
        let selected: Vec<(usize, ClipId)> = self.timeline_state.selected.iter().copied().collect();

        let mut processed: BTreeSet<(usize, ClipId)> = BTreeSet::new();
        let mut units: Vec<RippleUnit> = Vec::new();
        for &(track_index, clip_id) in &selected {
            if !processed.insert((track_index, clip_id)) {
                continue;
            }
            let start = self.project.timelines[timeline_id].tracks[track_index]
                .clips
                .iter()
                .find(|c| c.id == clip_id)
                .map(|c| c.timeline_start)
                .unwrap_or(0);
            let mut also_remove = Vec::new();
            for member in self.group_members(timeline_id, track_index, clip_id) {
                if processed.insert(member) {
                    also_remove.push(member);
                }
            }
            units.push(((track_index, clip_id), start, also_remove));
        }
        units.sort_by_key(|(_, start, _)| std::cmp::Reverse(*start));

        let commands: Vec<Box<dyn vv_core::Command>> = units
            .into_iter()
            .map(|((track_index, clip_id), _, also_remove)| {
                Box::new(
                    vv_core::RippleDeleteAllTracks::new(timeline_id, track_index, clip_id)
                        .with_also_remove(also_remove),
                ) as Box<dyn vv_core::Command>
            })
            .collect();

        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(commands)),
        );
        self.timeline_state.clear_selection();
        self.sync_selection_to_playhead();
    }

    /// Divide al playhead le clip selezionate che lo coprono o, senza
    /// selezione, tutte quelle che lo coprono su ogni track (tasto T).
    /// Un solo passo di history per l'intero taglio.
    /// I gruppi collegati (`Clip::linked_group`) i cui membri vengono
    /// tagliati insieme nello stesso punto restano collegati anche dopo:
    /// `SplitClip` non tocca il `linked_group` della metà sinistra (resta
    /// la stessa clip, solo accorciata), quindi serve solo ricollegare tra
    /// loro le metà *destre* (clip nuove, che partono scollegate) — un
    /// nuovo `LinkClips` per ogni gruppo originale con 2+ membri tagliati.
    fn split_at_playhead(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let playhead = self.timeline_state.playhead;
        let selected = &self.timeline_state.selected;
        let targets: Vec<(usize, ClipId, Option<vv_core::LinkGroupId>)> = self.project.timelines
            [timeline_id]
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(track_index, track)| {
                track
                    .clips
                    .iter()
                    .filter(move |c| playhead > c.timeline_start && playhead < c.timeline_end())
                    .filter(move |c| selected.is_empty() || selected.contains(&(track_index, c.id)))
                    .map(move |c| (track_index, c.id, c.linked_group))
            })
            .collect();
        if targets.is_empty() {
            return;
        }

        // Id della metà destra pre-allocato per ogni target, cosi da
        // poterlo usare subito per i comandi di ricollegamento.
        let new_ids: std::collections::HashMap<ClipId, ClipId> = targets
            .iter()
            .map(|(_, id, _)| (*id, self.project.alloc_clip_id()))
            .collect();

        let mut commands: Vec<Box<dyn vv_core::Command>> = targets
            .iter()
            .map(|(track_index, clip_id, _)| {
                Box::new(
                    vv_core::SplitClip::new(timeline_id, *track_index, *clip_id, playhead)
                        .with_new_clip_id(new_ids[clip_id]),
                ) as Box<dyn vv_core::Command>
            })
            .collect();

        let mut right_halves_by_group: std::collections::HashMap<vv_core::LinkGroupId, Vec<(usize, ClipId)>> =
            std::collections::HashMap::new();
        for (track_index, clip_id, group) in &targets {
            if let Some(g) = group {
                right_halves_by_group
                    .entry(*g)
                    .or_default()
                    .push((*track_index, new_ids[clip_id]));
            }
        }
        for right_halves in right_halves_by_group.into_values() {
            if right_halves.len() >= 2 {
                commands.push(Box::new(vv_core::LinkClips::new(timeline_id, right_halves)));
            }
        }

        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(commands)),
        );
        // "Selection follows playhead": seleziona la metà SINISTRA appena
        // tagliata sulla track video attiva (il suo id è invariato, la
        // metà che ha ottenuto un nuovo id è la destra — vedi sopra) *e*
        // la sua gemella audio collegata, esplicitamente, non tramite il
        // generico "clip sotto al playhead" (che per costruzione sarebbe
        // la metà destra, dato che il playhead è esattamente al suo
        // inizio: `clip_at` usa `start <= playhead < end`). L'intento più
        // comune dopo un taglio è rivedere/eliminare ciò che sta *prima*
        // del punto appena tagliato, non dopo. Con più track video, "la
        // track video" del taglio è quella più in alto tra quelle tagliate
        // — la stessa che il viewer mostrava un istante prima del taglio.
        if self.selection_follows_playhead
            && let Some((video_track, video_clip_id, _)) = targets
                .iter()
                .filter(|(track_index, _, _)| {
                    self.project.timelines[timeline_id].tracks[*track_index].kind
                        == TrackKind::Video
                })
                .max_by_key(|(track_index, _, _)| *track_index)
        {
            let mut selected = BTreeSet::from([(*video_track, *video_clip_id)]);
            selected.extend(self.group_members(timeline_id, *video_track, *video_clip_id));
            self.timeline_state
                .set_selection(selected, Some((*video_track, *video_clip_id)));
        }
    }
}

/// Mappa intervalli di frame *sorgente* (spazio nativo del media, quello
/// di `RenderAhead::cached_ranges_for`) in intervalli di frame di
/// *timeline*, per una clip: clampa al suo intervallo di trim
/// (`source_in..source_out`) e trasla per il suo `timeline_start`. Un
/// intervallo sorgente che cade fuori dal trim (o lo attraversa solo in
/// parte) viene scartato o accorciato di conseguenza. Funzione pura per
/// poterla testare senza un vero `RenderAhead`.
fn map_source_ranges_to_timeline(
    clip: &vv_core::Clip,
    source_ranges: &[(FrameIdx, FrameIdx)],
) -> Vec<(FrameIdx, FrameIdx)> {
    source_ranges
        .iter()
        .filter_map(|&(s_start, s_end)| {
            let start = s_start.max(clip.source_in);
            let end = s_end.min(clip.source_out - 1);
            (start <= end).then(|| {
                (
                    clip.timeline_start + (start - clip.source_in),
                    clip.timeline_start + (end - clip.source_in),
                )
            })
        })
        .collect()
}

fn track_end(project: &vv_core::Project, timeline_id: TimelineId, track_index: usize) -> FrameIdx {
    project.timelines[timeline_id]
        .tracks
        .get(track_index)
        .and_then(|t| t.clips.iter().map(|c| c.timeline_end()).max())
        .unwrap_or(0)
}

fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

/// Bottone diamante per il toggle keyframe di un parametro, allo stile
/// standard delle NLE: vuoto se il parametro non è animato (click = crea il
/// primo keyframe qui), pieno se c'è già un keyframe esattamente al frame
/// corrente (click = rimuovilo), vuoto-ma-animato altrimenti (click =
/// aggiungine uno qui).
///
/// Disegnato a mano (non i glifi Unicode "◇"/"◆") perché su alcune
/// combinazioni piattaforma/driver (es. Asahi Linux) i font bundled di
/// egui non li renderizzano — appaiono come quadratini vuoti.
fn keyframe_button(
    ui: &mut egui::Ui,
    is_constant: bool,
    has_keyframe_here: bool,
) -> egui::Response {
    let tooltip = if is_constant {
        "Anima: crea il primo keyframe qui"
    } else if has_keyframe_here {
        "Rimuovi il keyframe qui"
    } else {
        "Aggiungi un keyframe qui"
    };

    let size = egui::vec2(20.0, 20.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let painter = ui.painter();
        painter.rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
        let c = rect.center();
        let r = 5.0;
        let diamond = vec![
            c + egui::vec2(0.0, -r),
            c + egui::vec2(r, 0.0),
            c + egui::vec2(0.0, r),
            c + egui::vec2(-r, 0.0),
        ];
        if has_keyframe_here {
            painter.add(egui::Shape::convex_polygon(
                diamond,
                visuals.fg_stroke.color,
                egui::Stroke::NONE,
            ));
        } else {
            painter.add(egui::Shape::closed_line(diamond, visuals.fg_stroke));
        }
    }
    response.on_hover_text(tooltip)
}

/// Toggle "calamita" (snapping) della barra sotto il player: un ferro di
/// cavallo disegnato a mano (due gambe + arco inferiore + poli colorati),
/// non il glifo Unicode "🧲" — su alcune combinazioni piattaforma/driver
/// (es. Asahi Linux) i font bundled di egui non lo renderizzano (appare
/// come un quadratino vuoto). Evidenziato (sfondo di selezione) quando
/// `*enabled` è vero, sullo stile di `ui.toggle_value`.
fn magnet_toggle(ui: &mut egui::Ui, enabled: &mut bool) -> egui::Response {
    let size = egui::vec2(26.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *enabled = !*enabled;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, *enabled);
        let painter = ui.painter();
        painter.rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);

        let c = rect.center();
        let r: f32 = 6.0;
        let leg_top = c.y - 6.0;
        let arc_center_y = c.y + 1.0;
        let stroke = egui::Stroke::new(2.0, visuals.fg_stroke.color);

        painter.line_segment(
            [
                egui::pos2(c.x - r, leg_top),
                egui::pos2(c.x - r, arc_center_y),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(c.x + r, leg_top),
                egui::pos2(c.x + r, arc_center_y),
            ],
            stroke,
        );
        // Arco inferiore: t=0 -> gamba sinistra, t=π -> gamba destra,
        // passando per il punto più basso a t=π/2 (chiude la "U").
        let arc_points: Vec<egui::Pos2> = (0..=16)
            .map(|i| {
                let t = std::f32::consts::PI * (i as f32 / 16.0);
                egui::pos2(c.x - r * t.cos(), arc_center_y + r * t.sin())
            })
            .collect();
        painter.add(egui::Shape::line(arc_points, stroke));

        // Poli alle due punte, colorati come un vero magnete a ferro di
        // cavallo (convenzione da manuale scolastico: rosso e grigio).
        let pole_size = egui::vec2(r + 1.0, 3.0);
        painter.rect_filled(
            egui::Rect::from_center_size(egui::pos2(c.x - r, leg_top - 1.0), pole_size),
            1.0,
            egui::Color32::from_rgb(200, 60, 60),
        );
        painter.rect_filled(
            egui::Rect::from_center_size(egui::pos2(c.x + r, leg_top - 1.0), pole_size),
            1.0,
            egui::Color32::from_rgb(200, 200, 200),
        );
    }
    response
}

/// Traduce un'azione differita del pannello proprietà nel comando
/// `vv-core` corrispondente. Funzione libera (non un metodo) apposta:
/// testabile senza passare da un `egui::Context`.
fn build_effect_command(
    timeline_id: TimelineId,
    change: PendingEffectChange,
) -> Box<dyn vv_core::Command> {
    match change {
        PendingEffectChange::SetTransformDefault(track_index, clip_id, v) => Box::new(
            vv_core::SetClipTransform::new(timeline_id, track_index, clip_id, v),
        ),
        PendingEffectChange::SetGainDefault(track_index, clip_id, v) => Box::new(
            vv_core::SetClipGain::new(timeline_id, track_index, clip_id, v),
        ),
        PendingEffectChange::SetColorDefault(track_index, clip_id, v) => Box::new(
            vv_core::SetClipColor::new(timeline_id, track_index, clip_id, v),
        ),
        PendingEffectChange::UpsertTransformKeyframe(track_index, clip_id, frame, v) => {
            Box::new(vv_core::UpsertKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                frame,
                vv_core::KeyframeValue::Transform(v),
                vv_core::Interpolation::Linear,
            ))
        }
        PendingEffectChange::UpsertGainKeyframe(track_index, clip_id, frame, v) => {
            Box::new(vv_core::UpsertKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                frame,
                vv_core::KeyframeValue::Gain(v),
                vv_core::Interpolation::Linear,
            ))
        }
        PendingEffectChange::UpsertColorKeyframe(track_index, clip_id, frame, v) => {
            Box::new(vv_core::UpsertKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                frame,
                vv_core::KeyframeValue::Color(v),
                vv_core::Interpolation::Linear,
            ))
        }
        PendingEffectChange::RemoveTransformKeyframe(track_index, clip_id, frame) => {
            Box::new(vv_core::RemoveKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::KeyframeTarget::Transform,
                frame,
            ))
        }
        PendingEffectChange::RemoveGainKeyframe(track_index, clip_id, frame) => {
            Box::new(vv_core::RemoveKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::KeyframeTarget::Gain,
                frame,
            ))
        }
        PendingEffectChange::RemoveColorKeyframe(track_index, clip_id, frame) => {
            Box::new(vv_core::RemoveKeyframe::new(
                timeline_id,
                track_index,
                clip_id,
                vv_core::KeyframeTarget::Color,
                frame,
            ))
        }
    }
}

/// Su Wayland winit manda `Started` per lo scroll ad alta risoluzione ma
/// quasi mai `Ended`: egui resta "in touch" e somma i modificatori in OR,
/// quindi Alt rimane attivo (zoom bloccato) dopo il rilascio. Come `Move`
/// ogni evento usa invece i modificatori correnti.
fn unstick_wheel_modifiers(raw_input: &mut egui::RawInput) {
    for event in &mut raw_input.events {
        if let egui::Event::MouseWheel { phase, .. } = event
            && *phase == egui::TouchPhase::Start
        {
            *phase = egui::TouchPhase::Move;
        }
    }
}

impl eframe::App for VibeVideoApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        unstick_wheel_modifiers(raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(audio) = &mut self.timeline_audio {
            audio.tick();
            self.playback_speed = audio.speed();
            if audio.is_scrub_snippet_active() {
                ui.ctx().request_repaint();
            }
        }
        self.sync_timeline_audio();

        // Eventi Copy/Paste raccolti qui dentro (vedi sotto) ma gestiti
        // *fuori* dalla chiusura di `ui.input`: `Context::input` tiene il
        // lock in scrittura del contesto per tutta la sua durata, e
        // `handle_clipboard_events` deve poter chiamare `ctx.copy_text`
        // (che lo richiede anche lui) — farlo da dentro la chiusura
        // rientrava sullo stesso lock non rientrante e faceva deadlockare
        // l'intera UI al primo Ctrl+C (bug segnalato: "si blocca tutto
        // appena premo Ctrl+C su una clip", panic "Failed to acquire
        // RwLock write... Deadlock?").
        let mut clipboard_events: Vec<egui::Event> = Vec::new();
        let mut arrow_input = (None, 0.0);
        ui.input(|i| {
            arrow_input.0 = match (
                i.key_down(egui::Key::ArrowLeft),
                i.key_down(egui::Key::ArrowRight),
            ) {
                (true, false) => Some(-1),
                (false, true) => Some(1),
                _ => None,
            };
            arrow_input.1 = i.time;
            if i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace) {
                self.delete_selected();
            }
            // Tasto fisico "<" (il 102° tasto ISO, tra Shift sinistro e Z
            // sui layout europei/italiani — `IntlBackslash` in egui,
            // assente sui layout US ANSI): dedicato al ripple delete,
            // prima era Shift+Delete/Backspace.
            if i.key_pressed(egui::Key::IntlBackslash) {
                self.ripple_delete_selected();
            }
            if i.key_pressed(egui::Key::T) && !i.modifiers.command {
                self.split_at_playhead();
            }
            if i.modifiers.command && i.key_pressed(egui::Key::Z) {
                if i.modifiers.shift {
                    self.history.redo(&mut self.project);
                } else {
                    self.history.undo(&mut self.project);
                }
            }
            if i.key_pressed(egui::Key::Space) {
                self.toggle_playback();
            }
            // "a": come la barra spaziatrice, ma se già in riproduzione
            // accelera invece di mettere in pausa (1x -> 2x -> 4x -> 8x) —
            // vedi doc di `handle_fast_playback_key`.
            if i.key_pressed(egui::Key::A) {
                self.handle_fast_playback_key();
            }
            // Ctrl+I (Cmd+I su macOS): import media, come il pulsante toolbar.
            if i.modifiers.command && i.key_pressed(egui::Key::I) {
                self.import_media_dialog();
            }
            // Ctrl+S: salva (nel file corrente, o "salva con nome" se il
            // progetto non è ancora stato salvato). Ctrl+Shift+S: sempre
            // "salva con nome", anche se il progetto ha già un file.
            if i.modifiers.command && i.key_pressed(egui::Key::S) {
                if i.modifiers.shift {
                    self.save_project_as();
                } else {
                    self.save_project();
                }
            }
            // Ctrl+O: apri un progetto.
            if i.modifiers.command && i.key_pressed(egui::Key::O) {
                self.open_project_dialog();
            }
            // Ctrl+Shift+E: esporta, come il pulsante in File.
            if i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::E) {
                self.start_export();
            }
            // Ctrl+C / Ctrl+V: copia/incolla clip sulla timeline. Non
            // `key_pressed(Key::C/V)`: l'integrazione (eframe/winit)
            // intercetta Ctrl+C/Ctrl+V a monte e li consegna come eventi
            // semantici `Copy`/`Paste`, non come normali pressioni di
            // tasto — con `key_pressed` la scorciatoia risultava
            // silenziosamente inattiva (il pulsante in menu, che chiama
            // gli stessi metodi, funzionava comunque). Gestiti fuori da
            // qui (vedi sopra), solo raccolti: `handle_clipboard_events`
            // per il bug più subdolo trovato dopo.
            for event in &i.events {
                if matches!(event, egui::Event::Copy | egui::Event::Paste(_)) {
                    clipboard_events.push(event.clone());
                }
            }
            // Ctrl+"+"/Ctrl+"-" (anche Ctrl+"=", stesso tasto di "+" non
            // shiftato sulla maggior parte delle tastiere): zoom della
            // timeline.
            if i.modifiers.command
                && (i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals))
            {
                self.timeline_state.zoom_in();
            }
            if i.modifiers.command && i.key_pressed(egui::Key::Minus) {
                self.timeline_state.zoom_out();
            }
        });
        self.handle_clipboard_events(ui, &clipboard_events);
        if self.step_playhead_with_arrows(arrow_input.0, arrow_input.1) {
            ui.ctx().request_repaint();
        }

        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let export_disabled = self.timeline_id.is_none() || self.export.is_some();

                ui.menu_button("File", |ui| {
                    if ui.button("Apri progetto... (Ctrl+O)").clicked() {
                        self.open_project_dialog();
                        ui.close();
                    }
                    if ui.button("Salva (Ctrl+S)").clicked() {
                        self.save_project();
                        ui.close();
                    }
                    if ui.button("Salva con nome... (Ctrl+Shift+S)").clicked() {
                        self.save_project_as();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Importa media... (Ctrl+I)").clicked() {
                        self.import_media_dialog();
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(!export_disabled, egui::Button::new("Esporta... (Ctrl+Shift+E)"))
                        .on_hover_text("Esporta l'intera timeline in un file MP4 (H.264 + AAC)")
                        .clicked()
                    {
                        self.start_export();
                        ui.close();
                    }
                });

                ui.menu_button("Modifica", |ui| {
                    if ui.button("Undo (Ctrl+Z)").clicked() {
                        self.history.undo(&mut self.project);
                        ui.close();
                    }
                    if ui.button("Redo (Ctrl+Shift+Z)").clicked() {
                        self.history.redo(&mut self.project);
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            !self.timeline_state.selected.is_empty(),
                            egui::Button::new("Copia (Ctrl+C)"),
                        )
                        .clicked()
                    {
                        // Passa anche da qui (non solo `copy_selected_clips`
                        // diretto) per scrivere lo stesso placeholder nella
                        // clipboard di sistema — vedi doc di
                        // `handle_clipboard_events`: serve perché Ctrl+V da
                        // tastiera dipende da quella, non dalla nostra.
                        self.handle_clipboard_events(ui, &[egui::Event::Copy]);
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            !self.timeline_state.clipboard.is_empty(),
                            egui::Button::new("Incolla (Ctrl+V)"),
                        )
                        .on_hover_text("Incolla alla posizione del playhead")
                        .clicked()
                    {
                        self.paste_clipboard_at_playhead();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Elimina (Del)").clicked() {
                        self.delete_selected();
                        ui.close();
                    }
                    if ui
                        .button("Ripple delete (<)")
                        .on_hover_text(
                            "Rimuove la clip e chiude il gap su tutte le track, mantenendo il sync A/V",
                        )
                        .clicked()
                    {
                        self.ripple_delete_selected();
                        ui.close();
                    }
                    if ui
                        .button("Dividi (T)")
                        .on_hover_text("Taglia al playhead le clip selezionate, o tutte se non c'è selezione")
                        .clicked()
                    {
                        self.split_at_playhead();
                        ui.close();
                    }
                });

                ui.menu_button("Clip", |ui| {
                    if ui.button("Nuovo Solid Color").clicked() {
                        self.add_solid_color_clip();
                        ui.close();
                    }
                });

                ui.menu_button("Timeline", |ui| {
                    // Checkbox: restano aperti al click, a differenza dei
                    // pulsanti-azione altrove nei menu.
                    ui.checkbox(
                        &mut self.selection_follows_playhead,
                        "Selection follows playhead",
                    )
                    .on_hover_text(
                        "Sposta la selezione sulla clip video sotto al playhead a ogni scrub/taglio/ripple-delete",
                    );
                    ui.checkbox(&mut self.scrub_audio, "Audio durante lo scrub")
                        .on_hover_text("Suona un breve frammento audio a ogni spostamento manuale del playhead");
                    ui.separator();
                    if ui.button("Zoom avanti (Ctrl++)").clicked() {
                        self.timeline_state.zoom_in();
                        ui.close();
                    }
                    if ui.button("Zoom indietro (Ctrl+-)").clicked() {
                        self.timeline_state.zoom_out();
                        ui.close();
                    }
                    ui.label("Alt+scroll (o pinch) sopra la timeline zooma allo stesso modo.");
                });

                ui.menu_button("Playback", |ui| {
                    ui.menu_button("Proxy", |ui| {
                        if ui
                            .checkbox(&mut self.proxy_enabled, "Usa proxy")
                            .on_hover_text(
                                "Anteprima/editing da una copia a bassa risoluzione generata in \
                                 background invece che dal sorgente: scrub molto più fluido su \
                                 sorgenti lunghi. L'export non è mai influenzato, usa sempre i \
                                 sorgenti originali. Disattiva per lavori che richiedono la \
                                 qualità piena.",
                            )
                            .changed()
                            && let Some(render_ahead) = &self.render_ahead
                        {
                            render_ahead.set_proxy_enabled(self.proxy_enabled);
                        }
                        ui.horizontal(|ui| {
                            ui.label("Read-ahead avanti:");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.lookahead_secs)
                                        .range(0.0..=30.0)
                                        .speed(0.1)
                                        .suffix(" s"),
                                )
                                .on_hover_text(
                                    "Quanti secondi di timeline bufferizzare in anticipo avanti \
                                     dalla testina. Di più = scrub/playback più fluidi ma più RAM \
                                     e CPU spesi su frame che potrebbero non servire mai; di meno \
                                     = più leggero ma più probabile una breve attesa durante uno \
                                     scrub veloce. Resta comunque un margine minimo anche a 0.",
                                )
                                .changed()
                                && let Some(render_ahead) = &self.render_ahead
                            {
                                render_ahead.set_lookahead_secs(self.lookahead_secs);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Read-ahead dietro:");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut self.behind_secs)
                                        .range(0.0..=30.0)
                                        .speed(0.1)
                                        .suffix(" s"),
                                )
                                .on_hover_text(
                                    "Quanti secondi di timeline tenere bufferizzati anche dietro \
                                     la testina, oltre alla finestra in avanti: rende economico \
                                     uno scrub avanti-indietro ravvicinato senza dover \
                                     ridecodificare ogni volta. Resta comunque un margine minimo \
                                     anche a 0.",
                                )
                                .changed()
                                && let Some(render_ahead) = &self.render_ahead
                            {
                                render_ahead.set_behind_secs(self.behind_secs);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Cache video:");
                            // Espresso in MB nella UI, ma `cache_budget_bytes`
                            // resta in byte internamente (vedi doc del campo):
                            // effetto solo sui player aperti *dopo* la
                            // modifica, non su uno già in corso.
                            let mut budget_mb = (self.cache_budget_bytes / 1_000_000) as u32;
                            if ui
                                .add(
                                    egui::DragValue::new(&mut budget_mb)
                                        .range(100..=8000)
                                        .suffix(" MB"),
                                )
                                .on_hover_text(
                                    "Quanta RAM pre-decodificare per il player aperto: di più = \
                                     scrub/playback più fluidi, di meno = meno rischio di esaurire \
                                     la memoria (soprattutto con sorgenti 4K+). Effetto dal \
                                     prossimo cambio clip.",
                                )
                                .changed()
                            {
                                self.cache_budget_bytes = budget_mb as usize * 1_000_000;
                                if let Some(render_ahead) = &self.render_ahead {
                                    render_ahead.set_cache_budget_bytes(self.cache_budget_bytes);
                                }
                            }
                        });
                    });
                });

                ui.menu_button("Visualizza", |ui| {
                    ui.checkbox(&mut self.properties_panel_open, "Pannello proprietà");
                    ui.checkbox(&mut self.audiometer_enabled, "Audiometer")
                        .on_hover_text(
                            "Livello del player attivo, in una fascia stretta a destra della timeline",
                        );
                });
            });
        });

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Nessun pulsante play/pause: Spazio è implicito in ogni
                // editor video, un pulsante dedicato è ridondante (la
                // shortcut resta comunque attiva, vedi il blocco
                // `ui.input` più sopra).
                if let Some(meta) = &self.preview_meta {
                    let duration = meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9);
                    ui.label(format!("0.00s / {duration:.2}s"));
                } else if let Some(timeline_id) = self.timeline_id {
                    let timeline = &self.project.timelines[timeline_id];
                    let fps = timeline.fps.as_f64().max(1e-9);
                    let pos = self.timeline_state.playhead as f64 / fps;
                    let duration = timeline.total_frames() as f64 / fps;
                    ui.label(format!("{pos:.2}s / {duration:.2}s"));
                }
                if let Some(err) = &self.project_error {
                    ui.separator();
                    ui.colored_label(egui::Color32::RED, err);
                }
            });
        });

        self.show_export_progress(ui);

        // Frame a cui vengono lette/scritte le proprietà nel pannello:
        // sempre il playhead della timeline tradotto nello spazio frame
        // sorgente della clip *selezionata* (source_in + offset locale,
        // clampato dentro la clip) — indipendente da quale clip stia
        // effettivamente riproducendo il player, così modificare le
        // proprietà di una clip diversa da quella attiva resta coerente
        // con quello che si vede scorrendo la timeline fin lì.
        // Il pannello proprietà mostra l'editor completo solo quando la
        // selezione è di *una* clip sola: con selezione multipla o vuota
        // non c'è un singolo `source_frame`/set di effetti da modificare
        // (vedi il pannello "properties" più sotto).
        let single_selected = (self.timeline_state.selected.len() == 1)
            .then(|| *self.timeline_state.selected.iter().next().unwrap());

        let source_frame = single_selected
            .and_then(|(track_index, clip_id)| {
                let timeline_id = self.timeline_id?;
                let clip = self.project.timelines[timeline_id]
                    .tracks
                    .get(track_index)?
                    .clips
                    .iter()
                    .find(|c| c.id == clip_id)?;
                let local = (self.timeline_state.playhead - clip.timeline_start)
                    .clamp(0, clip.timeline_len().saturating_sub(1));
                Some(clip.source_in + local)
            })
            .unwrap_or(0);

        let selected_before_timeline_ui = self.timeline_state.selected.clone();
        let playhead_before_timeline_ui = self.timeline_state.playhead;
        // Se esiste già una timeline, la posizione esatta del rilascio (e
        // l'anteprima mentre si trascina) è gestita da `show_timeline`
        // stesso, che ha accesso a fps/scala per convertire pixel->frame.
        // Se non esiste ancora, non c'è nessuna scala a cui ancorare una
        // posizione: qui basta un semplice drop-ovunque che la crei al volo
        // (`add_media_to_timeline` -> `ensure_timeline_for`) e appenda il
        // media a frame 0.
        let mut media_drop: Option<(MediaId, FrameIdx, timeline_ui::MediaDropTarget)> = None;
        let mut dropped_on_empty_timeline: Option<MediaId> = None;
        egui::Panel::bottom("timeline")
            .default_size(240.0)
            .resizable(true)
            .show(ui, |ui| {
                if self.audiometer_enabled {
                    // Fascia stretta a destra, ritagliata *prima* di
                    // mostrare la timeline: le ruba solo questa larghezza
                    // fissa, non la comprime in proporzione.
                    egui::Panel::right("audiometer")
                        .default_size(40.0)
                        .resizable(false)
                        .show(ui, |ui| {
                            self.draw_audiometer(ui);
                        });
                }
                if let Some(timeline_id) = self.timeline_id {
                    let labels: HashMap<MediaId, String> = self
                        .project
                        .media_pool
                        .iter()
                        .map(|(id, item)| (id, file_label(&item.path)))
                        .collect();
                    let buffered_ranges = self.buffered_timeline_ranges();
                    let proxy_ranges = self.proxy_timeline_ranges();
                    // Il worker di `render_ahead` bufferizza su un thread
                    // proprio, a un ritmo suo indipendente dai repaint
                    // della UI (vedi doc del modulo `render_ahead`) — ma
                    // egui/eframe non ridisegna da solo se non c'è un
                    // input o una `request_repaint` esplicita: senza
                    // questo, la barra "buffered" avanzava solo quando
                    // *qualcos'altro* forzava comunque un repaint (es.
                    // muovere il mouse, che genera eventi di input) anche
                    // se il buffer stava davvero avanzando in background
                    // (bug segnalato dall'utente). `is_caught_up` viene
                    // dal worker stesso (vedi doc lì): stessa identica
                    // logica del repaint continuo durante l'export
                    // (`!done`, più sopra in questo file), si ferma da
                    // sé un ciclo dopo che il worker si è davvero
                    // stabilizzato.
                    if self.render_ahead.as_ref().is_some_and(|r| !r.is_caught_up()) {
                        ui.ctx().request_repaint();
                    }
                    // Picchi audio già in memoria (caricati dal file di cache
                    // la prima volta che servono, vedi `ensure_waveforms_loaded`):
                    // la timeline li disegna come waveform sulle clip audio.
                    self.ensure_waveforms_loaded();
                    let is_playing = self.is_timeline_playing();
                    media_drop = timeline_ui::show_timeline(
                        ui,
                        &mut self.project,
                        &mut self.history,
                        timeline_id,
                        &|id| labels.get(&id).cloned().unwrap_or_default(),
                        &mut self.timeline_state,
                        self.snapping_enabled,
                        &buffered_ranges,
                        &proxy_ranges,
                        &self.waveform_cache,
                        is_playing,
                    );
                } else {
                    let drop_rect = ui.available_rect_before_wrap();
                    let drop_id = ui.id().with("timeline_drop_zone_empty");
                    let drop_resp = ui.interact(drop_rect, drop_id, egui::Sense::hover());
                    dropped_on_empty_timeline = drop_resp
                        .dnd_release_payload::<MediaId>()
                        .map(|arc| *arc);
                    ui.label("Importa un media (o trascinalo qui dal media pool) per creare la timeline.");
                }
            });
        if let Some(media_id) = dropped_on_empty_timeline {
            self.add_media_to_timeline(media_id);
        }
        if let Some((media_id, start, target)) = media_drop {
            self.add_media_to_timeline_at(media_id, start, target);
        }

        // L'utente ha trascinato/cliccato il playhead in questo frame?
        // Serve per forzare un seek anche se si sta riproducendo (bug:
        // "durante il playback lo scrub veniva ignorato") — a differenza
        // di quando è `drive_playback` stesso a spostare il playhead per
        // seguire la riproduzione, che non deve innescare un seek.
        let user_scrubbed_playhead = self.timeline_state.playhead != playhead_before_timeline_ui;
        if user_scrubbed_playhead {
            self.sync_selection_to_playhead();
        }

        // Interagire con la timeline (selezionare una clip o spostare il
        // playhead) riprende il controllo del viewer dall'anteprima
        // "grezza" del media pool, se attiva.
        if self.browsing_media.is_some()
            && (self.timeline_state.selected != selected_before_timeline_ui || user_scrubbed_playhead)
        {
            self.stop_browsing();
        }

        if self.browsing_media.is_none() {
            self.ensure_active_clip_matches_playhead(user_scrubbed_playhead);
            if user_scrubbed_playhead {
                self.play_scrub_audio();
            }
            // `drive_playback` avanza il playhead da sé durante la
            // riproduzione: senza questo confronto, "selection follows
            // playhead" seguiva solo lo scrub manuale (già coperto sopra
            // da `user_scrubbed_playhead`) e restava fermo durante il
            // play normale (bug: "la selezione non segue durante la
            // riproduzione"). Il confronto prima/dopo, anziché una sync
            // incondizionata, lascia intatta un'eventuale selezione
            // esplicita impostata nello stesso frame da altrove (es.
            // `split_at_playhead`) quando il playhead in realtà non
            // si muove (caso normale: taglio da fermo).
            let playhead_before_playback = self.timeline_state.playhead;
            self.drive_playback();
            if self.timeline_state.playhead != playhead_before_playback {
                self.sync_selection_to_playhead();
            }
        }
        // Fuori dall'`if` sopra: il buffer a livello di timeline deve
        // restare caldo anche mentre si sta sfogliando un'anteprima
        // "grezza" dal media pool (`browsing_media`), non solo durante la
        // riproduzione sulla timeline.
        self.sync_render_ahead();

        let mut preview_action = None;
        let mut pending_effect = None;
        egui::Panel::left("media_pool")
            .default_size(260.0)
            .show(ui, |ui| {
                ui.heading("Media Pool");
                if let Some(err) = &self.import_error {
                    ui.colored_label(egui::Color32::RED, err);
                }
                // auto_shrink([false, false]): senza, la ScrollArea (e
                // quindi il pannello stesso) si restringe alla larghezza
                // del contenuto invece di riempire quella assegnata dal
                // Panel — stessa causa del bug "il resize del pannello
                // torna indietro al rilascio", vedi il commento identico
                // in timeline_ui::show_timeline.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let items: Vec<(MediaId, String, vv_core::MediaMeta)> = self
                            .project
                            .media_pool
                            .iter()
                            .map(|(id, item)| (id, file_label(&item.path), item.meta.clone()))
                            .collect();
                        for (id, label, meta) in items {
                            let group_resp = ui
                                .group(|ui| {
                                    ui.label(&label);
                                    ui.small(format!(
                                        "{}x{} · {:.2}fps · {}",
                                        meta.width,
                                        meta.height,
                                        meta.fps.as_f64(),
                                        if meta.has_audio { "audio" } else { "muto" }
                                    ));
                                })
                                .response;
                            // Doppio click: anteprima nel player (sostituisce
                            // il vecchio pulsante "Anteprima"). Trascinamento:
                            // droppato sulla timeline aggiunge il media
                            // (sostituisce il vecchio pulsante "Aggiungi"),
                            // vedi `dnd_release_payload` in show_timeline.
                            let interact_id = ui.id().with("media_pool_item").with(id);
                            let resp = ui
                                .interact(group_resp.rect, interact_id, egui::Sense::click_and_drag())
                                .on_hover_text(
                                    "Doppio click: anteprima · trascina sulla timeline per aggiungere",
                                );
                            resp.dnd_set_drag_payload(id);
                            if resp.double_clicked() {
                                preview_action = Some(id);
                            }
                            // "Ghost" che segue il cursore durante il
                            // trascinamento: senza, non c'era alcun feedback
                            // visivo che il drag fosse partito (l'elemento
                            // del media pool resta al suo posto, invariato).
                            if resp.dragged()
                                && let Some(pos) = ui.input(|i| i.pointer.hover_pos())
                            {
                                egui::Area::new(interact_id.with("drag_ghost"))
                                    .order(egui::Order::Tooltip)
                                    .fixed_pos(pos + egui::vec2(12.0, 12.0))
                                    .interactable(false)
                                    .show(ui.ctx(), |ui| {
                                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                                            // Niente icona "🎬" davanti: stesso
                                            // motivo del label play/pause più
                                            // sopra (Unicode astral-plane non
                                            // renderizzato su alcune piattaforme).
                                            ui.label(&label);
                                        });
                                    });
                            }
                        }
                    });
            });

        if self.properties_panel_open {
            egui::Panel::right("properties")
                .resizable(true)
                .default_size(300.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Proprietà");
                        // "x" ASCII invece di "✕" Unicode: stesso motivo
                        // del label play/pause qui sopra.
                        if ui
                            .small_button("x")
                            .on_hover_text("Nascondi pannello")
                            .clicked()
                        {
                            self.properties_panel_open = false;
                        }
                    });
                    ui.separator();
                    // Senza, il pannello si restringerebbe alla larghezza
                    // del contenuto (etichette/slider) invece di riempire
                    // quella assegnata dal Panel — stessa causa del bug
                    // "il resize del pannello torna indietro al rilascio",
                    // vedi il commento in timeline_ui::show_timeline.
                    ui.set_min_width(ui.available_width());

                    let selected_count = self.timeline_state.selected.len();
                    if let Some((track_index, clip_id)) = single_selected {
                        ui.heading("Clip selezionata");

                        let clip_info = self.timeline_id.and_then(|timeline_id| {
                            self.project.timelines[timeline_id]
                                .tracks
                                .get(track_index)?
                                .clips
                                .iter()
                                .find(|c| c.id == clip_id)
                                .map(|c| ClipPanelInfo {
                                    start: c.timeline_start,
                                    len: c.timeline_len(),
                                    is_solid_color: matches!(
                                        c.source,
                                        vv_core::ClipSource::SolidColor
                                    ),
                                    transform_constant: c.effects.transform.is_constant(),
                                    transform_kf_here: c
                                        .effects
                                        .transform
                                        .keyframe_at(source_frame)
                                        .is_some(),
                                    transform: c.effects.transform.value_at(source_frame),
                                    gain_constant: c.effects.gain_db.is_constant(),
                                    gain_kf_here: c
                                        .effects
                                        .gain_db
                                        .keyframe_at(source_frame)
                                        .is_some(),
                                    gain: c.effects.gain_db.value_at(source_frame),
                                    color_constant: c
                                        .effects
                                        .color
                                        .as_ref()
                                        .is_none_or(|k| k.is_constant()),
                                    color_kf_here: c
                                        .effects
                                        .color
                                        .as_ref()
                                        .and_then(|k| k.keyframe_at(source_frame))
                                        .is_some(),
                                    color: c
                                        .effects
                                        .color
                                        .as_ref()
                                        .map(|k| k.value_at(source_frame))
                                        .unwrap_or(vv_core::Rgba {
                                            r: 0.6,
                                            g: 0.6,
                                            b: 0.6,
                                            a: 1.0,
                                        }),
                                })
                        });

                        if let Some(ClipPanelInfo {
                            start,
                            len,
                            is_solid_color,
                            transform_constant,
                            transform_kf_here,
                            mut transform,
                            gain_constant,
                            gain_kf_here,
                            mut gain,
                            color_constant,
                            color_kf_here,
                            mut color,
                        }) = clip_info
                        {
                            ui.label(format!("Track: {track_index}"));
                            ui.label(format!("Start: {start} frame"));
                            ui.label(format!("Durata: {len} frame"));
                            ui.label(format!("Frame corrente (source): {source_frame}"));

                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label("Crop / zoom / posizione");
                                if keyframe_button(ui, transform_constant, transform_kf_here)
                                    .clicked()
                                {
                                    pending_effect = Some(if transform_kf_here {
                                        PendingEffectChange::RemoveTransformKeyframe(
                                            track_index,
                                            clip_id,
                                            source_frame,
                                        )
                                    } else {
                                        PendingEffectChange::UpsertTransformKeyframe(
                                            track_index,
                                            clip_id,
                                            source_frame,
                                            transform,
                                        )
                                    });
                                }
                            });
                            if !transform_constant {
                                ui.small(format!(
                                    "{} keyframe · animato",
                                    self.timeline_id
                                        .and_then(|tid| self.project.timelines[tid].tracks
                                            [track_index]
                                            .clips
                                            .iter()
                                            .find(|c| c.id == clip_id))
                                        .map(|c| c.effects.transform.keyframes().len())
                                        .unwrap_or(0)
                                ));
                            }

                            let mut transform_changed = false;
                            transform_changed |= ui
                                .add(
                                    egui::Slider::new(&mut transform.crop[0], 0.0..=0.99)
                                        .text("sinistra"),
                                )
                                .changed();
                            transform_changed |= ui
                                .add(
                                    egui::Slider::new(&mut transform.crop[1], 0.0..=0.99)
                                        .text("alto"),
                                )
                                .changed();
                            transform_changed |= ui
                                .add(
                                    egui::Slider::new(&mut transform.crop[2], 0.01..=1.0)
                                        .text("destra"),
                                )
                                .changed();
                            transform_changed |= ui
                                .add(
                                    egui::Slider::new(&mut transform.crop[3], 0.01..=1.0)
                                        .text("basso"),
                                )
                                .changed();
                            // Margine minimo per non invertire il rettangolo di crop.
                            transform.crop[0] = transform.crop[0].min(transform.crop[2] - 0.01);
                            transform.crop[1] = transform.crop[1].min(transform.crop[3] - 0.01);

                            transform_changed |= ui
                                .add(egui::Slider::new(&mut transform.zoom, 0.1..=5.0).text("zoom"))
                                .changed();
                            transform_changed |= ui
                                .add(
                                    egui::Slider::new(&mut transform.position[0], -1.0..=1.0)
                                        .text("posizione X"),
                                )
                                .changed();
                            transform_changed |= ui
                                .add(
                                    egui::Slider::new(&mut transform.position[1], -1.0..=1.0)
                                        .text("posizione Y"),
                                )
                                .changed();
                            if ui.button("Reset transform").clicked() {
                                transform = vv_core::Transform::default();
                                transform_changed = true;
                            }
                            if transform_changed {
                                pending_effect = Some(if transform_constant {
                                    PendingEffectChange::SetTransformDefault(
                                        track_index,
                                        clip_id,
                                        transform,
                                    )
                                } else {
                                    PendingEffectChange::UpsertTransformKeyframe(
                                        track_index,
                                        clip_id,
                                        source_frame,
                                        transform,
                                    )
                                });
                            }

                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.label("Gain audio (dB)");
                                if keyframe_button(ui, gain_constant, gain_kf_here).clicked() {
                                    pending_effect = Some(if gain_kf_here {
                                        PendingEffectChange::RemoveGainKeyframe(
                                            track_index,
                                            clip_id,
                                            source_frame,
                                        )
                                    } else {
                                        PendingEffectChange::UpsertGainKeyframe(
                                            track_index,
                                            clip_id,
                                            source_frame,
                                            gain,
                                        )
                                    });
                                }
                            });
                            if ui
                                .add(egui::Slider::new(&mut gain, -60.0..=12.0).text("dB"))
                                .changed()
                            {
                                pending_effect = Some(if gain_constant {
                                    PendingEffectChange::SetGainDefault(track_index, clip_id, gain)
                                } else {
                                    PendingEffectChange::UpsertGainKeyframe(
                                        track_index,
                                        clip_id,
                                        source_frame,
                                        gain,
                                    )
                                });
                            }

                            if is_solid_color {
                                ui.separator();
                                ui.horizontal(|ui| {
                                    ui.label("Colore");
                                    if keyframe_button(ui, color_constant, color_kf_here).clicked()
                                    {
                                        pending_effect = Some(if color_kf_here {
                                            PendingEffectChange::RemoveColorKeyframe(
                                                track_index,
                                                clip_id,
                                                source_frame,
                                            )
                                        } else {
                                            PendingEffectChange::UpsertColorKeyframe(
                                                track_index,
                                                clip_id,
                                                source_frame,
                                                color,
                                            )
                                        });
                                    }
                                });
                                let mut rgba = [color.r, color.g, color.b, color.a];
                                if ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed() {
                                    color = vv_core::Rgba {
                                        r: rgba[0],
                                        g: rgba[1],
                                        b: rgba[2],
                                        a: rgba[3],
                                    };
                                    pending_effect = Some(if color_constant {
                                        PendingEffectChange::SetColorDefault(
                                            track_index,
                                            clip_id,
                                            color,
                                        )
                                    } else {
                                        PendingEffectChange::UpsertColorKeyframe(
                                            track_index,
                                            clip_id,
                                            source_frame,
                                            color,
                                        )
                                    });
                                }
                            }
                        }
                    } else if selected_count > 1 {
                        ui.heading(format!("{selected_count} clip selezionate"));
                        ui.separator();
                        if let Some(timeline_id) = self.timeline_id {
                            for &(track_index, clip_id) in &self.timeline_state.selected {
                                if let Some(clip) = self.project.timelines[timeline_id]
                                    .tracks
                                    .get(track_index)
                                    .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
                                {
                                    ui.label(format!(
                                        "Track {track_index} · start {} · durata {} frame",
                                        clip.timeline_start,
                                        clip.timeline_len()
                                    ));
                                }
                            }
                        }
                    } else if let Some(timeline_id) = self.timeline_id {
                        let tl = &self.project.timelines[timeline_id];
                        ui.heading("Timeline");
                        ui.label(tl.name.clone());
                        ui.label(format!(
                            "{}x{} · {:.2} fps",
                            tl.resolution.0,
                            tl.resolution.1,
                            tl.fps.as_f64()
                        ));
                        ui.label(format!("{} track", tl.tracks.len()));
                        let playhead_secs =
                            self.timeline_state.playhead as f64 / tl.fps.as_f64().max(1.0);
                        ui.label(format!(
                            "Playhead: frame {} ({playhead_secs:.2}s)",
                            self.timeline_state.playhead
                        ));
                        ui.small("Nessuna clip selezionata.");
                    } else {
                        ui.label("Importa un media per creare la timeline.");
                    }
                });
        }

        if let Some(id) = preview_action {
            self.preview_media(id);
            // Anteprima "grezza" del media pool: non è (ancora) detto che
            // sia sulla timeline, quindi non ha un transform/gain di clip
            // da applicare, e il playhead non deve strapparcela via al
            // frame successivo.
            self.active_clip = None;
            self.browsing_media = Some(id);
        }
        if let (Some(timeline_id), Some(change)) = (self.timeline_id, pending_effect) {
            let cmd = build_effect_command(timeline_id, change);
            self.history.do_command(&mut self.project, cmd);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            // Barra di toggle subito sotto il player, alla DaVinci Resolve
            // (la barra con gli strumenti sta sotto il viewer, larga
            // quanto lui — non tutta la finestra): nidificata *dentro* la
            // CentralPanel invece che come `Panel::bottom` di primo
            // livello, così reclama una fetta solo di questa colonna
            // centrale (che il pannello proprietà, a destra, non copre).
            // Per ora solo la calamita dello snapping; altri toggle (es.
            // in futuro "ripple" globale on/off) troverebbero posto qui.
            egui::Panel::bottom("view_toggles")
                .default_size(28.0)
                .resizable(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        magnet_toggle(ui, &mut self.snapping_enabled)
                            .on_hover_text("Calamita: aggancia le clip trascinate ai bordi vicini");
                    });
                });
            // Le clip SolidColor non hanno un media da decodificare: il
            // colore (eventualmente keyframeato) va valutato al frame
            // *locale alla clip* sul playhead della timeline. Nessuna clip
            // attiva (vuoto sulla track video) mostra un frame nero allo
            // stesso modo, invece del placeholder testuale o
            // dell'ultimo frame rimasto — come un vero NLE. Non quando si
            // sta sfogliando un media "grezzo" dal media pool
            // (`browsing_media`): lì `active_clip` è `None` di proposito,
            // ma `browsing_decode_ahead`/`frame_texture` mostrano davvero
            // quell'anteprima.
            let solid_color_frame_info = self.timeline_id.and_then(|timeline_id| {
                let tl = &self.project.timelines[timeline_id];
                match self.active_clip {
                    Some((track_index, clip_id)) => {
                        let clip = tl
                            .tracks
                            .get(track_index)?
                            .clips
                            .iter()
                            .find(|c| c.id == clip_id)?;
                        match &clip.source {
                            vv_core::ClipSource::SolidColor => {
                                let local_frame =
                                    (self.timeline_state.playhead - clip.timeline_start).max(0);
                                let rgba = clip
                                    .effects
                                    .color
                                    .as_ref()
                                    .map(|k| k.value_at(local_frame))
                                    .unwrap_or(vv_core::Rgba {
                                        r: 0.0,
                                        g: 0.0,
                                        b: 0.0,
                                        a: 1.0,
                                    });
                                Some((tl.resolution, rgba))
                            }
                            vv_core::ClipSource::Media(_) => None,
                        }
                    }
                    None if self.browsing_media.is_none() => Some((
                        tl.resolution,
                        vv_core::Rgba {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        },
                    )),
                    None => None,
                }
            });

            if let Some(((w, h), rgba)) = solid_color_frame_info {
                // Immagine sintetica generata su CPU (nessun frame
                // decodificato da comporre): niente da guadagnare a
                // tenerla sulla GPU, resta sul path gestito da egui.
                let data = vv_render::solid_color_frame(rgba, w, h);
                let image =
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &data);
                match &mut self.frame_texture {
                    Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.frame_texture = Some(ui.ctx().load_texture(
                            "current-frame",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
                self.last_viewer_frame_kind = Some(ViewerFrameKind::SolidColor);
            } else if let Some((frame, source_frame)) = self.current_video_frame() {
                // Zero-copy (REFACTOR_PIPELINE.md B2): la texture di
                // output resta sulla GPU, registrata/aggiornata
                // direttamente nel renderer di egui-wgpu — nessun
                // readback CPU né re-upload via `egui::ColorImage` come
                // nel path sopra. Richiede il device condiviso con
                // egui-wgpu (`egui_render_state`, sempre presente
                // nell'app reale — vedi `main()`; `None` solo nei test
                // che costruiscono `VibeVideoApp` con `Default` senza una
                // finestra, dove semplicemente non c'è nulla da mostrare
                // per questo frame).
                if let Some(render_state) = self.egui_render_state.clone() {
                    let transform = self
                        .active_clip_effects()
                        .map(|e| e.transform.value_at(source_frame))
                        .unwrap_or_default();
                    let texture = self.compositor.render_frame_to_texture(
                        &frame_provider::as_render_yuv_frame(&frame),
                        &transform,
                        frame.width,
                        frame.height,
                    );
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let mut renderer = render_state.renderer.write();
                    match self.video_texture_id {
                        Some(id) => renderer.update_egui_texture_from_wgpu_texture(
                            &render_state.device,
                            &view,
                            wgpu::FilterMode::Linear,
                            id,
                        ),
                        None => {
                            self.video_texture_id = Some(renderer.register_native_texture(
                                &render_state.device,
                                &view,
                                wgpu::FilterMode::Linear,
                            ));
                        }
                    }
                    drop(renderer);
                    self.video_display_size =
                        Some(egui::vec2(frame.width as f32, frame.height as f32));
                    self.last_viewer_frame_kind = Some(ViewerFrameKind::Video);
                }
            }

            match self.last_viewer_frame_kind {
                Some(ViewerFrameKind::SolidColor) => {
                    if let Some(texture) = &self.frame_texture {
                        let available = ui.available_size();
                        let tex_size = texture.size_vec2();
                        let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                        let display_size = tex_size * scale.max(0.0);
                        ui.centered_and_justified(|ui| {
                            ui.add(
                                egui::Image::from_texture(texture).fit_to_exact_size(display_size),
                            );
                        });
                    }
                }
                Some(ViewerFrameKind::Video) => {
                    if let (Some(id), Some(tex_size)) =
                        (self.video_texture_id, self.video_display_size)
                    {
                        let available = ui.available_size();
                        let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                        let display_size = tex_size * scale.max(0.0);
                        ui.centered_and_justified(|ui| {
                            ui.add(
                                egui::Image::new(egui::load::SizedTexture::new(id, tex_size))
                                    .fit_to_exact_size(display_size),
                            );
                        });
                    }
                }
                None => {
                    if let Some(err) = &self.preview_error {
                        ui.colored_label(egui::Color32::RED, format!("Errore player: {err}"));
                    } else {
                        ui.centered_and_justified(|ui| {
                            ui.label(if self.browsing_media.is_some() || self.active_clip.is_some() {
                                "Decodifica in corso..."
                            } else {
                                "Importa un media, aggiungilo alla timeline e premi Spazio."
                            });
                        });
                    }
                }
            }
        });

        if self.is_timeline_playing()
        {
            ui.ctx().request_repaint();
        }

        // Nota: la causa del bug "il resize di un pannello torna indietro
        // al rilascio" era altrove (ScrollArea/contenuto che si restringe
        // al contenuto invece di riempire lo spazio assegnato, vedi
        // `auto_shrink` in timeline_ui::show_timeline e nel pannello
        // media pool/proprietà qui in main.rs), non la mancanza di
        // repaint. Questo repaint aggiuntivo resta comunque utile per
        // tenere fluide le interazioni di drag in generale (clip nella
        // timeline, resize dei pannelli) quando il player non sta
        // riproducendo e quindi non ci sarebbe altrimenti un repaint
        // continuo.
        if ui
            .ctx()
            .input(|i| i.pointer.any_down() || i.pointer.any_released())
        {
            ui.ctx().request_repaint();
        }
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Argomento opzionale: path di un video da importare subito all'avvio
    // (comodo per debug/smoke test, oltre che per l'uso da riga di comando).
    let startup_path = std::env::args().nth(1).map(PathBuf::from);

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "vibevideo",
        options,
        Box::new(move |cc| {
            // Zoom della timeline con Alt+scroll invece del Ctrl di default.
            cc.egui_ctx
                .options_mut(|o| o.input_options.zoom_modifier = egui::Modifiers::ALT);
            let mut app = VibeVideoApp::default();
            // Aperto subito: aprire lo stream audio blocca per centinaia di ms.
            app.timeline_audio = Some(TimelineAudio::new());
            // Condivide il device/queue wgpu di egui-wgpu invece del
            // device headless indipendente di `Default`: necessario per
            // il path zero-copy del viewer (REFACTOR_PIPELINE.md B2) —
            // una texture creata su un device diverso da quello del
            // renderer egui non può essergli registrata. `NativeOptions`
            // sopra richiede sempre `Renderer::Wgpu`, quindi in pratica
            // questo è sempre `Some`; il fallback al device headless
            // resta solo per non fare panic se eframe cambiasse renderer.
            if let Some(render_state) = cc.wgpu_render_state.clone() {
                app.compositor = vv_render::Compositor::new(
                    std::sync::Arc::new(render_state.device.clone()),
                    std::sync::Arc::new(render_state.queue.clone()),
                );
                app.egui_render_state = Some(render_state);
            }
            if let Some(path) = startup_path {
                app.import_media(path);
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_timeline_with_clip(
        app: &mut VibeVideoApp,
        track_index: usize,
        start: FrameIdx,
        len: FrameIdx,
    ) -> vv_core::ClipId {
        if app.timeline_id.is_none() {
            let id = app.project.timelines.insert(vv_core::Timeline {
                name: "T".into(),
                fps: vv_core::Rational::new(25, 1),
                resolution: (1920, 1080),
                tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
            });
            app.timeline_id = Some(id);
        }
        let clip_id = app.project.alloc_clip_id();
        let clip = vv_core::Clip {
            id: clip_id,
            source: vv_core::ClipSource::SolidColor,
            source_in: 0,
            source_out: len,
            timeline_start: start,
            effects: vv_core::EffectStack::default(),
            linked_group: None,
            audio_stream_index: 0,
        };
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::InsertClip {
                timeline: app.timeline_id.unwrap(),
                track_index,
                clip,
            }),
        );
        clip_id
    }

    /// Due track video sovrapposte (REFACTOR_PIPELINE.md B4): la seconda
    /// (aggiunta con `AddTrack`, quindi in coda a `tracks` — più in alto
    /// di quella di default) ha una clip più corta al centro di quella
    /// della prima. `active_video_clip_at` deve vedere quella in cima dove
    /// c'è, e tornare a quella sotto appena finisce — la stessa
    /// generalizzazione che il player/il viewer usano per seguire il
    /// playhead.
    #[test]
    fn active_video_clip_at_prefers_the_topmost_video_track() {
        let mut app = VibeVideoApp::default();
        let bottom = make_timeline_with_clip(&mut app, 0, 0, 30);
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::AddTrack::new(timeline_id, TrackKind::Video)),
        );
        let top = make_timeline_with_clip(&mut app, 2, 10, 10);

        assert_eq!(app.active_video_clip_at(5), Some((0, bottom)));
        assert_eq!(app.active_video_clip_at(15), Some((2, top)));
        assert_eq!(app.active_video_clip_at(25), Some((0, bottom)));
    }

    #[test]
    fn ripple_delete_selected_shifts_other_tracks_and_selects_clip_under_playhead() {
        let mut app = VibeVideoApp::default();
        let video_a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_b = make_timeline_with_clip(&mut app, 0, 10, 10);
        let audio_a = make_timeline_with_clip(&mut app, 1, 0, 10);
        let _audio_b = make_timeline_with_clip(&mut app, 1, 10, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, video_b)]);
        app.ripple_delete_selected();

        // "Selection follows playhead" (attivo di default) riseleziona
        // quel che ora si trova sotto al playhead (fermo a 0): video_a,
        // che era già lì. Comodo per incatenare più ripple-delete senza
        // dover ricliccare la prossima clip ogni volta.
        assert_eq!(app.timeline_state.selected, BTreeSet::from([(0, video_a)]));
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].id, video_a);
        // La clip audio che partiva allo stesso istante si è spostata a 0
        // anche se sta su un'altra track: comportamento ripple globale.
        assert_eq!(tl.tracks[1].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[0].id, audio_a);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 0);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 0);
    }

    #[test]
    fn ripple_delete_selected_clears_selection_when_follow_playhead_disabled() {
        let mut app = VibeVideoApp {
            selection_follows_playhead: false,
            ..VibeVideoApp::default()
        };
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_b = make_timeline_with_clip(&mut app, 0, 10, 10);

        app.timeline_state.selected = BTreeSet::from([(0, video_b)]);
        app.ripple_delete_selected();

        assert!(app.timeline_state.selected.is_empty());
    }

    /// Multi-selezione: cancellare due clip non adiacenti insieme (bug
    /// "voglio selezionare più clip con ctrl+click/shift+click") deve
    /// chiudere entrambi i gap correttamente, non solo il primo.
    #[test]
    fn delete_selected_removes_every_selected_clip() {
        let mut app = VibeVideoApp::default();
        let a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let b = make_timeline_with_clip(&mut app, 0, 10, 10);
        let c = make_timeline_with_clip(&mut app, 0, 20, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, a), (0, c)]);
        app.delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].id, b);
        assert_eq!(
            tl.tracks[0].clips[0].timeline_start, 10,
            "delete normale non shifta nulla"
        );
    }

    /// Ripple-delete con due clip selezionate non adiacenti: elaborandole
    /// da destra a sinistra (per `timeline_start` decrescente), ogni
    /// rimozione non deve alterare la posizione, già calcolata, dell'altra
    /// non ancora processata — altrimenti si otterrebbe un doppio
    /// spostamento o un gap non chiuso correttamente.
    #[test]
    fn ripple_delete_selected_multiple_clips_closes_every_gap() {
        let mut app = VibeVideoApp::default();
        let a = make_timeline_with_clip(&mut app, 0, 0, 10); // [0,10)
        let b = make_timeline_with_clip(&mut app, 0, 10, 10); // [10,20), da rimuovere
        let c = make_timeline_with_clip(&mut app, 0, 20, 10); // [20,30)
        let d = make_timeline_with_clip(&mut app, 0, 30, 10); // [30,40), da rimuovere
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, b), (0, d)]);
        app.ripple_delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.tracks[0].clips[0].id, a);
        assert_eq!(tl.tracks[0].clips[0].timeline_start, 0);
        assert_eq!(tl.tracks[0].clips[1].id, c);
        assert_eq!(
            tl.tracks[0].clips[1].timeline_start, 10,
            "c deve scivolare fino a chiudere il gap lasciato da b, non restare a 20 né finire oltre"
        );

        // Un solo undo ripristina tutto: entrambe le clip e le posizioni originali.
        app.history.undo(&mut app.project);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 4);
        assert_eq!(tl.tracks[0].clips[2].id, c);
        assert_eq!(tl.tracks[0].clips[2].timeline_start, 20);
    }

    #[test]
    fn delete_selected_does_not_shift_other_tracks() {
        let mut app = VibeVideoApp::default();
        let video_a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_b = make_timeline_with_clip(&mut app, 0, 10, 10);
        make_timeline_with_clip(&mut app, 1, 0, 10);
        make_timeline_with_clip(&mut app, 1, 10, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, video_b)]);
        app.delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].id, video_a);
        assert_eq!(tl.tracks[1].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 10);
    }

    /// La clip media sotto al playhead diventa attiva e il gain impostato
    /// via comando si legge dai suoi effetti.
    #[test]
    fn loading_a_media_clip_sets_active_clip_and_gain() {
        let dir = std::env::temp_dir().join("vv-app-main-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1",
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

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        let media_id = app
            .project
            .media_pool
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("media importato atteso nel pool");

        app.add_media_to_timeline(media_id);
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);
        app.ensure_active_clip_matches_playhead(false);

        assert_eq!(app.active_clip, Some((0, clip_id)));
        assert_eq!(
            app.active_clip_effects().map(|e| e.gain_db.default),
            Some(0.0)
        );

        self_test_set_gain(&mut app, timeline_id, 0, clip_id, -12.0);
        assert_eq!(
            app.active_clip_effects().map(|e| e.gain_db.default),
            Some(-12.0)
        );
    }

    fn self_test_set_gain(
        app: &mut VibeVideoApp,
        timeline_id: TimelineId,
        track_index: usize,
        clip_id: vv_core::ClipId,
        db: f32,
    ) {
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::SetClipGain::new(
                timeline_id,
                track_index,
                clip_id,
                db,
            )),
        );
    }

    #[test]
    fn build_effect_command_upsert_gain_keyframe_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        let cmd = build_effect_command(
            timeline_id,
            PendingEffectChange::UpsertGainKeyframe(0, clip_id, 5, -9.0),
        );
        app.history.do_command(&mut app.project, cmd);

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(
            clip.effects.gain_db.keyframe_at(5),
            Some((-9.0, vv_core::Interpolation::Linear))
        );
    }

    #[test]
    fn build_effect_command_remove_transform_keyframe_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        let transform = vv_core::Transform {
            zoom: 2.5,
            ..vv_core::Transform::default()
        };
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::UpsertTransformKeyframe(0, clip_id, 3, transform),
            ),
        );
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::RemoveTransformKeyframe(0, clip_id, 3),
            ),
        );

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(clip.effects.transform.is_constant());
    }

    #[test]
    fn build_effect_command_set_defaults_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::SetGainDefault(0, clip_id, -3.0),
            ),
        );
        let transform = vv_core::Transform {
            zoom: 1.5,
            ..vv_core::Transform::default()
        };
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::SetTransformDefault(0, clip_id, transform),
            ),
        );

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(clip.effects.gain_db.default, -3.0);
        assert_eq!(clip.effects.transform.default.zoom, 1.5);
    }

    #[test]
    fn add_solid_color_clip_creates_timeline_and_initialized_color() {
        let mut app = VibeVideoApp::default();
        assert!(app.timeline_id.is_none());

        app.add_solid_color_clip();

        let timeline_id = app.timeline_id.expect("doveva crearsi una timeline");
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(matches!(clip.source, vv_core::ClipSource::SolidColor));
        assert!(clip.effects.color.is_some());
        assert_eq!(clip.timeline_len(), 125); // 5s a 25fps di default
    }

    #[test]
    fn solid_color_clip_under_the_playhead_becomes_the_active_clip() {
        let mut app = VibeVideoApp::default();
        app.add_solid_color_clip();
        let timeline_id = app.timeline_id.unwrap();
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.ensure_active_clip_matches_playhead(false);

        assert_eq!(app.active_clip, Some((0, clip_id)));
    }

    #[test]
    fn build_effect_command_color_upsert_and_remove_round_trip() {
        let mut app = VibeVideoApp::default();
        app.add_solid_color_clip();
        let timeline_id = app.timeline_id.unwrap();
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        let red = vv_core::Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::UpsertColorKeyframe(0, clip_id, 10, red),
            ),
        );
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(
            clip.effects
                .color
                .as_ref()
                .unwrap()
                .keyframe_at(10)
                .unwrap()
                .0
                .r,
            1.0
        );

        app.history.do_command(
            &mut app.project,
            build_effect_command(
                timeline_id,
                PendingEffectChange::RemoveColorKeyframe(0, clip_id, 10),
            ),
        );
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(clip.effects.color.as_ref().unwrap().is_constant());
    }

    const TEST_FPS: f64 = 25.0;

    fn clock_frame(app: &VibeVideoApp) -> FrameIdx {
        app.timeline_audio
            .as_ref()
            .expect("timeline_audio atteso")
            .position_frame(TEST_FPS)
    }

    /// Simula il mixer arrivato a `frame` senza aspettare in tempo reale.
    fn move_clock_to(app: &mut VibeVideoApp, frame: FrameIdx) {
        app.timeline_audio().seek_frame(frame, TEST_FPS);
    }

    #[test]
    fn moving_the_playhead_while_paused_seeks_the_timeline_clock() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 50);
        app.timeline_audio();

        app.timeline_state.playhead = 25;
        app.ensure_active_clip_matches_playhead(false);
        assert_eq!(clock_frame(&app), 25);

        app.timeline_state.playhead = 10;
        app.ensure_active_clip_matches_playhead(false);
        assert_eq!(clock_frame(&app), 10);
    }

    /// Bug: "il player mi ignora se sposto la playhead mentre riproduce".
    #[test]
    fn scrubbing_during_playback_moves_the_clock_only_when_forced() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 500);
        app.toggle_playback();
        assert!(app.is_timeline_playing());

        app.timeline_state.playhead = 300;
        app.ensure_active_clip_matches_playhead(true);
        let after_scrub = clock_frame(&app);
        assert!((300..310).contains(&after_scrub), "clock={after_scrub}");

        // Playhead mosso da `drive_playback`: nessun seek.
        move_clock_to(&mut app, 100);
        app.timeline_state.playhead = 400;
        app.ensure_active_clip_matches_playhead(false);
        assert!(clock_frame(&app) < 300);
    }

    #[test]
    fn playback_follows_the_clock_across_a_cut() {
        let mut app = VibeVideoApp::default();
        let clip_a = make_timeline_with_clip(&mut app, 0, 0, 25);
        let clip_b = make_timeline_with_clip(&mut app, 0, 25, 25);
        app.toggle_playback();
        assert_eq!(app.active_clip, Some((0, clip_a)));

        move_clock_to(&mut app, 25);
        app.drive_playback();

        assert_eq!(app.timeline_state.playhead, 25);
        assert_eq!(app.active_clip, Some((0, clip_b)));
        assert!(app.is_timeline_playing());
    }

    /// Bug: "la selezione non segue durante la riproduzione".
    #[test]
    fn selection_follows_playhead_during_normal_playback() {
        let mut app = VibeVideoApp::default();
        assert!(app.selection_follows_playhead, "attivo di default");
        let clip_a = make_timeline_with_clip(&mut app, 0, 0, 25);
        let clip_b = make_timeline_with_clip(&mut app, 0, 25, 25);
        app.toggle_playback();
        app.sync_selection_to_playhead();
        assert_eq!(app.timeline_state.selected, BTreeSet::from([(0, clip_a)]));

        move_clock_to(&mut app, 30);
        let playhead_before = app.timeline_state.playhead;
        app.drive_playback();
        assert_ne!(app.timeline_state.playhead, playhead_before);
        app.sync_selection_to_playhead();

        assert_eq!(app.timeline_state.selected, BTreeSet::from([(0, clip_b)]));
    }

    /// Bug: "la riproduzione parte solo se seleziono la clip".
    #[test]
    fn toggle_playback_works_without_any_selection() {
        let mut app = VibeVideoApp::default();
        let clip = make_timeline_with_clip(&mut app, 0, 0, 25);
        assert!(app.timeline_state.selected.is_empty());

        app.toggle_playback();

        assert!(app.is_timeline_playing());
        assert_eq!(app.active_clip, Some((0, clip)));
    }

    /// Bug: "quando la playhead passa su un segmento vuoto, non deve
    /// saltare alla prossima clip ma riprodurre una schermata nera".
    #[test]
    fn playback_runs_through_a_gap_and_picks_up_the_next_clip() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let clip_b = make_timeline_with_clip(&mut app, 0, 20, 10);
        app.toggle_playback();

        move_clock_to(&mut app, 15);
        app.drive_playback();
        assert!(app.is_timeline_playing());
        assert_eq!(app.timeline_state.playhead, 15);
        assert!(app.active_clip.is_none(), "schermo nero nel vuoto");

        move_clock_to(&mut app, 22);
        app.drive_playback();
        assert_eq!(app.active_clip, Some((0, clip_b)));
    }

    #[test]
    fn toggle_playback_starts_from_inside_a_gap_and_pauses_again() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        make_timeline_with_clip(&mut app, 0, 20, 10);
        app.timeline_state.playhead = 15;

        app.toggle_playback();
        assert!(app.is_timeline_playing());
        assert_eq!(clock_frame(&app), 15);

        app.toggle_playback();
        assert!(!app.is_timeline_playing());
    }

    #[test]
    fn toggle_playback_does_nothing_past_the_end_of_the_content() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        app.timeline_state.playhead = 15;

        app.toggle_playback();

        assert!(!app.is_timeline_playing());
    }

    /// Una clip solo audio oltre l'ultima video fa parte del contenuto.
    #[test]
    fn playback_reaches_the_end_of_audio_only_content_and_stops_there() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        make_timeline_with_clip(&mut app, 1, 0, 30);
        app.timeline_state.playhead = 20;
        app.toggle_playback();
        assert!(app.is_timeline_playing());

        move_clock_to(&mut app, 45);
        app.drive_playback();

        assert!(!app.is_timeline_playing());
        assert_eq!(app.timeline_state.playhead, 30);
    }

    /// Come fa `ui()` a ogni frame, finché lo stretch non porta a `speed`.
    fn wait_for_playback_speed(app: &mut VibeVideoApp, speed: f64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.playback_speed != speed {
            assert!(std::time::Instant::now() < deadline, "velocità {speed}x mai applicata");
            std::thread::sleep(std::time::Duration::from_millis(5));
            let audio = app.timeline_audio();
            audio.tick();
            app.playback_speed = audio.speed();
        }
    }

    /// Tasto "a": da fermo parte a 1x, poi 2x -> 4x -> 8x e resta a 8x; la
    /// barra spaziatrice mette sempre in pausa e riporta a 1x.
    #[test]
    fn fast_playback_key_cycles_speed_and_space_always_resets_it() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 5000);

        app.handle_fast_playback_key();
        assert!(app.is_timeline_playing());
        assert_eq!(app.playback_speed, 1.0);

        for expected in [2.0, 4.0, 8.0] {
            app.handle_fast_playback_key();
            wait_for_playback_speed(&mut app, expected);
        }
        app.handle_fast_playback_key();
        assert_eq!(app.playback_speed, 8.0, "oltre 8x resta a 8x");

        app.toggle_playback();
        assert!(!app.is_timeline_playing());
        assert_eq!(app.playback_speed, 1.0, "la pausa riporta a velocità normale");

        app.handle_fast_playback_key();
        assert!(app.is_timeline_playing());
        assert_eq!(app.playback_speed, 1.0);
    }

    #[test]
    fn fast_playback_advances_the_playhead_faster() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 5000);
        app.toggle_playback();
        app.request_playback_speed(4.0);
        wait_for_playback_speed(&mut app, 4.0);
        app.drive_playback();
        let before = app.timeline_state.playhead;
        std::thread::sleep(std::time::Duration::from_millis(400));
        app.drive_playback();
        // 400ms a 4x = 1.6s = 40 frame.
        let advanced = app.timeline_state.playhead - before;
        assert!((32..=50).contains(&advanced), "advanced={advanced}");
    }

    /// Bug: spostare/mutare una clip audio non cambiava nulla in anteprima
    /// (l'audio veniva sempre dal media della clip video).
    #[test]
    fn preview_mix_follows_audio_clip_edits() {
        let dir = std::env::temp_dir().join("vv-app-preview-mix-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=1",
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

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let timeline_id = app.timeline_id.unwrap();
        let audio_track = app.project.timelines[timeline_id]
            .first_track_index(TrackKind::Audio)
            .unwrap();
        let audio_clip = app.project.timelines[timeline_id].tracks[audio_track].clips[0].id;

        app.timeline_audio();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.sync_timeline_audio();
            if !app.timeline_audio().has_pending_buffers() {
                app.sync_timeline_audio();
                break;
            }
            assert!(std::time::Instant::now() < deadline, "decodifica audio mai arrivata");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let peak = |app: &VibeVideoApp, frame| {
            app.timeline_audio
                .as_ref()
                .unwrap()
                .render(frame, TEST_FPS, 5)
                .iter()
                .fold(0.0f32, |m, s| m.max(s.abs()))
        };
        assert!(peak(&app, 10) > 0.1, "la clip audio deve suonare");
        assert_eq!(peak(&app, 60), 0.0);

        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::MoveClip::new(timeline_id, audio_clip, audio_track, audio_track, 50)),
        );
        app.sync_timeline_audio();
        assert_eq!(peak(&app, 10), 0.0, "la vecchia posizione ora è silenzio");
        assert!(peak(&app, 60) > 0.1, "la clip suona nella nuova posizione");

        // Il fast forward stretcha il mix, non un media.
        app.timeline_state.playhead = 50;
        app.toggle_playback();
        app.request_playback_speed(2.0);
        wait_for_playback_speed(&mut app, 2.0);
        let stretched = app.timeline_audio().stretched_peak().unwrap();
        assert!(stretched > 0.1, "stretched={stretched}");
        app.toggle_playback();

        app.project.timelines[timeline_id].tracks[audio_track].muted = true;
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::MoveClip::new(timeline_id, audio_clip, audio_track, audio_track, 55)),
        );
        app.sync_timeline_audio();
        assert_eq!(peak(&app, 60), 0.0, "track muted");
    }

    #[test]
    fn alt_released_mid_wheel_gesture_stops_zooming() {
        let ctx = egui::Context::default();
        ctx.options_mut(|o| o.input_options.zoom_modifier = egui::Modifiers::ALT);
        let wheel = |phase, modifiers| egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 2.0),
            phase,
            modifiers,
        };
        let mut zooms = Vec::new();
        for (phase, modifiers) in [
            (egui::TouchPhase::Start, egui::Modifiers::ALT),
            (egui::TouchPhase::Move, egui::Modifiers::ALT),
            (egui::TouchPhase::Move, egui::Modifiers::NONE),
        ] {
            let mut raw_input = egui::RawInput {
                events: vec![egui::Event::ModifiersChanged(modifiers), wheel(phase, modifiers)],
                ..Default::default()
            };
            unstick_wheel_modifiers(&mut raw_input);
            ctx.run_ui(raw_input, |_| {}).textures_delta.clear();
            zooms.push(ctx.input(|i| i.zoom_delta()));
        }
        assert_ne!(zooms[1], 1.0, "con Alt lo scroll zooma");
        assert_eq!(zooms[2], 1.0, "rilasciato Alt lo scroll non zooma più");
    }

    /// Senza selezione T taglia tutte le track in un colpo solo.
    #[test]
    fn split_at_playhead_cuts_every_track_without_selection() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        assert!(app.timeline_state.selected.is_empty());
        app.timeline_state.playhead = 8;
        app.split_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2, "track video tagliata");
        assert_eq!(
            tl.tracks[1].clips.len(),
            2,
            "track audio tagliata anche senza selezione"
        );
        assert_eq!(tl.tracks[0].clips[0].id, video_id);
        assert_eq!(tl.tracks[1].clips[0].id, audio_id);

        // Un solo undo annulla entrambi i tagli (CompositeCommand).
        app.history.undo(&mut app.project);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
    }

    #[test]
    fn split_at_playhead_cuts_only_selected_clips() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        make_timeline_with_clip(&mut app, 1, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.set_single_selection(Some((0, video_id)));
        app.timeline_state.playhead = 8;
        app.split_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2, "clip selezionata tagliata");
        assert_eq!(tl.tracks[1].clips.len(), 1, "clip non selezionata intatta");
    }

    /// Bug: tagliare con T una coppia video+audio collegata scollegava
    /// entrambe le metà (comportamento corretto per un taglio "singolo",
    /// ma non quando entrambi i membri della coppia vengono tagliati
    /// insieme nello stesso punto): dopo, selezionare il video non
    /// evidenziava più l'audio. Le metà sinistre restano nel gruppo
    /// originale (SplitClip non lo tocca), le metà destre vengono
    /// ricollegate tra loro in un gruppo nuovo.
    #[test]
    fn split_at_playhead_keeps_linked_group_on_both_halves() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 20);
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                vec![(0, video_id), (1, audio_id)],
            )),
        );

        app.timeline_state.playhead = 8;
        app.split_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips.len(), 2);
        let video_left = &tl.tracks[0].clips[0];
        let video_right = &tl.tracks[0].clips[1];
        let audio_left = &tl.tracks[1].clips[0];
        let audio_right = &tl.tracks[1].clips[1];
        assert_eq!(video_left.id, video_id);
        assert_eq!(audio_left.id, audio_id);
        let left_group = video_left
            .linked_group
            .expect("le metà sinistre restano collegate");
        assert_eq!(audio_left.linked_group, Some(left_group));
        let right_group = video_right
            .linked_group
            .expect("le metà destre vengono ricollegate tra loro");
        assert_eq!(audio_right.linked_group, Some(right_group));
        assert_ne!(
            left_group, right_group,
            "le metà destre hanno un gruppo nuovo, non quello della sinistra"
        );
        assert_ne!(video_right.id, video_id);
        assert_ne!(audio_right.id, audio_id);

        // "Selection follows playhead" seleziona la metà SINISTRA appena
        // tagliata (quella che si presume già rivista) e la sua gemella
        // audio collegata, non la metà destra sotto al playhead.
        assert_eq!(
            app.timeline_state.selected,
            BTreeSet::from([(0, video_id), (1, audio_id)])
        );

        // Un solo undo annulla il taglio e il ricollegamento delle metà
        // destre: le metà sinistre non erano mai state toccate, restano nel
        // gruppo originale.
        app.history.undo(&mut app.project);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].linked_group, Some(left_group));
        assert_eq!(tl.tracks[1].clips[0].linked_group, Some(left_group));
    }

    #[test]
    fn map_source_ranges_to_timeline_translates_and_clamps_to_the_trim() {
        // Clip: source_in=100, source_out=150 (trim di 50 frame), piazzata
        // a timeline_start=20.
        let clip = vv_core::Clip {
            id: ClipId(0),
            source: vv_core::ClipSource::SolidColor,
            source_in: 100,
            source_out: 150,
            timeline_start: 20,
            effects: vv_core::EffectStack::default(),
            linked_group: None,
            audio_stream_index: 0,
        };

        // Dentro al trim: tradotto 1:1 con l'offset timeline_start-source_in.
        assert_eq!(
            map_source_ranges_to_timeline(&clip, &[(110, 120)]),
            vec![(30, 40)]
        );

        // Sporge da entrambi i lati: accorciato al trim.
        assert_eq!(
            map_source_ranges_to_timeline(&clip, &[(50, 200)]),
            vec![(20, 69)]
        );

        // Completamente fuori dal trim: scartato.
        assert!(map_source_ranges_to_timeline(&clip, &[(0, 99)]).is_empty());

        // Più intervalli: ognuno tradotto/filtrato indipendentemente.
        assert_eq!(
            map_source_ranges_to_timeline(&clip, &[(0, 99), (110, 115), (500, 600)]),
            vec![(30, 35)]
        );
    }

    #[test]
    fn delete_selected_removes_every_selected_clip_together() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                vec![(0, video_id), (1, audio_id)],
            )),
        );

        // La selezione contiene già l'intero gruppo, come farebbe un click
        // reale (vedi `timeline_ui::expand_to_linked_groups`):
        // `delete_selected` si fida di questo invariante, non tira dentro
        // esplicitamente i collegamenti.
        app.timeline_state.selected = BTreeSet::from([(0, video_id), (1, audio_id)]);
        app.delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert!(tl.tracks[0].clips.is_empty());
        assert!(tl.tracks[1].clips.is_empty());
        assert!(app.timeline_state.selected.is_empty());

        // Un solo undo ripristina entrambe (CompositeCommand).
        app.history.undo(&mut app.project);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
    }

    #[test]
    fn arrows_step_one_frame_then_scroll_at_half_speed_while_held() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 1000);
        app.timeline_state.playhead = 100;

        app.step_playhead_with_arrows(Some(1), 10.0);
        assert_eq!(app.timeline_state.playhead, 101, "un frame subito");
        app.step_playhead_with_arrows(Some(1), 10.2);
        assert_eq!(app.timeline_state.playhead, 101, "prima del ritardo resta lì");
        // 25fps a 0.5x: 1s dopo il ritardo = 12 frame in più.
        app.step_playhead_with_arrows(Some(1), 10.0 + ARROW_HOLD_DELAY_SECS + 1.0);
        assert_eq!(app.timeline_state.playhead, 113);

        app.step_playhead_with_arrows(None, 12.0);
        app.step_playhead_with_arrows(Some(-1), 12.1);
        assert_eq!(app.timeline_state.playhead, 112, "rilasciata, un nuovo passo singolo");
    }

    #[test]
    fn copy_then_paste_creates_a_new_clip_at_the_playhead_with_a_new_id() {
        let mut app = VibeVideoApp::default();
        let original_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, original_id)]);
        app.copy_selected_clips();
        assert_eq!(app.timeline_state.clipboard.len(), 1);

        app.timeline_state.playhead = 50;
        app.paste_clipboard_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2, "l'originale + l'incollata");
        let pasted = &tl.tracks[0].clips[1];
        assert_ne!(pasted.id, original_id, "un id nuovo, non lo stesso");
        assert_eq!(pasted.timeline_start, 50, "incollata al playhead");
        assert_eq!(pasted.timeline_len(), 10);
        assert_eq!(
            app.timeline_state.selected,
            BTreeSet::from([(0, pasted.id)]),
            "la clip incollata diventa la selezione"
        );
        assert_eq!(app.timeline_state.playhead, 60, "testina in fondo all'incollata");
    }

    #[test]
    fn copy_then_paste_relinks_a_linked_group_to_each_other_not_to_the_originals() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 10);
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                vec![(0, video_id), (1, audio_id)],
            )),
        );

        // Selezione già completa (come da un click reale sul gruppo, vedi
        // `timeline_ui::expand_to_linked_groups`).
        app.timeline_state.selected = BTreeSet::from([(0, video_id), (1, audio_id)]);
        app.copy_selected_clips();
        assert_eq!(app.timeline_state.clipboard.len(), 2);

        // Playhead spostato oltre gli originali: qui si vuole verificare
        // solo il ricollegamento, non l'"overwrite" di `make_room_for_ranges`
        // (che ha un test dedicato più sotto).
        app.timeline_state.playhead = 100;
        app.paste_clipboard_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        let new_video = &tl.tracks[0].clips[1];
        let new_audio = &tl.tracks[1].clips[1];
        let new_group = new_video
            .linked_group
            .expect("le clip incollate restano collegate tra loro");
        assert_eq!(new_audio.linked_group, Some(new_group));
        assert_ne!(
            Some(new_group),
            tl.tracks[0].clips[0].linked_group,
            "non collegata al gruppo originale"
        );
    }

    #[test]
    fn copy_then_paste_multiple_clips_preserves_their_relative_spacing() {
        let mut app = VibeVideoApp::default();
        let a_id = make_timeline_with_clip(&mut app, 0, 0, 10); // [0,10)
        let b_id = make_timeline_with_clip(&mut app, 0, 20, 10); // [20,30), 10 frame di gap da "a"
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, a_id), (0, b_id)]);
        app.copy_selected_clips();

        app.timeline_state.playhead = 100;
        app.paste_clipboard_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 4);
        let pasted: Vec<_> = tl.tracks[0].clips[2..].iter().collect();
        let starts: BTreeSet<FrameIdx> = pasted.iter().map(|c| c.timeline_start).collect();
        // "a" incollata a 100 (ancora = inizio più a sinistra), "b" 20
        // frame dopo, esattamente come nell'originale.
        assert_eq!(starts, BTreeSet::from([100, 120]));
    }

    #[test]
    fn paste_with_an_empty_clipboard_is_a_no_op() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let timeline_id = app.timeline_id.unwrap();

        assert!(app.timeline_state.clipboard.is_empty());
        app.paste_clipboard_at_playhead();

        assert_eq!(app.project.timelines[timeline_id].tracks[0].clips.len(), 1);
    }

    /// Bug segnalato: "il copia-incolla funziona solo dal menu, non da
    /// tastiera". Causa: `egui-winit` genera `Event::Paste` solo se la
    /// clipboard di *sistema* non è vuota (vedi doc di
    /// `handle_clipboard_events`) — questo test verifica che un
    /// `Event::Copy` scriva sempre qualcosa di non vuoto lì, così un
    /// successivo Ctrl+V da tastiera possa davvero generare l'evento.
    /// Gira dentro un `egui::Context` "nudo" (`run_ui`), non il vero
    /// `eframe::Frame` di `ui()` (non costruibile fuori da `eframe`):
    /// stesso trucco già usato per `show_timeline` in `timeline_ui.rs`.
    #[test]
    fn handle_clipboard_events_primes_the_system_clipboard_after_a_copy() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.handle_clipboard_events(ui, &[egui::Event::Copy]);
        });
        output.textures_delta.clear();

        assert_eq!(app.timeline_state.clipboard.len(), 1);
        let copied_something_non_empty = output
            .platform_output
            .commands
            .iter()
            .any(|cmd| matches!(cmd, egui::OutputCommand::CopyText(text) if !text.is_empty()));
        assert!(
            copied_something_non_empty,
            "doveva scrivere qualcosa di non vuoto nella clipboard di sistema"
        );
    }

    /// Senza nulla di selezionato, `copy_selected_clips` è un no-op: non
    /// deve nemmeno toccare la clipboard di sistema (altrimenti Ctrl+C a
    /// vuoto cancellerebbe silenziosamente quel che l'utente avesse
    /// eventualmente copiato altrove per incollarlo in un'altra app).
    #[test]
    fn handle_clipboard_events_does_not_touch_system_clipboard_when_nothing_is_selected() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.handle_clipboard_events(ui, &[egui::Event::Copy]);
        });
        output.textures_delta.clear();

        assert!(app.timeline_state.clipboard.is_empty());
        assert!(
            output.platform_output.commands.is_empty(),
            "senza nulla da copiare non deve toccare la clipboard di sistema"
        );
    }

    /// `Event::Paste` va gestito a prescindere dal suo payload testuale
    /// (la clipboard vera è `timeline_state.clipboard`, non il testo di
    /// sistema): incolla comunque quel che avevamo già copiato.
    #[test]
    fn handle_clipboard_events_pastes_regardless_of_the_paste_events_payload() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);
        app.copy_selected_clips();
        app.timeline_state.playhead = 100;

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.handle_clipboard_events(ui, &[egui::Event::Paste("qualsiasi cosa".to_owned())]);
        });
        output.textures_delta.clear();

        let timeline_id = app.timeline_id.unwrap();
        assert_eq!(app.project.timelines[timeline_id].tracks[0].clips.len(), 2);
    }

    /// Bug segnalato: incollare una clip sopra un'altra la copriva solo
    /// visivamente, ma il player continuava a riprodurre quella
    /// sottostante. Se il nuovo intervallo copre *interamente* una clip
    /// esistente, quella va rimossa del tutto (`make_room_for_ranges`).
    #[test]
    fn paste_over_an_existing_clip_it_fully_covers_deletes_the_underlying_clip() {
        let mut app = VibeVideoApp::default();
        let existing = make_timeline_with_clip(&mut app, 0, 0, 10); // [0,10)
        let source = make_timeline_with_clip(&mut app, 0, 50, 10); // da copiare, stessa lunghezza
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, source)]);
        app.copy_selected_clips();
        app.timeline_state.playhead = 0;
        app.paste_clipboard_at_playhead(); // nuovo range [0,10), copre "existing" per intero

        let tl = &app.project.timelines[timeline_id];
        assert!(
            tl.tracks[0].clips.iter().all(|c| c.id != existing),
            "la clip completamente coperta doveva essere rimossa"
        );
        // "source" originale (a 50) + la nuova clip incollata (a 0).
        assert_eq!(tl.tracks[0].clips.len(), 2);
    }

    /// La coda di una clip esistente sporge oltre l'inizio della nuova
    /// clip incollata: va accorciata lì (bordo destro), non rimossa né
    /// lasciata sovrapposta.
    #[test]
    fn paste_overlapping_the_tail_of_an_existing_clip_trims_its_end() {
        let mut app = VibeVideoApp::default();
        let existing = make_timeline_with_clip(&mut app, 0, 0, 10); // [0,10)
        let source = make_timeline_with_clip(&mut app, 0, 50, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, source)]);
        app.copy_selected_clips();
        app.timeline_state.playhead = 5;
        app.paste_clipboard_at_playhead(); // nuovo range [5,15)

        let tl = &app.project.timelines[timeline_id];
        let trimmed = tl.tracks[0]
            .clips
            .iter()
            .find(|c| c.id == existing)
            .expect("doveva restare, solo accorciata");
        assert_eq!(trimmed.timeline_start, 0);
        assert_eq!(trimmed.timeline_end(), 5);
    }

    /// La testa di una clip esistente sporge prima della fine della nuova
    /// clip incollata: va accorciata lì (bordo sinistro).
    #[test]
    fn paste_overlapping_the_head_of_an_existing_clip_trims_its_start() {
        let mut app = VibeVideoApp::default();
        let existing = make_timeline_with_clip(&mut app, 0, 10, 10); // [10,20)
        let source = make_timeline_with_clip(&mut app, 0, 50, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, source)]);
        app.copy_selected_clips();
        app.timeline_state.playhead = 5;
        app.paste_clipboard_at_playhead(); // nuovo range [5,15)

        let tl = &app.project.timelines[timeline_id];
        let trimmed = tl.tracks[0]
            .clips
            .iter()
            .find(|c| c.id == existing)
            .expect("doveva restare, solo accorciata");
        assert_eq!(trimmed.timeline_start, 15);
        assert_eq!(trimmed.timeline_end(), 20);
    }

    /// La nuova clip incollata cade interamente nel mezzo di una clip
    /// esistente più lunga: quella va divisa in due, con il pezzo centrale
    /// (coperto) che sparisce.
    #[test]
    fn paste_inside_an_existing_clip_splits_it_in_two() {
        let mut app = VibeVideoApp::default();
        let existing = make_timeline_with_clip(&mut app, 0, 0, 20); // [0,20)
        let source = make_timeline_with_clip(&mut app, 0, 50, 5);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, source)]);
        app.copy_selected_clips();
        app.timeline_state.playhead = 8;
        app.paste_clipboard_at_playhead(); // nuovo range [8,13)

        // Oltre a "existing" (destinata a dividersi) e alla clip appena
        // incollata (a 8), resta in giro anche "source" (a 50, mai
        // toccata: è il sorgente della copia, non sovrapposto a nulla).
        // I due pezzi attesi sono esattamente a 0 e 13.
        let tl = &app.project.timelines[timeline_id];
        let mut halves: Vec<_> = tl.tracks[0]
            .clips
            .iter()
            .filter(|c| c.timeline_start == 0 || c.timeline_start == 13)
            .collect();
        halves.sort_by_key(|c| c.timeline_start);
        assert_eq!(halves.len(), 2, "la clip originale doveva dividersi in due");
        assert_eq!(
            halves[0].id, existing,
            "la metà sinistra mantiene l'id originale (comportamento di SplitClip)"
        );
        assert_eq!(halves[0].timeline_end(), 8);
        assert_eq!(halves[1].timeline_start, 13);
        assert_eq!(halves[1].timeline_end(), 20);
    }

    /// Se lo split coinvolge un gruppo collegato (paste della coppia
    /// video+audio copiata insieme, che quindi taglia entrambe le track
    /// nello stesso punto), le due metà nuove devono restare collegate
    /// *tra loro*, non alla vecchia gemella (persa nello split).
    #[test]
    fn paste_splitting_a_linked_group_relinks_the_new_halves_to_each_other() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20); // [0,20)
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 20); // [0,20)
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                vec![(0, video_id), (1, audio_id)],
            )),
        );

        let src_video = make_timeline_with_clip(&mut app, 0, 100, 5);
        let src_audio = make_timeline_with_clip(&mut app, 1, 100, 5);
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                vec![(0, src_video), (1, src_audio)],
            )),
        );
        // Selezione già completa (come da un click reale sul gruppo).
        app.timeline_state.selected = BTreeSet::from([(0, src_video), (1, src_audio)]);
        app.copy_selected_clips();
        assert_eq!(app.timeline_state.clipboard.len(), 2);

        app.timeline_state.playhead = 8;
        app.paste_clipboard_at_playhead(); // nuovo range [8,13) su entrambe le track

        // Oltre alle due metà (a 0 e 13), restano in giro anche
        // "src_video"/"src_audio" (a 100, mai toccate: sono il sorgente
        // della copia).
        let tl = &app.project.timelines[timeline_id];
        let mut video_halves: Vec<_> = tl.tracks[0]
            .clips
            .iter()
            .filter(|c| c.timeline_start == 0 || c.timeline_start == 13)
            .collect();
        video_halves.sort_by_key(|c| c.timeline_start);
        let mut audio_halves: Vec<_> = tl.tracks[1]
            .clips
            .iter()
            .filter(|c| c.timeline_start == 0 || c.timeline_start == 13)
            .collect();
        audio_halves.sort_by_key(|c| c.timeline_start);

        assert_eq!(video_halves.len(), 2);
        assert_eq!(audio_halves.len(), 2);
        let left_group = video_halves[0]
            .linked_group
            .expect("la metà sinistra resta nel gruppo originale");
        assert_eq!(audio_halves[0].linked_group, Some(left_group));
        let right_group = video_halves[1]
            .linked_group
            .expect("la metà destra viene ricollegata alla sua gemella");
        assert_eq!(audio_halves[1].linked_group, Some(right_group));
        assert_ne!(left_group, right_group);
    }

    /// Richiesta: "inserisci un indicatore visivo delle porzioni di
    /// timeline presenti in memoria". Verifica l'integrazione end-to-end
    /// (non solo la funzione pura `map_source_ranges_to_timeline`, già
    /// coperta a parte): con `render_ahead` reale in corso su un thread
    /// separato, la clip sotto al playhead produce intervalli bufferizzati
    /// entro i propri limiti di timeline.
    #[test]
    fn buffered_timeline_ranges_reports_the_clip_under_the_playheads_decoded_frames() {
        let dir = std::env::temp_dir().join("vv-app-buffered-ranges-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
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

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let timeline_id = app.timeline_id.unwrap();
        let clip = app.project.timelines[timeline_id].tracks[0].clips[0].clone();

        // Manda a `render_ahead` la clip appena inserita (nell'app vera
        // succede a ogni frame UI via `sync_render_ahead`).
        app.sync_render_ahead();

        let start = std::time::Instant::now();
        loop {
            if !app.buffered_timeline_ranges().is_empty() {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "timeout: nessun frame bufferizzato entro 2s"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        for (s, e) in app.buffered_timeline_ranges() {
            assert!(
                s >= clip.timeline_start && e < clip.timeline_end(),
                "range {s}..{e} fuori dai limiti della clip ({}..{})",
                clip.timeline_start,
                clip.timeline_end()
            );
        }
    }

    /// `proxy_timeline_ranges` copre l'intera clip appena il proxy è
    /// pronto su disco (non solo la parte già bufferizzata, a
    /// differenza di `buffered_timeline_ranges` — vedi doc del metodo),
    /// è vuoto finché non lo è, ed è vuoto a prescindere se il toggle è
    /// disattivato.
    #[test]
    fn proxy_timeline_ranges_covers_the_whole_clip_once_the_proxy_is_ready() {
        let dir = std::env::temp_dir().join("vv-app-proxy-ranges-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
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

        // Fingerprint calcolato prima dell'import vero e proprio, solo per
        // ripulire un eventuale proxy rimasto da un run precedente di
        // questo stesso test con lo stesso content_hash (path+dimensione+
        // mtime coincidenti) — altrimenti l'asserzione "vuoto subito dopo
        // l'import" sotto sarebbe fragile, non per una vera race ma per
        // stato residuo su disco.
        let content_hash = vv_media::content_fingerprint(&path).unwrap();
        let _ = std::fs::remove_file(vv_media::proxy::proxy_path_for(content_hash));

        let mut app = VibeVideoApp::default();
        app.import_media(path); // accoda anche la generazione del proxy
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let timeline_id = app.timeline_id.unwrap();
        let clip = app.project.timelines[timeline_id].tracks[0].clips[0].clone();

        assert!(
            app.proxy_timeline_ranges().is_empty(),
            "il proxy non può già essere pronto subito dopo l'import"
        );

        let start = std::time::Instant::now();
        loop {
            if vv_media::proxy::proxy_exists(content_hash) {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "timeout: proxy mai generato"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(
            app.proxy_timeline_ranges(),
            vec![(clip.timeline_start, clip.timeline_end() - 1)],
            "con il proxy pronto e il toggle attivo, deve coprire l'intera clip"
        );

        app.proxy_enabled = false;
        assert!(
            app.proxy_timeline_ranges().is_empty(),
            "col toggle disattivato non deve segnalare nulla, anche col proxy pronto"
        );
    }

    /// Test end-to-end del bug segnalato ("il buffer si ferma sempre al
    /// bordo della clip successiva"): a differenza del vecchio sistema
    /// per-clip, `render_ahead` deve bufferizzare *oltre* la fine della
    /// clip attiva, dentro la clip successiva, PRIMA che il playhead la
    /// raggiunga — un taglio netto tra due media diversi, nessun caso
    /// speciale necessario.
    #[test]
    fn buffered_timeline_ranges_covers_the_next_clip_before_the_playhead_reaches_it() {
        let dir = std::env::temp_dir().join("vv-app-buffered-ranges-cut-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("clip_a.mp4");
        let path_b = dir.join("clip_b.mp4");
        for (path, duration) in [(&path_a, 2), (&path_b, 1)] {
            let status = std::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc=size=320x240:rate=25:duration={duration}"),
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    path.to_str().unwrap(),
                ])
                .status()
                .expect("ffmpeg CLI non trovato");
            assert!(status.success());
        }

        let mut app = VibeVideoApp::default();
        app.import_media(path_a);
        app.import_media(path_b);
        let mut media_ids = app.project.media_pool.iter().map(|(id, _)| id);
        let media_a = media_ids.next().unwrap();
        let media_b = media_ids.next().unwrap();
        drop(media_ids);

        app.add_media_to_timeline_at(media_a, 0, timeline_ui::MediaDropTarget::Default); // [0,50)
        app.add_media_to_timeline_at(media_b, 50, timeline_ui::MediaDropTarget::Default); // [50,75), adiacente
        let timeline_id = app.timeline_id.unwrap();
        let clip_b = app.project.timelines[timeline_id].tracks[0].clips[1].clone();

        // Playhead vicino alla fine della prima clip: la finestra di
        // lookahead di `render_ahead` (3s) attraversa abbondantemente il
        // taglio a 50.
        app.timeline_state.playhead = 45;
        app.sync_render_ahead();

        let start = std::time::Instant::now();
        loop {
            let covers_next_clip = app
                .buffered_timeline_ranges()
                .iter()
                .any(|&(s, e)| s < clip_b.timeline_end() && e >= clip_b.timeline_start);
            if covers_next_clip {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "timeout: la clip successiva non è mai stata bufferizzata in anticipo"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn save_project_to_then_load_project_from_round_trips_and_resets_ui_state() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let timeline_id = app.timeline_id.unwrap();
        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);
        app.timeline_state.playhead = 5;

        let dir = std::env::temp_dir().join("vv-app-persistence-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("progetto.vvproj");

        app.save_project_to(&path);
        assert!(app.project_error.is_none(), "{:?}", app.project_error);
        assert_eq!(app.current_project_path, Some(path.clone()));

        // Un progetto "nuovo" in memoria (un'altra clip, un'altra
        // selezione/playhead): caricare deve sostituire tutto, non
        // fondere.
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 999);
        app.timeline_state.playhead = 42;

        app.load_project_from(path.clone());
        assert!(app.project_error.is_none(), "{:?}", app.project_error);
        assert_eq!(app.current_project_path, Some(path));

        let loaded_timeline_id = app.timeline_id.expect("timeline attesa dopo il load");
        assert_eq!(
            loaded_timeline_id, timeline_id,
            "stessa TimelineId di prima: SlotMap round-trippa le chiavi"
        );
        assert_eq!(
            app.project.timelines[loaded_timeline_id].tracks[0].clips[0].id,
            clip_id
        );
        // Stato UI del progetto precedente azzerato, non ereditato dal
        // vecchio `app` né rimasto dal progetto appena sovrascritto.
        assert!(app.timeline_state.selected.is_empty());
        assert_eq!(app.timeline_state.playhead, 0);
    }

    #[test]
    fn load_project_from_a_bad_path_sets_project_error_without_touching_the_current_project() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.load_project_from(std::env::temp_dir().join("vv-app-persistence-test/nope.vvproj"));

        assert!(app.project_error.is_some());
        // Il progetto corrente (mai salvato) resta intatto: un load fallito
        // non deve cancellare del lavoro non salvato.
        assert_eq!(app.timeline_id, Some(timeline_id));
        assert_eq!(
            app.project.timelines[timeline_id].tracks[0].clips[0].id,
            clip_id
        );
    }

    /// Un media con *due* stream audio (es. mix stereo + 5.1 separato, il
    /// bug reale che ha motivato `Clip::audio_stream_index`): l'import deve
    /// creare una clip audio per stream, su track audio separate (la
    /// seconda creata al volo, visto che di default la timeline ne ha una
    /// sola), e collegarle tutte insieme (video compreso) nello stesso
    /// gruppo — vedi doc di `insert_media_clip`.
    #[test]
    fn add_media_to_timeline_creates_one_audio_clip_per_audio_stream() {
        let dir = std::env::temp_dir().join("vv-app-multi-audio-import-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("two_audio_streams.mp4");

        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=48000:duration=1",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:a",
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

        let mut app = VibeVideoApp::default();
        app.import_media(path.clone());
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);

        let timeline_id = app.timeline_id.unwrap();
        let tl = &app.project.timelines[timeline_id];

        assert_eq!(
            tl.tracks.len(),
            3,
            "video + 2 track audio (una creata al volo per il secondo stream)"
        );
        assert_eq!(tl.tracks[0].kind, TrackKind::Video);
        assert_eq!(tl.tracks[1].kind, TrackKind::Audio);
        assert_eq!(tl.tracks[2].kind, TrackKind::Audio);
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
        assert_eq!(tl.tracks[2].clips.len(), 1);

        let video_clip = &tl.tracks[0].clips[0];
        let audio_clip_0 = &tl.tracks[1].clips[0];
        let audio_clip_1 = &tl.tracks[2].clips[0];

        assert_eq!(audio_clip_0.audio_stream_index, 0);
        assert_eq!(audio_clip_1.audio_stream_index, 1);

        // Video e *tutti* gli stream audio finiscono nello stesso gruppo
        // collegato (`Clip::linked_group`), non solo il primo.
        let group = video_clip.linked_group.expect("il video è collegato");
        assert_eq!(audio_clip_0.linked_group, Some(group));
        assert_eq!(
            audio_clip_1.linked_group,
            Some(group),
            "anche il secondo stream audio fa parte dello stesso gruppo"
        );
    }
}
