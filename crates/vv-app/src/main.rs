//! Finestra egui: media pool (sinistra), viewer (centro), timeline
//! multi-traccia (basso), toolbar con play/pause/seek per l'anteprima.
//!
//! Il viewer mostra l'anteprima del media della clip selezionata in
//! timeline, con crop/zoom/gain (milestone 5, valori statici) applicati
//! tramite il compositor GPU di `vv-render`. Non è ancora il compositing
//! multi-track completo della timeline (che richiede risolvere "quale clip
//! è attiva su quale track a che tempo" per ogni frame, milestone
//! successiva) — qui si vede sempre e solo la clip selezionata, non il
//! risultato finale con tutte le track sovrapposte.
//!
//! La texture del frame corrente viene riusata in-place a ogni frame
//! (`TextureHandle::set`) invece di riallocarla 25-60 volte al secondo. Il
//! compositor fa oggi un round-trip CPU->GPU->CPU per restare compatibile
//! con questo path: passare la texture GPU direttamente a egui (zero-copy)
//! è un'ottimizzazione futura, vedi doc di `vv_render::Compositor`.

mod export;
mod player;
mod timeline_ui;

use player::Player;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use vv_core::{ClipId, FrameIdx, MediaId, TimelineId, Track, TrackKind};

/// Unica track video supportata per ora dalla riproduzione timeline-aware
/// (l'app crea sempre "video in track 0, audio in track 1", vedi
/// `add_media_to_timeline`): il compositing multi-track vero e proprio
/// resta per una milestone successiva.
const VIDEO_TRACK: usize = 0;

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

/// Stato di `VibeVideoApp::gap_playback` — vedi il suo doc per il quadro
/// generale.
struct GapPlayback {
    started_at: Instant,
    start_frame: FrameIdx,
    next_clip_id: ClipId,
    next_clip_start: FrameIdx,
    /// Player della prossima clip già aperto e posizionato al suo
    /// `source_in`, ma non ancora in play: dà al decode-ahead tutto il
    /// tempo residuo del vuoto per popolare la cache dei frame, invece di
    /// partire da zero solo al cambio di controllo (bug: "il primo
    /// secondo di video resta nero, l'audio invece parte subito"). `None`
    /// per un generatore SolidColor o se l'apertura è fallita — in quel
    /// caso il cambio di controllo ricade su `load_video_clip`.
    preloaded_player: Option<Player>,
}

/// Quanti secondi prima della fine della clip attiva (nello spazio della
/// clip stessa) iniziare a precaricare la prossima clip video sulla track
/// — vedi `VibeVideoApp::maintain_next_clip_preload`. Dà al decode-ahead
/// del nuovo player tempo di scaldarsi *prima* del taglio invece che a
/// partire da lì, sia sui tagli netti sia sui vuoti (bug segnalato:
/// l'indicatore "buffered" mostrava il buffer fermarsi a fine clip e
/// ripartire da zero solo quando la testina raggiungeva la successiva).
const NEXT_CLIP_PRELOAD_LOOKAHEAD_SECS: f64 = 3.0;

/// Stato di `VibeVideoApp::next_preload` — vedi il suo doc per il quadro
/// generale.
struct NextPreload {
    clip_id: ClipId,
    player: Player,
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

struct VibeVideoApp {
    project: vv_core::Project,
    history: vv_core::History,
    timeline_id: Option<TimelineId>,
    timeline_state: timeline_ui::TimelineState,
    import_error: Option<String>,

    preview_path: Option<PathBuf>,
    preview_meta: Option<vv_core::MediaMeta>,
    preview_player: Option<Player>,
    preview_error: Option<String>,
    frame_texture: Option<egui::TextureHandle>,
    /// Traccia audio già decodificata per path, riusata da `preview_media`
    /// invece di ridecodificarla da zero ogni volta che il player per quel
    /// media viene riaperto (es. il playhead attraversa un vuoto: il
    /// player si chiude e riapre, e senza cache ogni attraversamento
    /// ridecodificherebbe l'intero file — un hitch percepibile, da
    /// centinaia di ms a oltre 1s per file lunghi). Non invalidata se il
    /// file cambia su disco durante la sessione: un limite accettabile per
    /// una cache di sessione.
    audio_cache: HashMap<PathBuf, std::sync::Arc<vv_media::AudioBuffer>>,

    /// Budget di memoria (byte) per la cache dei frame decodificati di
    /// *ogni* player aperto (vedi doc di `Player::open`/`DecodeAhead::spawn`):
    /// configurabile dall'utente nel menu "Visualizza" invece di una
    /// costante fissa nel codice, perché quanta RAM vale la pena dedicare
    /// a un margine di riproduzione fluida contro OOM dipende
    /// dall'hardware/uso dell'utente, non da una scelta valida per tutti.
    cache_budget_bytes: usize,

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

    /// Riproduzione in corso attraverso un vuoto sulla track video: nessun
    /// player da seguire lì (schermo nero, vedi il rendering nel viewer),
    /// quindi il playhead avanza a un proprio orologio a parete finché non
    /// raggiunge `next_clip_start`, punto in cui il controllo passa al
    /// player di quella clip (`GapPlayback::next_clip_id`). `None` quando
    /// non si sta attraversando un vuoto durante la riproduzione.
    gap_playback: Option<GapPlayback>,

    /// Player della prossima clip video sulla track, aperto in anticipo
    /// mentre quella attiva si avvicina alla fine (vedi
    /// `maintain_next_clip_preload`/`NEXT_CLIP_PRELOAD_LOOKAHEAD_SECS`) —
    /// sia che tra le due ci sia un vuoto sia che siano adiacenti. Consumato
    /// da `load_video_clip` (taglio netto) o `begin_gap_playback` (vuoto)
    /// quando il momento arriva, invece di aprire un player a freddo
    /// esattamente lì. `None` se non c'è nulla da precaricare al momento
    /// (fine contenuto, non in riproduzione, o già consumato/scartato).
    next_preload: Option<NextPreload>,

    /// "Selection follows playhead": attiva di default, disattivabile
    /// dalle impostazioni. Quando attiva, spostare il playhead (scrub o
    /// click sul righello) o tagliare/eliminare seleziona automaticamente
    /// la clip sulla track video sotto al playhead — comodo per fare più
    /// tagli/ripple-delete in rapida successione senza dover ricliccare
    /// ogni volta la clip.
    selection_follows_playhead: bool,

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
    /// da `Player::peak_linear_stereo` è istantaneo, senza smorzamento
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
            preview_path: None,
            preview_meta: None,
            preview_player: None,
            preview_error: None,
            frame_texture: None,
            audio_cache: HashMap::new(),
            cache_budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
            active_clip: None,
            compositor: vv_render::Compositor::new_headless(),
            last_synced_playhead: 0,
            browsing_media: None,
            gap_playback: None,
            next_preload: None,
            selection_follows_playhead: true,
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
                let media_id = self.project.media_pool.insert(vv_core::MediaItem {
                    path: path.clone(),
                    meta,
                    content_hash: 0, // placeholder: hashing reale arriva con la cache (milestone 8)
                });
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
                self.preview_path = None;
                self.preview_meta = None;
                self.preview_player = None;
                self.preview_error = None;
                self.frame_texture = None;
                self.active_clip = None;
                self.last_synced_playhead = 0;
                self.browsing_media = None;
                self.current_project_path = Some(path);
                self.project_error = None;
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
    /// buffer audio (`Player::peak_linear_stereo`), con un decadimento
    /// applicato qui frame per frame perché il valore istantaneo da solo
    /// farebbe scendere le barre a scatti invece che dolcemente.
    fn draw_audiometer(&mut self, ui: &mut egui::Ui) {
        const DECAY: f32 = 0.85;
        let (raw_l, raw_r) = self
            .preview_player
            .as_ref()
            .map(Player::peak_linear_stereo)
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

    fn preview_media(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let path = item.path.clone();
        let meta = item.meta.clone();
        let duration_secs = meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9);

        // `frame_texture` non viene azzerata qui apposta: il viewer
        // continua a mostrare l'ultimo frame finché il nuovo player non ne
        // decodifica uno (sovrascrive la texture in-place), invece di un
        // flash a vuoto durante il cambio media.
        self.preview_player = None;
        self.preview_error = None;

        let cached_audio = self.audio_cache.get(&path).cloned();
        match Player::open(&path, duration_secs, cached_audio, self.cache_budget_bytes) {
            Ok((player, audio_buffer)) => {
                self.preview_player = Some(player);
                if let Some(buffer) = audio_buffer {
                    self.audio_cache.insert(path.clone(), buffer);
                }
            }
            Err(e) => self.preview_error = Some(e),
        }
        self.preview_path = Some(path);
        self.preview_meta = Some(meta);
    }

    /// La clip su `track_index` che copre `frame` (nello spazio della
    /// timeline), se c'è.
    fn clip_at(&self, track_index: usize, frame: FrameIdx) -> Option<ClipId> {
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .tracks
            .get(track_index)?
            .clips
            .iter()
            .find(|c| frame >= c.timeline_start && frame < c.timeline_end())
            .map(|c| c.id)
    }

    /// Se "selection follows playhead" è attivo, allinea la selezione alla
    /// clip sulla track video sotto al playhead corrente (selezione vuota
    /// se il playhead è su un vuoto): collassa sempre a una singola clip,
    /// anche se prima della sync la selezione era multipla. No-op se la
    /// funzionalità è disattivata dalle impostazioni.
    fn sync_selection_to_playhead(&mut self) {
        if !self.selection_follows_playhead {
            return;
        }
        let clip = self
            .clip_at(VIDEO_TRACK, self.timeline_state.playhead)
            .map(|id| (VIDEO_TRACK, id));
        self.timeline_state.set_single_selection(clip);
    }

    /// Carica `clip_id` (sulla track video) nel player, posizionandosi al
    /// frame locale corrispondente al playhead corrente — sia per lo
    /// scrub in una clip diversa sia per il primo aggancio all'avvio della
    /// riproduzione. Le clip SolidColor non hanno un player: il colore si
    /// legge direttamente nel pannello centrale dal playhead.
    fn load_video_clip(&mut self, clip_id: ClipId) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(clip) = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .cloned()
        else {
            return;
        };

        // Se la clip precedente era sullo *stesso media*, non riaprire il
        // player: `Player::open` rifà un decode completo della traccia
        // audio e apre un nuovo decoder video. Basta un seek sul player
        // già aperto — anzi, se il player è già esattamente al frame
        // giusto (il caso di un T-split: le due metà sono contigue nello
        // stesso file, `source_out` dell'una == `source_in` dell'altra),
        // nemmeno quello: un seek è comunque un flush del decoder,
        // evitabile del tutto in quel caso. Ma "stesso media" da solo NON
        // basta a garantire che il riuso sia gratis: due clip che
        // referenziano *punti diversi e non contigui* dello stesso file
        // (es. due trim separati dello stesso sorgente incollati altrove
        // sulla timeline) richiedono comunque un seek reale — bug
        // segnalato: proprio in quel caso il riuso "gratis" veniva scelto
        // a prescindere, ignorando un eventuale preload già scaldato in
        // anticipo per quella clip (l'indicatore "buffered" mostrava
        // progresso, ma al taglio il costo del seek si pagava comunque,
        // perché il player riusato non era quello preparato in anticipo).
        let previous_media = self.active_clip.and_then(|(track_index, id)| {
            self.project.timelines[timeline_id]
                .tracks
                .get(track_index)?
                .clips
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| match c.source {
                    vv_core::ClipSource::Media(m) => Some(m),
                    vv_core::ClipSource::SolidColor => None,
                })
        });

        self.active_clip = Some((VIDEO_TRACK, clip_id));

        match clip.source {
            vv_core::ClipSource::Media(media_id) => {
                let same_media = can_reuse_player_for(previous_media, media_id);
                // Continuazione davvero gratis: il player esistente è già
                // posizionato esattamente dove serve, nessun seek in vista
                // (vedi sopra). Confrontato sulla posizione *reale* del
                // player, non solo dedotto dai dati della clip: resta
                // corretto anche per uno scrub diretto a metà clip, non
                // solo per il taglio netto a inizio clip.
                let already_at_target = same_media
                    && self
                        .preview_player
                        .as_ref()
                        .is_some_and(|p| p.current_source_frame() == clip.source_in);

                if !already_at_target {
                    // Preferisce un preload già scaldato in anticipo per
                    // *questa* clip (vedi `maintain_next_clip_preload`), a
                    // prescindere dal fatto che sia lo stesso media o no:
                    // se non è una continuazione gratis, riusare il player
                    // esistente con un seek costa comunque quanto aprirne
                    // uno nuovo. Solo se non c'è alcun preload pronto si
                    // ricade sul riuso-con-seek (se stesso media, evita
                    // almeno la riapertura completa) o sull'apertura a
                    // freddo (se media diverso).
                    match self.next_preload.take() {
                        Some(preload) if preload.clip_id == clip_id => {
                            self.preview_player = Some(preload.player);
                        }
                        _ if same_media && self.preview_player.is_some() => {
                            // Riusa il player già aperto così com'è: il
                            // seek verso il target, più sotto, lo porterà
                            // al punto giusto.
                        }
                        _ => self.preview_media(media_id),
                    }
                }
                if let Some(player) = &self.preview_player {
                    player.set_gain_db(clip.effects.gain_db.default);
                }
                let local = (self.timeline_state.playhead - clip.timeline_start)
                    .clamp(0, clip.timeline_len().saturating_sub(1));
                let target = clip.source_in + local;
                if let Some(player) = &mut self.preview_player
                    && player.current_source_frame() != target
                {
                    player.seek_to_frame(target);
                }
            }
            vv_core::ClipSource::SolidColor => {
                self.preview_player = None;
                self.frame_texture = None;
                self.preview_error = None;
            }
        }
    }

    /// Se non stiamo mostrando un'anteprima "grezza" da media pool, tiene
    /// il player agganciato a qualunque clip copra il playhead sulla track
    /// video: ricarica solo quando cambia davvero (scrub, selezione di
    /// un'altra zona, o dopo un avanzamento automatico).
    /// `force_seek` va messo a `true` quando il cambio di `playhead` di
    /// questo frame viene da un'interazione diretta dell'utente con la
    /// timeline (trascinamento del ruler/di una clip, click), *anche se*
    /// il player sta riproducendo — altrimenti lo scrub durante il
    /// playback verrebbe ignorato: il player continuerebbe a suonare da
    /// dov'era, riscrivendo il playhead sopra al tentativo dell'utente al
    /// frame successivo (bug: "il player mi ignora se sposto la playhead
    /// mentre riproduce"). Quando invece il playhead si muove perché è
    /// `drive_playback` stesso ad averlo appena spostato per seguire la
    /// riproduzione, `force_seek` deve restare `false`: altrimenti si
    /// farebbe un seek (con relativo flush del decoder) a ogni singolo
    /// frame, anche se il player è già esattamente lì.
    fn ensure_active_clip_matches_playhead(&mut self, force_seek: bool) {
        if self.browsing_media.is_some() || self.timeline_id.is_none() {
            return;
        }
        let desired = self.clip_at(VIDEO_TRACK, self.timeline_state.playhead);
        let current = self
            .active_clip
            .filter(|(t, _)| *t == VIDEO_TRACK)
            .map(|(_, id)| id);

        if desired != current {
            match desired {
                // `load_video_clip` fa già un seek verso il playhead
                // corrente: non serve altro qui.
                Some(clip_id) => self.load_video_clip(clip_id),
                None => {
                    self.active_clip = None;
                    self.preview_player = None;
                    self.frame_texture = None;
                }
            }
        } else if let Some(clip_id) = desired
            && self.timeline_state.playhead != self.last_synced_playhead
        {
            let is_playing = self.preview_player.as_ref().is_some_and(Player::is_playing);
            if force_seek || !is_playing {
                // Stessa clip: segui con un seek nella clip già aperta,
                // senza riaprirla. Se stavamo riproducendo, resta in
                // riproduzione da lì (non mettere in pausa).
                self.seek_active_player_to_playhead(clip_id);
            }
        }
        self.last_synced_playhead = self.timeline_state.playhead;
    }

    /// Fa un seek del player già aperto verso il punto della clip
    /// corrispondente al playhead corrente (senza riaprirlo).
    fn seek_active_player_to_playhead(&mut self, clip_id: ClipId) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(clip) = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
        else {
            return;
        };
        let local = (self.timeline_state.playhead - clip.timeline_start)
            .clamp(0, clip.timeline_len().saturating_sub(1));
        let target = clip.source_in + local;
        if let Some(player) = &mut self.preview_player {
            player.seek_to_frame(target);
        }
    }

    /// Fa play/pause sulla clip sotto al playhead, senza bisogno che sia
    /// selezionata (bug: "la riproduzione parte solo se seleziono la
    /// clip"). Se il player non è ancora agganciato lo aggancia subito. Se
    /// il playhead sta attraversando un vuoto (`gap_playback` attivo),
    /// ferma l'orologio a vuoto (non c'è un player da mettere in pausa) —
    /// e se invece è *fermo* dentro un vuoto, riprende quell'orologio
    /// verso la prossima clip sulla track video, se c'è.
    fn toggle_playback(&mut self) {
        self.browsing_media = None;
        if self.gap_playback.take().is_some() {
            return;
        }
        self.ensure_active_clip_matches_playhead(false);
        if let Some(player) = &mut self.preview_player {
            player.toggle_play_pause();
            return;
        }
        if let Some(timeline_id) = self.timeline_id
            && let Some((next_id, next_start)) =
                self.next_video_clip_from(timeline_id, self.timeline_state.playhead)
        {
            self.begin_gap_playback(self.timeline_state.playhead, next_id, next_start);
        }
    }

    /// Guida la riproduzione a ogni frame UI: se il player sta suonando,
    /// il playhead lo segue; se ha raggiunto la fine del *trim* della clip
    /// attiva (non della fine del file, che può essere più lunga), avanza
    /// oltre — dritto alla prossima clip se comincia esattamente lì, o
    /// attraverso un vuoto (`gap_playback`: schermo nero, orologio a
    /// parete) se c'è, invece di saltare la riproduzione in avanti fino ad
    /// essa (bug: "il vuoto viene saltato invece di essere riprodotto").
    ///
    /// Se `gap_playback` è attivo, questa stessa funzione fa anche da
    /// "player" per il vuoto: avanza il playhead a orologio finché non
    /// raggiunge la prossima clip, poi le passa il controllo.
    ///
    /// Limite noto: se la prossima clip è un generatore SolidColor la
    /// riproduzione si ferma lì, perché un generatore non ha un player che
    /// faccia da orologio — avanzare il tempo "a vuoto" durante un
    /// generatore è un'estensione futura (diversa da un vuoto vero e
    /// proprio, già gestito qui).
    fn drive_playback(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };

        if let Some(gap) = &self.gap_playback {
            let fps = self.project.timelines[timeline_id].fps.as_f64().max(1e-9);
            let elapsed_frames = (gap.started_at.elapsed().as_secs_f64() * fps).round() as FrameIdx;
            let frame = (gap.start_frame + elapsed_frames).min(gap.next_clip_start);
            self.timeline_state.playhead = frame;
            if frame >= gap.next_clip_start {
                let next_clip_id = gap.next_clip_id;
                let preloaded_player = self.gap_playback.take().and_then(|g| g.preloaded_player);
                match preloaded_player {
                    // Già aperto e posizionato durante il vuoto (vedi
                    // `begin_gap_playback`): il decode-ahead ha già avuto
                    // tempo di popolare la cache, niente hitch.
                    Some(mut player) => {
                        player.play();
                        self.active_clip = Some((VIDEO_TRACK, next_clip_id));
                        self.preview_player = Some(player);
                    }
                    None => {
                        self.load_video_clip(next_clip_id);
                        if let Some(player) = &mut self.preview_player {
                            player.play();
                        }
                    }
                }
            }
            return;
        }

        let Some((track_index, clip_id)) = self.active_clip else {
            return;
        };
        if track_index != VIDEO_TRACK {
            return;
        }
        let Some(clip) = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .cloned()
        else {
            return;
        };
        let Some(player) = &self.preview_player else {
            return;
        };
        if !player.is_playing() {
            return;
        }

        if player.current_source_frame() >= clip.source_out {
            self.advance_playback_past(timeline_id, clip.timeline_end());
        } else {
            let local = player.current_source_frame() - clip.source_in;
            self.timeline_state.playhead = clip.timeline_start + local;
        }
    }

    /// La prima clip sulla track video con inizio >= `from_frame`, se c'è.
    fn next_video_clip_from(
        &self,
        timeline_id: TimelineId,
        from_frame: FrameIdx,
    ) -> Option<(ClipId, FrameIdx)> {
        self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .filter(|c| c.timeline_start >= from_frame)
            .min_by_key(|c| c.timeline_start)
            .map(|c| (c.id, c.timeline_start))
    }

    /// Azzera la clip/player attivi (siamo in un vuoto: schermo nero, vedi
    /// il rendering nel viewer) e avvia l'orologio a parete di
    /// `gap_playback` verso `next_clip_id`/`next_clip_start`. Riusa il
    /// player già precaricato in anticipo da `maintain_next_clip_preload`
    /// se punta già alla clip giusta (il caso comune: aveva l'intero
    /// residuo della clip precedente per scaldarsi, non solo la durata del
    /// vuoto), altrimenti lo apre solo ora (clip troppo corta perché il
    /// preload anticipato abbia fatto in tempo) — vedi doc di
    /// `GapPlayback::preloaded_player`.
    fn begin_gap_playback(
        &mut self,
        from_frame: FrameIdx,
        next_clip_id: ClipId,
        next_clip_start: FrameIdx,
    ) {
        self.active_clip = None;
        self.preview_player = None;
        self.frame_texture = None;
        self.timeline_state.playhead = from_frame;
        let preloaded_player = match self.next_preload.take() {
            Some(preload) if preload.clip_id == next_clip_id => Some(preload.player),
            _ => self.preload_player_for_clip(next_clip_id),
        };
        self.gap_playback = Some(GapPlayback {
            started_at: Instant::now(),
            start_frame: from_frame,
            next_clip_id,
            next_clip_start,
            preloaded_player,
        });
    }

    /// Apre in anticipo (senza avviare la riproduzione) il player della
    /// clip video `clip_id`, posizionato già al suo `source_in`: dà al
    /// decode-ahead tutto il tempo residuo del vuoto per popolare la
    /// cache dei frame prima del cambio di controllo a fine
    /// `gap_playback`, invece di partire da zero solo in quel momento
    /// (bug: audio già in RAM che parte subito, ma un secondo circa di
    /// schermo nero finché il decoder video si scalda). `None` per un
    /// generatore SolidColor (nessun player) o se l'apertura fallisce —
    /// in quel caso il cambio di controllo ricade su `load_video_clip`.
    fn preload_player_for_clip(&mut self, clip_id: ClipId) -> Option<Player> {
        let timeline_id = self.timeline_id?;
        let clip = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
            .clips
            .iter()
            .find(|c| c.id == clip_id)?
            .clone();
        let vv_core::ClipSource::Media(media_id) = clip.source else {
            return None;
        };
        let item = self.project.media_pool.get(media_id)?;
        let path = item.path.clone();
        let meta = item.meta.clone();
        let duration_secs = meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9);
        let cached_audio = self.audio_cache.get(&path).cloned();
        let (mut player, audio_buffer) =
            Player::open(&path, duration_secs, cached_audio, self.cache_budget_bytes).ok()?;
        if let Some(buffer) = audio_buffer {
            self.audio_cache.insert(path, buffer);
        }
        player.set_gain_db(clip.effects.gain_db.default);
        player.seek_to_frame(clip.source_in);
        Some(player)
    }

    /// Da chiamare a ogni frame UI: mentre la clip video attiva (o, durante
    /// un vuoto, `gap_playback`) si avvicina alla fine entro
    /// `NEXT_CLIP_PRELOAD_LOOKAHEAD_SECS`, avvia in anticipo il player
    /// della prossima clip video sulla track — sia che tra le due ci sia
    /// un vuoto sia che siano adiacenti (bug segnalato: l'indicatore
    /// "buffered" mostrava il buffer fermarsi a fine clip e ripartire da
    /// zero solo quando la testina raggiungeva la successiva; prima
    /// l'unico preload esisteva già ma solo per il caso "vuoto", avviato
    /// solo all'inizio del vuoto stesso). Scarta un preload obsoleto
    /// (l'utente ha scrubbato altrove, o la prossima clip è cambiata) e
    /// non fa nulla se non c'è riproduzione in corso o non c'è una
    /// prossima clip da precaricare.
    fn maintain_next_clip_preload(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            self.next_preload = None;
            return;
        };
        let fps = self.project.timelines[timeline_id].fps.as_f64().max(1e-9);

        let next: Option<(ClipId, f64)> = if let Some(gap) = &self.gap_playback {
            let remaining = (gap.next_clip_start - self.timeline_state.playhead) as f64 / fps;
            Some((gap.next_clip_id, remaining))
        } else {
            match self.active_clip {
                Some((VIDEO_TRACK, clip_id))
                    if self.preview_player.as_ref().is_some_and(Player::is_playing) =>
                {
                    let clip = self.project.timelines[timeline_id].tracks[VIDEO_TRACK]
                        .clips
                        .iter()
                        .find(|c| c.id == clip_id)
                        .cloned();
                    match (clip, &self.preview_player) {
                        (Some(clip), Some(player)) => {
                            let remaining =
                                (clip.source_out - player.current_source_frame()) as f64 / fps;
                            self.next_video_clip_from(timeline_id, clip.timeline_end())
                                .and_then(|(next_id, _)| {
                                    let next_clip = self.project.timelines[timeline_id].tracks
                                        [VIDEO_TRACK]
                                        .clips
                                        .iter()
                                        .find(|c| c.id == next_id)?;
                                    // Continuazione gratis nello stesso
                                    // file (es. dopo un T-split):
                                    // `load_video_clip` la riprende senza
                                    // alcun seek, un preload qui sarebbe
                                    // solo un player extra sprecato.
                                    (!is_seamless_continuation(&clip, next_clip))
                                        .then_some((next_id, remaining))
                                })
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        };

        let Some((next_id, remaining_secs)) = next else {
            self.next_preload = None;
            return;
        };
        if remaining_secs > NEXT_CLIP_PRELOAD_LOOKAHEAD_SECS {
            self.next_preload = None;
            return;
        }
        if self
            .next_preload
            .as_ref()
            .is_some_and(|p| p.clip_id == next_id)
        {
            return;
        }
        if let Some(player) = self.preload_player_for_clip(next_id) {
            self.next_preload = Some(NextPreload {
                clip_id: next_id,
                player,
            });
        }
    }

    /// Intervalli (in frame di *timeline*) attualmente bufferizzati nei
    /// player aperti in questo momento, per l'indicatore visivo "buffered"
    /// sulla timeline (richiesta: "visualizzare durante la riproduzione
    /// come viene fatto il buffer"). Include la clip attiva
    /// (`preview_player`) e, se in corso, il preload della prossima clip
    /// — durante un vuoto (`gap_playback.preloaded_player`) o su un taglio
    /// netto in avvicinamento (`next_preload`, vedi
    /// `maintain_next_clip_preload`): sono gli unici player che possono
    /// esistere in un dato momento in questa app.
    fn buffered_timeline_ranges(&self) -> Vec<(FrameIdx, FrameIdx)> {
        let Some(timeline_id) = self.timeline_id else {
            return Vec::new();
        };
        let tl = &self.project.timelines[timeline_id];
        let mut ranges = Vec::new();

        if let Some((track_index, clip_id)) = self.active_clip
            && let Some(player) = &self.preview_player
            && let Some(clip) = tl
                .tracks
                .get(track_index)
                .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
        {
            ranges.extend(map_source_ranges_to_timeline(
                clip,
                &player.cached_source_ranges(),
            ));
        }

        if let Some(gap) = &self.gap_playback
            && let Some(player) = &gap.preloaded_player
            && let Some(clip) = tl.tracks[VIDEO_TRACK]
                .clips
                .iter()
                .find(|c| c.id == gap.next_clip_id)
        {
            ranges.extend(map_source_ranges_to_timeline(
                clip,
                &player.cached_source_ranges(),
            ));
        }

        if let Some(preload) = &self.next_preload
            && let Some(clip) = tl.tracks[VIDEO_TRACK]
                .clips
                .iter()
                .find(|c| c.id == preload.clip_id)
        {
            ranges.extend(map_source_ranges_to_timeline(
                clip,
                &preload.player.cached_source_ranges(),
            ));
        }

        ranges
    }

    /// Continua la riproduzione oltre la fine (nello spazio timeline) della
    /// clip appena conclusa: se la prossima clip sulla track video comincia
    /// esattamente lì, ci salta senza soluzione di continuità (nessun
    /// vuoto); se c'è un vuoto prima, entra in `gap_playback`; se non c'è
    /// nessuna clip successiva, è la fine del contenuto — si ferma.
    fn advance_playback_past(&mut self, timeline_id: TimelineId, from_frame: FrameIdx) {
        match self.next_video_clip_from(timeline_id, from_frame) {
            Some((next_id, next_start)) if next_start == from_frame => {
                self.timeline_state.playhead = next_start;
                self.load_video_clip(next_id);
                if let Some(player) = &mut self.preview_player {
                    player.play();
                }
            }
            Some((next_id, next_start)) => {
                self.begin_gap_playback(from_frame, next_id, next_start);
            }
            None => {
                if let Some(player) = &mut self.preview_player {
                    player.pause();
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
        id
    }

    /// Crea una clip generatore SolidColor da 5s e la accoda in fondo alla
    /// track video 0. Il colore iniziale è grigio medio, modificabile
    /// subito dal pannello proprietà una volta selezionata.
    fn add_solid_color_clip(&mut self) {
        let timeline_id = self.ensure_timeline();
        let fps = self.project.timelines[timeline_id].fps.as_f64();
        let default_len = (fps * 5.0).round() as FrameIdx;
        let video_start = track_end(&self.project, timeline_id, 0);

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
            linked: None,
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip,
            }),
        );
    }

    fn apply_active_clip_gain(&mut self) {
        let Some(gain) = self.active_clip_effects().map(|e| e.gain_db.default) else {
            return;
        };
        if let Some(player) = &self.preview_player {
            player.set_gain_db(gain);
        }
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

    /// La gemella collegata (`Clip::linked`) di una clip, con la sua
    /// track: `None` se non è collegata a nulla. Cerca su tutte le track
    /// perché il chiamante conosce solo la track della clip di partenza.
    fn linked_partner(
        &self,
        timeline_id: TimelineId,
        track_index: usize,
        clip_id: ClipId,
    ) -> Option<(usize, ClipId)> {
        let partner_id = self.project.timelines[timeline_id]
            .tracks
            .get(track_index)?
            .clips
            .iter()
            .find(|c| c.id == clip_id)?
            .linked?;
        let partner_track = self.project.timelines[timeline_id]
            .tracks
            .iter()
            .position(|t| t.clips.iter().any(|c| c.id == partner_id))?;
        Some((partner_track, partner_id))
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
        let video_start = track_end(&self.project, timeline_id, 0);
        self.insert_media_clip(timeline_id, media_id, &meta, video_start);
    }

    /// Come `add_media_to_timeline`, ma piazza la clip (e la sua gemella
    /// audio, se c'è) esattamente a `start`, invece che in coda: usato dal
    /// drag&drop dal media pool sulla timeline, dove la posizione viene dal
    /// punto orizzontale in cui l'utente rilascia (vedi
    /// `timeline_ui::show_timeline`).
    fn add_media_to_timeline_at(&mut self, media_id: MediaId, start: FrameIdx) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let meta = item.meta.clone();
        let timeline_id = self.ensure_timeline_for(&meta);
        self.insert_media_clip(timeline_id, media_id, &meta, start);
    }

    /// Inserisce la clip video (e, se il media ha audio, la sua gemella
    /// collegata sulla track audio) entrambe a `start`.
    fn insert_media_clip(
        &mut self,
        timeline_id: TimelineId,
        media_id: MediaId,
        meta: &vv_core::MediaMeta,
        start: FrameIdx,
    ) {
        let video_clip_id = self.project.alloc_clip_id();
        // Se la clip ha anche audio, le due metà vengono collegate di
        // default (vedi doc di `Clip::linked`): l'utente può scollegarle
        // dal menu contestuale sulla clip, in timeline_ui.
        let audio_clip_id = meta.has_audio.then(|| self.project.alloc_clip_id());

        let video_clip = vv_core::Clip {
            id: video_clip_id,
            source: vv_core::ClipSource::Media(media_id),
            source_in: 0,
            source_out: meta.duration_frames,
            timeline_start: start,
            effects: vv_core::EffectStack::default(),
            linked: audio_clip_id,
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip: video_clip,
            }),
        );

        if let Some(audio_clip_id) = audio_clip_id {
            let audio_clip = vv_core::Clip {
                id: audio_clip_id,
                source: vv_core::ClipSource::Media(media_id),
                source_in: 0,
                source_out: meta.duration_frames,
                timeline_start: start,
                effects: vv_core::EffectStack::default(),
                linked: Some(video_clip_id),
            };
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::InsertClip {
                    timeline: timeline_id,
                    track_index: 1,
                    clip: audio_clip,
                }),
            );
        }
    }

    /// Normal delete: rimuove *tutte* le clip selezionate (e la gemella
    /// collegata di ciascuna, se c'è), lasciando un vuoto al loro posto.
    /// Le altre track non si muovono. Un solo passo di history per tutte
    /// insieme. L'ordine non conta: `LiftDelete` non sposta nient'altro.
    fn delete_selected(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            return;
        }
        let selected: Vec<(usize, ClipId)> = self.timeline_state.selected.iter().copied().collect();
        let mut to_delete: BTreeSet<(usize, ClipId)> = selected.iter().copied().collect();
        for &(track_index, clip_id) in &selected {
            if let Some(partner) = self.linked_partner(timeline_id, track_index, clip_id) {
                to_delete.insert(partner);
            }
        }

        let commands: Vec<Box<dyn vv_core::Command>> = to_delete
            .into_iter()
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

    /// Copia le clip selezionate (+ la gemella collegata di ciascuna, se
    /// non già anch'essa selezionata esplicitamente — stesso principio di
    /// `delete_selected`) in `timeline_state.clipboard`, pronte per
    /// `paste_clipboard_at_playhead`. No-op se non c'è nulla di
    /// selezionato.
    fn copy_selected_clips(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            return;
        }
        let selected: Vec<(usize, ClipId)> = self.timeline_state.selected.iter().copied().collect();
        let mut to_copy: BTreeSet<(usize, ClipId)> = selected.iter().copied().collect();
        for &(track_index, clip_id) in &selected {
            if let Some(partner) = self.linked_partner(timeline_id, track_index, clip_id) {
                to_copy.insert(partner);
            }
        }

        let tl = &self.project.timelines[timeline_id];
        // (id originale, gemella originale, entry) — l'id e la gemella
        // servono solo per risolvere `linked_index` qui sotto, non entrano
        // nell'entry salvata (i vecchi ClipId non sopravvivono al paste).
        let mut collected: Vec<(ClipId, Option<ClipId>, timeline_ui::ClipboardEntry)> = to_copy
            .iter()
            .filter_map(|&(track_index, clip_id)| {
                let clip = tl
                    .tracks
                    .get(track_index)?
                    .clips
                    .iter()
                    .find(|c| c.id == clip_id)?;
                Some((
                    clip_id,
                    clip.linked,
                    timeline_ui::ClipboardEntry {
                        track_index,
                        relative_start: clip.timeline_start,
                        source: clip.source.clone(),
                        source_in: clip.source_in,
                        source_out: clip.source_out,
                        effects: clip.effects.clone(),
                        linked_index: None,
                    },
                ))
            })
            .collect();

        if collected.is_empty() {
            return;
        }

        let anchor = collected
            .iter()
            .map(|(_, _, e)| e.relative_start)
            .min()
            .unwrap_or(0);
        for (_, _, e) in &mut collected {
            e.relative_start -= anchor;
        }
        for i in 0..collected.len() {
            if let Some(partner_id) = collected[i].1
                && let Some(j) = collected.iter().position(|(id, _, _)| *id == partner_id)
            {
                collected[i].2.linked_index = Some(j);
            }
        }

        self.timeline_state.clipboard = collected.into_iter().map(|(_, _, e)| e).collect();
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
    /// Dividere una clip la scollega temporaneamente dalla sua gemella
    /// (comportamento di `SplitClip`): se la gemella è *anche lei* tra le
    /// track coinvolte in `ranges` (il caso comune: si incolla sempre la
    /// coppia video+audio insieme, vedi `copy_selected_clips`), viene
    /// divisa a sua volta con lo stesso taglio e le nuove metà vengono
    /// ricollegate subito dopo — stesso principio di
    /// `split_all_at_playhead`. Se la gemella non è tra `ranges` (si sta
    /// incollando solo un lato) resta scollegata: limite noto, accettabile
    /// perché non incide sulla riproduzione.
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
            let overlapping: Vec<(ClipId, FrameIdx, FrameIdx, FrameIdx, Option<ClipId>)> =
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
                                    c.linked,
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();

            for (clip_id, old_start, old_end, source_in, linked) in overlapping {
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

                let Some(partner_id) = linked else { continue };
                let Some((partner_track, _)) =
                    self.linked_partner(timeline_id, track_index, clip_id)
                else {
                    continue;
                };
                if !range_tracks.contains(&partner_track)
                    || !processed.insert((partner_track, partner_id))
                {
                    continue;
                }
                let Some((p_start, p_end, p_source_in)) =
                    self.clip_bounds(timeline_id, partner_track, partner_id)
                else {
                    continue;
                };
                let partner_split = self.resolve_overlap(
                    timeline_id,
                    partner_track,
                    partner_id,
                    p_start,
                    p_end,
                    p_source_in,
                    new_start,
                    new_end,
                    commands,
                );

                if let (Some((left, right)), Some((partner_left, partner_right))) =
                    (split_halves, partner_split)
                {
                    commands.push(Box::new(vv_core::LinkClips::new(
                        timeline_id,
                        (track_index, left),
                        (partner_track, partner_left),
                    )));
                    commands.push(Box::new(vv_core::LinkClips::new(
                        timeline_id,
                        (track_index, right),
                        (partner_track, partner_right),
                    )));
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
                linked: entry.linked_index.map(|j| new_ids[j]),
            };
            new_selection.insert((entry.track_index, new_ids[i]));
            commands.push(Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: entry.track_index,
                clip,
            }));
        }

        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(commands)),
        );
        let anchor = new_selection.iter().next().copied();
        self.timeline_state.set_selection(new_selection, anchor);
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
    /// `split_all_at_playhead` per evitare doppi spostamenti.
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
            if let Some(partner) = self.linked_partner(timeline_id, track_index, clip_id) {
                processed.insert(partner);
                also_remove.push(partner);
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

    /// Divide *tutte* le clip che coprono il playhead, su ogni track (tasto
    /// T): comportamento standard da "lametta", non richiede una
    /// selezione (bug: "il taglio funzionava solo sulla track
    /// selezionata"). Un solo passo di history per l'intero taglio.
    /// Taglia con T tutte le clip sotto al playhead, su ogni track. Le
    /// coppie collegate (`Clip::linked`) i cui *entrambi* i membri vengono
    /// tagliati nello stesso punto restano collegate anche dopo: metà
    /// sinistra con metà sinistra, metà destra con metà destra. Senza
    /// questo, `SplitClip` scollegherebbe sempre entrambe le metà (comportamento
    /// corretto quando si taglia una sola clip di una coppia, perché lì
    /// solo un pezzo rappresenta ancora l'intera durata collegata), e
    /// selezionare il video dopo un taglio non evidenzierebbe più l'audio.
    fn split_all_at_playhead(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let playhead = self.timeline_state.playhead;
        let targets: Vec<(usize, ClipId, Option<ClipId>)> = self.project.timelines[timeline_id]
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(track_index, track)| {
                track
                    .clips
                    .iter()
                    .filter(move |c| playhead > c.timeline_start && playhead < c.timeline_end())
                    .map(move |c| (track_index, c.id, c.linked))
            })
            .collect();
        if targets.is_empty() {
            return;
        }

        let target_ids: std::collections::HashSet<ClipId> =
            targets.iter().map(|(_, id, _)| *id).collect();
        let track_of: std::collections::HashMap<ClipId, usize> =
            targets.iter().map(|(t, id, _)| (*id, *t)).collect();

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

        let mut relinked = std::collections::HashSet::new();
        for (track_index, clip_id, linked) in &targets {
            let Some(partner_id) = linked else {
                continue;
            };
            if !target_ids.contains(partner_id) || relinked.contains(clip_id) {
                continue;
            }
            relinked.insert(*clip_id);
            relinked.insert(*partner_id);
            let partner_track = track_of[partner_id];
            commands.push(Box::new(vv_core::LinkClips::new(
                timeline_id,
                (*track_index, *clip_id),
                (partner_track, *partner_id),
            )));
            commands.push(Box::new(vv_core::LinkClips::new(
                timeline_id,
                (*track_index, new_ids[clip_id]),
                (partner_track, new_ids[partner_id]),
            )));
        }

        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(commands)),
        );
        // "Selection follows playhead": seleziona la metà SINISTRA appena
        // tagliata sulla track video (il suo id è invariato, la metà che
        // ha ottenuto un nuovo id è la destra — vedi sopra) *e* la sua
        // gemella audio collegata, esplicitamente, non tramite il
        // generico "clip sotto al playhead" (che per costruzione
        // sarebbe la metà destra, dato che il playhead è esattamente al
        // suo inizio: `clip_at` usa `start <= playhead < end`). L'intento
        // più comune dopo un taglio è rivedere/eliminare ciò che sta
        // *prima* del punto appena tagliato, non dopo.
        if self.selection_follows_playhead
            && let Some((video_track, video_clip_id, _)) = targets
                .iter()
                .find(|(track_index, _, _)| *track_index == VIDEO_TRACK)
        {
            let mut selected = BTreeSet::from([(*video_track, *video_clip_id)]);
            if let Some(partner) = self.linked_partner(timeline_id, *video_track, *video_clip_id) {
                selected.insert(partner);
            }
            self.timeline_state
                .set_selection(selected, Some((*video_track, *video_clip_id)));
        }
    }
}

/// Il player aperto per `previous_media` si può riusare (solo un seek,
/// niente riapertura) per mostrare una clip il cui media è `next_media`?
/// Funzione pura per poterla testare senza passare da un `Player` vero.
fn can_reuse_player_for(previous_media: Option<MediaId>, next_media: MediaId) -> bool {
    previous_media == Some(next_media)
}

/// `true` se `b` continua `a` senza soluzione di continuità nello stesso
/// file sorgente (stesso media, `b.source_in == a.source_out`) — il caso
/// tipico di un T-split. Solo in questo caso il riuso del player già
/// aperto è davvero gratis (nessun seek): due clip che condividono il
/// media ma referenziano punti diversi e non contigui del file (es. due
/// trim separati incollati altrove sulla timeline) richiedono comunque un
/// seek reale, quindi non contano come "seamless" qui — vedi uso in
/// `maintain_next_clip_preload`/`load_video_clip`. Funzione pura per
/// poterla testare senza un vero `Player`.
fn is_seamless_continuation(a: &vv_core::Clip, b: &vv_core::Clip) -> bool {
    matches!(
        (&a.source, &b.source),
        (vv_core::ClipSource::Media(m1), vv_core::ClipSource::Media(m2)) if m1 == m2
    ) && b.source_in == a.source_out
}

/// Mappa intervalli di frame *sorgente* (spazio nativo del media, quello
/// di `Player::cached_source_ranges`) in intervalli di frame di
/// *timeline*, per una clip: clampa al suo intervallo di trim
/// (`source_in..source_out`) e trasla per il suo `timeline_start`. Un
/// intervallo sorgente che cade fuori dal trim (o lo attraversa solo in
/// parte) viene scartato o accorciato di conseguenza. Funzione pura per
/// poterla testare senza un vero `Player`.
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

impl eframe::App for VibeVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(player) = &mut self.preview_player {
            player.tick();
        }

        // Gain keyframeato: va aggiornato a ogni frame UI in base alla
        // posizione corrente del player (control-rate ~60Hz, non
        // sample-accurate: sufficiente per l'automazione in anteprima,
        // l'export a milestone 9 potrà fare di meglio se servirà).
        if let Some(effects) = self.active_clip_effects()
            && !effects.gain_db.is_constant()
            && let Some(player) = &self.preview_player
        {
            let gain = effects.gain_db.value_at(player.current_source_frame());
            player.set_gain_db(gain);
        }

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
        ui.input(|i| {
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
                self.split_all_at_playhead();
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
                        .on_hover_text("Taglia tutte le clip sotto al playhead, su ogni track")
                        .clicked()
                    {
                        self.split_all_at_playhead();
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
                    ui.separator();
                    if ui.button("Zoom avanti (Ctrl++)").clicked() {
                        self.timeline_state.zoom_in();
                        ui.close();
                    }
                    if ui.button("Zoom indietro (Ctrl+-)").clicked() {
                        self.timeline_state.zoom_out();
                        ui.close();
                    }
                    ui.label("Ctrl+scroll (o pinch) sopra la timeline zooma allo stesso modo.");
                });

                ui.menu_button("Visualizza", |ui| {
                    ui.checkbox(&mut self.properties_panel_open, "Pannello proprietà");
                    ui.checkbox(&mut self.audiometer_enabled, "Audiometer")
                        .on_hover_text(
                            "Livello del player attivo, in una fascia stretta a destra della timeline",
                        );
                    ui.separator();
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
                        }
                    });
                });
            });
        });

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Nessun pulsante play/pause: Spazio è implicito in ogni
                // editor video, un pulsante dedicato è ridondante (la
                // shortcut resta comunque attiva, vedi il blocco
                // `ui.input` più sopra).
                if let Some(player) = &self.preview_player {
                    let duration = player.duration_secs();
                    let pos = player.position_secs();
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
        let mut media_drop: Option<(MediaId, FrameIdx)> = None;
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
                    media_drop = timeline_ui::show_timeline(
                        ui,
                        &mut self.project,
                        &mut self.history,
                        timeline_id,
                        &|id| labels.get(&id).cloned().unwrap_or_default(),
                        &mut self.timeline_state,
                        self.snapping_enabled,
                        &buffered_ranges,
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
        if let Some((media_id, start)) = media_drop {
            self.add_media_to_timeline_at(media_id, start);
        }

        // L'utente ha trascinato/cliccato il playhead in questo frame?
        // Serve per forzare un seek anche se si sta riproducendo (bug:
        // "durante il playback lo scrub veniva ignorato") — a differenza
        // di quando è `drive_playback` stesso a spostare il playhead per
        // seguire la riproduzione, che non deve innescare un seek.
        let user_scrubbed_playhead = self.timeline_state.playhead != playhead_before_timeline_ui;
        if user_scrubbed_playhead {
            self.sync_selection_to_playhead();
            // Uno scrub manuale prevale sempre sull'orologio a vuoto:
            // altrimenti `drive_playback` lo riscriverebbe sopra al
            // tentativo dell'utente al frame successivo (stesso principio
            // di `force_seek` qui sotto, ma per `gap_playback`).
            self.gap_playback = None;
        }

        // Interagire con la timeline (selezionare una clip o spostare il
        // playhead) riprende il controllo del viewer dall'anteprima
        // "grezza" del media pool, se attiva.
        if self.timeline_state.selected != selected_before_timeline_ui || user_scrubbed_playhead {
            self.browsing_media = None;
        }

        if self.browsing_media.is_none() {
            self.ensure_active_clip_matches_playhead(user_scrubbed_playhead);
            // `drive_playback` avanza il playhead da sé durante la
            // riproduzione: senza questo confronto, "selection follows
            // playhead" seguiva solo lo scrub manuale (già coperto sopra
            // da `user_scrubbed_playhead`) e restava fermo durante il
            // play normale (bug: "la selezione non segue durante la
            // riproduzione"). Il confronto prima/dopo, anziché una sync
            // incondizionata, lascia intatta un'eventuale selezione
            // esplicita impostata nello stesso frame da altrove (es.
            // `split_all_at_playhead`) quando il playhead in realtà non
            // si muove (caso normale: taglio da fermo).
            let playhead_before_playback = self.timeline_state.playhead;
            self.drive_playback();
            if self.timeline_state.playhead != playhead_before_playback {
                self.sync_selection_to_playhead();
            }
            self.maintain_next_clip_preload();
        }

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
            self.apply_active_clip_gain();
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
            // *locale alla clip* sul playhead della timeline, l'unico
            // orologio che ha senso per un generatore (un Player non ha
            // motivo di esistere per un riempimento uniforme). Nessuna
            // clip attiva (il playhead è su un vuoto della track video, o
            // sta attraversandolo — `gap_playback`) mostra un frame nero
            // allo stesso modo, invece del placeholder testuale o
            // dell'ultimo frame rimasto — come un vero NLE. Non quando si
            // sta sfogliando un media "grezzo" dal media pool
            // (`browsing_media`): lì `active_clip` è `None` di proposito,
            // ma `preview_player`/`frame_texture` mostrano davvero
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
            } else if let Some(frame) = self.preview_player.as_ref().and_then(|p| p.current_frame())
            {
                let source_frame = self
                    .preview_player
                    .as_ref()
                    .map(|p| p.current_source_frame())
                    .unwrap_or(0);
                let transform = self
                    .active_clip_effects()
                    .map(|e| e.transform.value_at(source_frame))
                    .unwrap_or_default();
                let composited = self.compositor.render_frame(
                    &frame.data,
                    frame.width,
                    frame.height,
                    &transform,
                    frame.width,
                    frame.height,
                );
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &composited,
                );
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
            }

            if let Some(texture) = &self.frame_texture {
                let available = ui.available_size();
                let tex_size = texture.size_vec2();
                let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                let display_size = tex_size * scale.max(0.0);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::from_texture(texture).fit_to_exact_size(display_size));
                });
            } else if let Some(err) = &self.preview_error {
                ui.colored_label(egui::Color32::RED, format!("Errore player: {err}"));
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(if self.preview_player.is_some() {
                        "Decodifica in corso..."
                    } else {
                        "Importa un media, aggiungilo alla timeline e premi Spazio."
                    });
                });
            }
        });

        // Anche durante un vuoto attraversato in riproduzione
        // (`gap_playback`): lì non c'è un player la cui riproduzione
        // richieda già repaint da sé, ma il playhead deve comunque
        // avanzare a orologio finché non raggiunge la prossima clip.
        if self.preview_player.as_ref().is_some_and(Player::is_playing)
            || self.gap_playback.is_some()
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
        Box::new(move |_cc| {
            let mut app = VibeVideoApp::default();
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
            linked: None,
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

    /// Esercita il collegamento completo introdotto in milestone 5:
    /// caricare una clip video apre il player sul suo media
    /// (`load_video_clip`) e il gain impostato via comando arriva davvero
    /// all'`AudioPlayer` sottostante, senza panic.
    #[test]
    fn loading_a_media_clip_opens_preview_and_applies_gain() {
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
        app.load_video_clip(clip_id);

        assert_eq!(app.active_clip, Some((0, clip_id)));
        assert!(app.preview_player.is_some(), "doveva aprirsi un player");
        assert_eq!(
            app.active_clip_effects().map(|e| e.gain_db.default),
            Some(0.0)
        );

        self_test_set_gain(&mut app, timeline_id, 0, clip_id, -12.0);
        assert_eq!(
            app.active_clip_effects().map(|e| e.gain_db.default),
            Some(-12.0)
        );
        // Non deve panicare anche se il player è ancora in fase di apertura.
        app.apply_active_clip_gain();
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
    fn loading_solid_color_clip_clears_preview_player_and_sets_active_clip() {
        let mut app = VibeVideoApp::default();
        app.add_solid_color_clip();
        let timeline_id = app.timeline_id.unwrap();
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.load_video_clip(clip_id);

        assert_eq!(app.active_clip, Some((0, clip_id)));
        assert!(app.preview_player.is_none());
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

    /// Riproduce il bug segnalato: trascinare il playhead della timeline
    /// non spostava l'anteprima del player della clip selezionata.
    #[test]
    fn dragging_timeline_playhead_seeks_the_active_media_player() {
        let dir = std::env::temp_dir().join("vv-app-playhead-sync-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=2",
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
        let timeline_id = app.timeline_id.unwrap();
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);
        app.load_video_clip(clip_id);
        // Il player appena aperto non ha ancora un frame in cache: il primo
        // sync forza comunque il seek verso il playhead (0 qui, no-op).
        app.ensure_active_clip_matches_playhead(false);

        // L'utente trascina il playhead a metà clip (frame 25 su 50, clip
        // a 25fps/2s): il player deve seguirlo con un seek.
        app.timeline_state.playhead = 25;
        app.ensure_active_clip_matches_playhead(false);

        let player = app.preview_player.as_ref().expect("player atteso");
        assert_eq!(
            player.current_source_frame(),
            25,
            "il player doveva fare un seek verso il frame del playhead"
        );

        // Da fermo e senza ulteriori spostamenti, un altro sync non deve
        // ri-seekare (idempotenza): verifichiamo indirettamente che
        // last_synced_playhead sia stato aggiornato, riportando il
        // playhead a un valore diverso e controllando che segua di nuovo.
        app.timeline_state.playhead = 10;
        app.ensure_active_clip_matches_playhead(false);
        assert_eq!(
            app.preview_player.as_ref().unwrap().current_source_frame(),
            10
        );
    }

    /// Riproduce il bug segnalato: durante il playback, raggiunto un
    /// taglio (fine del trim della clip), il playhead si fermava lì e il
    /// riquadro continuava a mostrare la clip come se non fosse tagliata.
    /// Verifica invece che si avanzi automaticamente alla clip successiva
    /// sulla track video, restando in riproduzione.
    #[test]
    fn playback_auto_advances_to_next_clip_at_a_cut() {
        let dir = std::env::temp_dir().join("vv-app-auto-advance-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=2",
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
        let timeline_id = app.timeline_id.unwrap();
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let first_clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        // Taglia la clip (50 frame) a metà: due clip, [0,25) e [25,50).
        app.timeline_state.playhead = 25;
        app.split_all_at_playhead();
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(
            tl.tracks[0].clips.len(),
            2,
            "il taglio doveva creare 2 clip"
        );
        let second_clip_id = tl.tracks[0].clips[1].id;
        assert_ne!(first_clip_id, second_clip_id);

        // Riproduci dall'inizio della prima metà.
        app.timeline_state.playhead = 0;
        app.ensure_active_clip_matches_playhead(false);
        assert_eq!(app.active_clip, Some((0, first_clip_id)));
        app.toggle_playback();
        assert!(app.preview_player.as_ref().unwrap().is_playing());

        // Simula "il player ha raggiunto la fine del trim della prima
        // metà" (frame 25, cioè source_out) senza aspettare la decodifica
        // reale: è esattamente la condizione che drive_playback controlla.
        app.preview_player.as_mut().unwrap().seek_to_frame(25);
        app.drive_playback();

        assert_eq!(
            app.active_clip,
            Some((0, second_clip_id)),
            "doveva avanzare alla clip successiva al taglio"
        );
        assert_eq!(app.timeline_state.playhead, 25);
        assert!(
            app.preview_player.as_ref().unwrap().is_playing(),
            "la riproduzione doveva continuare, non fermarsi"
        );
    }

    /// Bug: "selection follows playhead" seguiva solo lo scrub manuale,
    /// non l'avanzamento del playhead durante la normale riproduzione.
    /// `ui()` chiama `sync_selection_to_playhead` anche dopo
    /// `drive_playback` quando il playhead si è mosso: qui si riproduce
    /// esattamente quella sequenza (senza passare da un vero frame egui).
    #[test]
    fn selection_follows_playhead_during_normal_playback() {
        let dir = std::env::temp_dir().join("vv-app-selection-follows-playback-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=2",
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
        assert!(app.selection_follows_playhead, "attivo di default");
        app.import_media(path);
        let timeline_id = app.timeline_id.unwrap();
        let media_id = app.project.media_pool.iter().next().unwrap().0;
        app.add_media_to_timeline(media_id);
        let first_clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.timeline_state.playhead = 25;
        app.split_all_at_playhead();
        let second_clip_id = app.project.timelines[timeline_id].tracks[0].clips[1].id;

        app.timeline_state.playhead = 0;
        app.ensure_active_clip_matches_playhead(false);
        app.toggle_playback();
        assert_eq!(
            app.timeline_state.selected,
            BTreeSet::from([(0, first_clip_id)])
        );

        // Come sopra: simula il player che ha raggiunto la fine del trim
        // della prima metà, poi replica esattamente la sequenza di `ui()`
        // (drive_playback, e se il playhead si è mosso, risincronizza la
        // selezione).
        app.preview_player.as_mut().unwrap().seek_to_frame(25);
        let playhead_before = app.timeline_state.playhead;
        app.drive_playback();
        assert_ne!(
            app.timeline_state.playhead, playhead_before,
            "il playhead deve essersi mosso"
        );
        app.sync_selection_to_playhead();

        assert_eq!(
            app.timeline_state.selected,
            BTreeSet::from([(0, second_clip_id)]),
            "la selezione deve seguire il playhead anche durante il play normale, non solo lo scrub manuale"
        );
    }

    /// Riproduce il bug segnalato: la riproduzione partiva solo se una
    /// clip era selezionata esplicitamente in timeline.
    #[test]
    fn toggle_playback_works_without_any_selection() {
        let dir = std::env::temp_dir().join("vv-app-play-without-selection-test");
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

        assert!(app.timeline_state.selected.is_empty());
        assert_eq!(app.active_clip, None);

        app.toggle_playback();

        assert!(
            app.active_clip.is_some(),
            "doveva agganciare la clip sotto al playhead"
        );
        assert!(
            app.preview_player.as_ref().is_some_and(Player::is_playing),
            "la riproduzione doveva partire senza alcuna selezione"
        );
    }

    /// Due clip attaccate (nessun vuoto tra loro): `advance_playback_past`
    /// deve saltare dritto alla prossima senza passare da `gap_playback`.
    #[test]
    fn advance_playback_past_jumps_directly_when_clips_are_back_to_back() {
        let mut app = VibeVideoApp::default();
        let clip_a = make_timeline_with_clip(&mut app, VIDEO_TRACK, 0, 10);
        let clip_b = make_timeline_with_clip(&mut app, VIDEO_TRACK, 10, 10);
        let timeline_id = app.timeline_id.unwrap();
        app.load_video_clip(clip_a);

        app.advance_playback_past(timeline_id, 10);

        assert!(app.gap_playback.is_none());
        assert_eq!(app.timeline_state.playhead, 10);
        assert_eq!(app.active_clip, Some((VIDEO_TRACK, clip_b)));
    }

    /// Bug segnalato: "quando la playhead passa su un segmento vuoto, non
    /// deve saltare alla prossima clip ma riprodurre una schermata nera".
    /// Se c'è un vuoto prima della prossima clip, `advance_playback_past`
    /// deve entrare in `gap_playback` invece di saltarci dentro subito.
    #[test]
    fn advance_playback_past_enters_gap_playback_when_there_is_a_gap_before_the_next_clip() {
        let mut app = VibeVideoApp::default();
        let clip_a = make_timeline_with_clip(&mut app, VIDEO_TRACK, 0, 10);
        let clip_b = make_timeline_with_clip(&mut app, VIDEO_TRACK, 15, 10);
        let timeline_id = app.timeline_id.unwrap();
        app.load_video_clip(clip_a);

        app.advance_playback_past(timeline_id, 10);

        assert!(
            app.active_clip.is_none(),
            "schermo nero: nessuna clip attiva nel vuoto"
        );
        assert_eq!(app.timeline_state.playhead, 10);
        let gap = app
            .gap_playback
            .as_ref()
            .expect("doveva entrare in gap_playback");
        assert_eq!(gap.start_frame, 10);
        assert_eq!(gap.next_clip_id, clip_b);
        assert_eq!(gap.next_clip_start, 15);
    }

    /// Nessuna clip successiva sulla track video: fine del contenuto, non
    /// deve né saltare né entrare in un `gap_playback` che non porta da
    /// nessuna parte.
    #[test]
    fn advance_playback_past_does_nothing_special_when_there_is_no_next_clip() {
        let mut app = VibeVideoApp::default();
        let clip_a = make_timeline_with_clip(&mut app, VIDEO_TRACK, 0, 10);
        let timeline_id = app.timeline_id.unwrap();
        app.load_video_clip(clip_a);

        app.advance_playback_past(timeline_id, 10);

        assert!(app.gap_playback.is_none());
    }

    /// Mentre `gap_playback` è attivo, `drive_playback` deve avanzare il
    /// playhead a orologio a parete senza saltare subito alla prossima
    /// clip: verifica lo stato intermedio (dentro il vuoto, non ancora
    /// arrivato).
    #[test]
    fn drive_playback_advances_the_playhead_through_a_gap_without_jumping() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, VIDEO_TRACK, 0, 10);
        let clip_b = make_timeline_with_clip(&mut app, VIDEO_TRACK, 100, 10);
        let timeline_id = app.timeline_id.unwrap();
        let _ = timeline_id;

        // Timeline a 25fps (vedi `make_timeline_with_clip`): ~200ms fa
        // equivalgono a circa 5 frame, ben prima dei 100 della prossima
        // clip.
        app.gap_playback = Some(GapPlayback {
            started_at: Instant::now() - std::time::Duration::from_millis(200),
            start_frame: 0,
            next_clip_id: clip_b,
            next_clip_start: 100,
            preloaded_player: None,
        });

        app.drive_playback();

        assert!(app.gap_playback.is_some(), "il vuoto non è ancora finito");
        let playhead = app.timeline_state.playhead;
        assert!(
            playhead > 0 && playhead < 100,
            "playhead={playhead} doveva essere avanzato ma non ancora arrivato"
        );
        assert!(
            app.active_clip.is_none(),
            "schermo nero finché siamo nel vuoto"
        );
    }

    /// Una volta che l'orologio a parete del vuoto ha superato la prossima
    /// clip, `drive_playback` deve consegnare il controllo a quella clip
    /// (uscire da `gap_playback`, agganciare la clip, farla partire).
    #[test]
    fn drive_playback_hands_off_to_the_next_clip_once_the_gap_elapses() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, VIDEO_TRACK, 0, 10);
        let clip_b = make_timeline_with_clip(&mut app, VIDEO_TRACK, 5, 10);
        let timeline_id = app.timeline_id.unwrap();
        let _ = timeline_id;

        app.gap_playback = Some(GapPlayback {
            started_at: Instant::now() - std::time::Duration::from_secs(10),
            start_frame: 0,
            next_clip_id: clip_b,
            next_clip_start: 5,
            preloaded_player: None,
        });

        app.drive_playback();

        assert!(
            app.gap_playback.is_none(),
            "il vuoto doveva essere concluso"
        );
        assert_eq!(app.timeline_state.playhead, 5);
        assert_eq!(app.active_clip, Some((VIDEO_TRACK, clip_b)));
    }

    /// Bug fix per "toggle_playback deve fermare l'orologio a vuoto se
    /// attivo, invece di ignorarlo (non c'è un player da mettere in
    /// pausa)".
    #[test]
    fn toggle_playback_stops_gap_playback_clock() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, VIDEO_TRACK, 0, 10);
        let clip_b = make_timeline_with_clip(&mut app, VIDEO_TRACK, 15, 10);

        app.gap_playback = Some(GapPlayback {
            started_at: Instant::now(),
            start_frame: 10,
            next_clip_id: clip_b,
            next_clip_start: 15,
            preloaded_player: None,
        });

        app.toggle_playback();

        assert!(app.gap_playback.is_none());
    }

    /// Se il playhead è fermo dentro un vuoto (nessun `gap_playback`
    /// attivo, nessuna clip agganciata), `toggle_playback` deve far
    /// ripartire l'orologio a vuoto verso la prossima clip video, non
    /// restare inerte.
    #[test]
    fn toggle_playback_resumes_gap_playback_when_paused_inside_a_gap() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, VIDEO_TRACK, 0, 10);
        let clip_b = make_timeline_with_clip(&mut app, VIDEO_TRACK, 20, 10);
        app.timeline_state.playhead = 15;

        assert!(app.gap_playback.is_none());
        assert!(app.active_clip.is_none());

        app.toggle_playback();

        let gap = app
            .gap_playback
            .as_ref()
            .expect("doveva avviare l'orologio a vuoto verso la prossima clip");
        assert_eq!(gap.start_frame, 15);
        assert_eq!(gap.next_clip_id, clip_b);
        assert_eq!(gap.next_clip_start, 20);
    }

    /// Riproduce il bug segnalato: tagliare con T richiedeva di
    /// selezionare esplicitamente la track video, e ogni track andava
    /// tagliata separatamente. `split_all_at_playhead` non deve dipendere
    /// dalla selezione e deve tagliare tutte le track in un colpo solo.
    #[test]
    fn split_all_at_playhead_cuts_every_track_without_selection() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        assert!(app.timeline_state.selected.is_empty());
        app.timeline_state.playhead = 8;
        app.split_all_at_playhead();

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

    /// Bug: tagliare con T una coppia video+audio collegata scollegava
    /// entrambe le metà (comportamento corretto per un taglio "singolo",
    /// ma non quando entrambi i membri della coppia vengono tagliati
    /// insieme nello stesso punto): dopo, selezionare il video non
    /// evidenziava più l'audio. Le metà sinistra/destra devono restare
    /// collegate tra loro.
    #[test]
    fn split_all_at_playhead_keeps_linked_pair_linked_on_both_halves() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 20);
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                (0, video_id),
                (1, audio_id),
            )),
        );

        app.timeline_state.playhead = 8;
        app.split_all_at_playhead();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips.len(), 2);
        let video_left = &tl.tracks[0].clips[0];
        let video_right = &tl.tracks[0].clips[1];
        let audio_left = &tl.tracks[1].clips[0];
        let audio_right = &tl.tracks[1].clips[1];
        assert_eq!(video_left.id, video_id);
        assert_eq!(audio_left.id, audio_id);
        assert_eq!(video_left.linked, Some(audio_left.id));
        assert_eq!(audio_left.linked, Some(video_left.id));
        assert_eq!(video_right.linked, Some(audio_right.id));
        assert_eq!(audio_right.linked, Some(video_right.id));
        assert_ne!(video_right.id, video_id);
        assert_ne!(audio_right.id, audio_id);

        // "Selection follows playhead" seleziona la metà SINISTRA appena
        // tagliata (quella che si presume già rivista) e la sua gemella
        // audio collegata, non la metà destra sotto al playhead.
        assert_eq!(
            app.timeline_state.selected,
            BTreeSet::from([(0, video_id), (1, audio_id)])
        );

        // Un solo undo annulla i due tagli *e* i due ricollegamenti.
        app.history.undo(&mut app.project);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].linked, Some(audio_id));
        assert_eq!(tl.tracks[1].clips[0].linked, Some(video_id));
    }

    #[test]
    fn can_reuse_player_for_same_media_is_true() {
        let id = MediaId::default();
        assert!(can_reuse_player_for(Some(id), id));
    }

    #[test]
    fn can_reuse_player_for_different_media_is_false() {
        let mut project = vv_core::Project::default();
        let media_a = project.media_pool.insert(dummy_media_item());
        let media_b = project.media_pool.insert(dummy_media_item());
        assert!(!can_reuse_player_for(Some(media_a), media_b));
    }

    #[test]
    fn can_reuse_player_for_no_previous_is_false() {
        assert!(!can_reuse_player_for(None, MediaId::default()));
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
            linked: None,
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

    /// Bug segnalato: due clip dello *stesso* media ma con punti di
    /// attacco non contigui (es. due trim separati incollati altrove)
    /// venivano trattate come una continuazione gratis (riuso col solo
    /// seek), ignorando un eventuale preload già scaldato in anticipo —
    /// il seek costava comunque quanto aprire un player nuovo. Solo la
    /// vera contiguità (`source_out` dell'una == `source_in` dell'altra,
    /// tipica di un T-split) è davvero gratis.
    #[test]
    fn is_seamless_continuation_only_for_the_same_media_and_contiguous_source_ranges() {
        let base = vv_core::Clip {
            id: ClipId(0),
            source: vv_core::ClipSource::Media(MediaId::default()),
            source_in: 0,
            source_out: 50,
            timeline_start: 0,
            effects: vv_core::EffectStack::default(),
            linked: None,
        };

        // Stesso media, contiguo: source_in di b == source_out di a.
        let contiguous = vv_core::Clip {
            source_in: 50,
            timeline_start: 50,
            ..base.clone()
        };
        assert!(is_seamless_continuation(&base, &contiguous));

        // Stesso media, MA non contiguo: due trim separati dello stesso
        // file.
        let non_contiguous = vv_core::Clip {
            source_in: 60,
            timeline_start: 50,
            ..base.clone()
        };
        assert!(!is_seamless_continuation(&base, &non_contiguous));

        // Media diverso, anche se numericamente "contiguo".
        let other_media = vv_core::Clip {
            source: vv_core::ClipSource::Media(
                vv_core::Project::default()
                    .media_pool
                    .insert(dummy_media_item()),
            ),
            source_in: 50,
            timeline_start: 50,
            ..base.clone()
        };
        assert!(!is_seamless_continuation(&base, &other_media));

        // SolidColor non conta mai come continuazione.
        let solid = vv_core::Clip {
            source: vv_core::ClipSource::SolidColor,
            source_in: 50,
            timeline_start: 50,
            ..base.clone()
        };
        assert!(!is_seamless_continuation(&base, &solid));
    }

    fn dummy_media_item() -> vv_core::MediaItem {
        vv_core::MediaItem {
            path: "dummy.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 10,
                fps: vv_core::Rational::new(25, 1),
                width: 1920,
                height: 1080,
                has_audio: false,
                sample_rate: 48000,
                channels: 2,
            },
            content_hash: 0,
        }
    }

    #[test]
    fn delete_selected_removes_linked_audio_partner_together() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                (0, video_id),
                (1, audio_id),
            )),
        );

        app.timeline_state.selected = BTreeSet::from([(0, video_id)]);
        app.delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert!(tl.tracks[0].clips.is_empty());
        assert!(
            tl.tracks[1].clips.is_empty(),
            "la clip audio collegata deve sparire insieme al video"
        );
        assert!(app.timeline_state.selected.is_empty());

        // Un solo undo ripristina entrambe (CompositeCommand).
        app.history.undo(&mut app.project);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 1);
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
    }

    #[test]
    fn copy_then_paste_relinks_a_linked_pair_to_each_other_not_to_the_originals() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 10);
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                (0, video_id),
                (1, audio_id),
            )),
        );

        // Selezionare solo il video deve comunque copiare anche l'audio
        // collegato (stesso principio di `delete_selected`).
        app.timeline_state.selected = BTreeSet::from([(0, video_id)]);
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
        assert_eq!(new_video.linked, Some(new_audio.id));
        assert_eq!(new_audio.linked, Some(new_video.id));
        assert_ne!(
            new_video.linked,
            Some(video_id),
            "non collegata all'originale"
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

    /// Se lo split coinvolge una coppia collegata (paste della coppia
    /// video+audio copiata insieme, che quindi taglia entrambe le track
    /// nello stesso punto), le due metà nuove devono restare collegate
    /// *tra loro*, non alla vecchia gemella (persa nello split).
    #[test]
    fn paste_splitting_a_linked_pair_relinks_the_new_halves_to_each_other() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20); // [0,20)
        let audio_id = make_timeline_with_clip(&mut app, 1, 0, 20); // [0,20)
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                (0, video_id),
                (1, audio_id),
            )),
        );

        let src_video = make_timeline_with_clip(&mut app, 0, 100, 5);
        let src_audio = make_timeline_with_clip(&mut app, 1, 100, 5);
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::LinkClips::new(
                timeline_id,
                (0, src_video),
                (1, src_audio),
            )),
        );
        app.timeline_state.selected = BTreeSet::from([(0, src_video)]);
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
        assert_eq!(video_halves[0].linked, Some(audio_halves[0].id));
        assert_eq!(audio_halves[0].linked, Some(video_halves[0].id));
        assert_eq!(video_halves[1].linked, Some(audio_halves[1].id));
        assert_eq!(audio_halves[1].linked, Some(video_halves[1].id));
    }

    /// Bug segnalato: dopo un vuoto, l'audio della clip successiva parte
    /// subito ma il video resta nero per circa un secondo. Verifica il
    /// pezzo base: `preload_player_for_clip` apre davvero un player per
    /// una clip Media, non per un generatore SolidColor (nessun player
    /// possibile).
    #[test]
    fn preload_player_for_clip_opens_a_player_only_for_media_clips() {
        let dir = std::env::temp_dir().join("vv-app-preload-test");
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
        let media_clip_id = app.project.timelines[timeline_id].tracks[VIDEO_TRACK].clips[0].id;

        assert!(
            app.preload_player_for_clip(media_clip_id).is_some(),
            "doveva aprire un player per la clip video"
        );

        let solid_id = make_timeline_with_clip(&mut app, VIDEO_TRACK, 1000, 10);
        assert!(app.preload_player_for_clip(solid_id).is_none());
    }

    /// Richiesta: "inserisci un indicatore visivo delle porzioni di
    /// timeline presenti in memoria". Verifica l'integrazione end-to-end
    /// (non solo la funzione pura `map_source_ranges_to_timeline`, già
    /// coperta a parte): la clip attiva su un player vero, con del
    /// decode-ahead reale in corso su un thread separato, produce
    /// intervalli bufferizzati entro i propri limiti di timeline.
    #[test]
    fn buffered_timeline_ranges_reports_the_active_clips_decoded_frames() {
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
        let clip = app.project.timelines[timeline_id].tracks[VIDEO_TRACK].clips[0].clone();

        app.load_video_clip(clip.id);
        assert!(app.preview_player.is_some());

        // Il decode-ahead popola la cache su un thread separato: attende
        // che ci sia almeno qualcosa, con un timeout generoso.
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

    /// Bug segnalato: l'indicatore "buffered" mostrava il buffer fermarsi
    /// esattamente a fine clip e ripartire da zero solo quando la testina
    /// raggiungeva la clip successiva — anche su un taglio netto, senza
    /// alcun vuoto in mezzo (il preload esisteva già, ma solo per il caso
    /// "vuoto", avviato solo all'inizio del vuoto stesso). Verifica che
    /// avvicinandosi alla fine della clip attiva (entro
    /// `NEXT_CLIP_PRELOAD_LOOKAHEAD_SECS`) parta il preload della
    /// prossima, prima ancora di raggiungerla.
    #[test]
    fn maintain_next_clip_preload_starts_warming_up_the_next_clip_within_the_lookahead_window() {
        let dir = std::env::temp_dir().join("vv-app-preload-lookahead-test");
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

        app.add_media_to_timeline_at(media_a, 0); // [0,50)
        app.add_media_to_timeline_at(media_b, 50); // [50,75), adiacente
        let timeline_id = app.timeline_id.unwrap();
        let clip_a_id = app.project.timelines[timeline_id].tracks[VIDEO_TRACK].clips[0].id;
        let clip_b_id = app.project.timelines[timeline_id].tracks[VIDEO_TRACK].clips[1].id;

        app.load_video_clip(clip_a_id);
        let player = app.preview_player.as_mut().expect("player atteso");
        player.seek_to_frame(48); // 2 frame residui = 0.08s, entro i 3s di lookahead
        player.play();

        app.maintain_next_clip_preload();

        let preload = app
            .next_preload
            .as_ref()
            .expect("doveva iniziare a precaricare la prossima clip");
        assert_eq!(preload.clip_id, clip_b_id);
    }

    /// Lontano dalla fine della clip attiva, non deve ancora precaricare
    /// nulla (eviterebbe di tenere sempre due player aperti per l'intera
    /// durata della riproduzione, vanificando il contenimento della
    /// memoria discusso in precedenza).
    #[test]
    fn maintain_next_clip_preload_does_nothing_far_from_the_end_of_the_clip() {
        let dir = std::env::temp_dir().join("vv-app-preload-lookahead-far-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("clip_a.mp4");
        let path_b = dir.join("clip_b.mp4");
        for (path, duration) in [(&path_a, 5), (&path_b, 1)] {
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

        app.add_media_to_timeline_at(media_a, 0); // [0,125)
        app.add_media_to_timeline_at(media_b, 125);

        let clip_a_id =
            app.project.timelines[app.timeline_id.unwrap()].tracks[VIDEO_TRACK].clips[0].id;
        app.load_video_clip(clip_a_id);
        let player = app.preview_player.as_mut().expect("player atteso");
        player.seek_to_frame(0); // 5s residui, ben oltre i 3s di lookahead
        player.play();

        app.maintain_next_clip_preload();

        assert!(app.next_preload.is_none());
    }

    /// Test end-to-end del bug segnalato: su un taglio netto tra due clip
    /// di media diversi, il preload avviato in anticipo da
    /// `maintain_next_clip_preload` deve essere quello effettivamente
    /// usato al momento del cambio (`advance_playback_past`), non un
    /// player aperto a freddo lì — verificato indirettamente controllando
    /// che l'indicatore "buffered" non sia vuoto *subito dopo* il cambio,
    /// senza alcuna attesa.
    #[test]
    fn advance_playback_past_uses_a_proactively_preloaded_player_at_a_straight_cut() {
        let dir = std::env::temp_dir().join("vv-app-preload-handoff-cut-test");
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

        app.add_media_to_timeline_at(media_a, 0); // [0,50)
        app.add_media_to_timeline_at(media_b, 50); // [50,75), adiacente
        let timeline_id = app.timeline_id.unwrap();
        let clip_b_id = app.project.timelines[timeline_id].tracks[VIDEO_TRACK].clips[1].id;

        let clip_a_id = app.project.timelines[timeline_id].tracks[VIDEO_TRACK].clips[0].id;
        app.load_video_clip(clip_a_id);
        let player = app.preview_player.as_mut().expect("player atteso");
        player.seek_to_frame(48);
        player.play();
        app.maintain_next_clip_preload();
        assert!(app.next_preload.is_some(), "il preload doveva partire");

        // Attende che il preload abbia già qualcosa in cache, altrimenti il
        // test non distinguerebbe "preload usato ma ancora vuoto" da
        // "preload non usato affatto".
        let start = std::time::Instant::now();
        loop {
            if app
                .next_preload
                .as_ref()
                .is_some_and(|p| !p.player.cached_source_ranges().is_empty())
            {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "timeout: il preload non ha mai bufferizzato nulla"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        app.advance_playback_past(timeline_id, 50);

        assert_eq!(app.active_clip, Some((VIDEO_TRACK, clip_b_id)));
        assert!(
            app.next_preload.is_none(),
            "il preload consumato deve svuotarsi"
        );
        assert!(
            !app.buffered_timeline_ranges().is_empty(),
            "il buffer doveva essere già presente subito dopo il taglio, non ripartire da zero"
        );
    }

    /// Bug segnalato: "l'indicatore sembra bufferizzare fino alla clip
    /// successiva, ma poi entra comunque tardi di 1s come se non ci
    /// fosse alcun buffer". Causa: due clip dello *stesso* media ma con
    /// punti di attacco non contigui (es. due trim separati dello stesso
    /// file incollati altrove, come nel caso segnalato) venivano
    /// riconosciute come "stesso media" e quindi riusate col solo seek,
    /// ignorando il preload — che pure era stato avviato e mostrava
    /// progresso nell'indicatore, ma restava inutilizzato. Verifica che
    /// in questo caso specifico (stesso media, non contiguo) il preload
    /// venga comunque usato.
    #[test]
    fn load_video_clip_uses_a_matching_preload_for_the_same_media_when_not_contiguous() {
        let dir = std::env::temp_dir().join("vv-app-same-media-noncontiguous-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=3",
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
        let meta = app.project.media_pool.get(media_id).unwrap().meta.clone();
        let timeline_id = app.ensure_timeline_for(&meta);

        // Clip A: source [0,50) a timeline [0,50). Clip B: source [60,75)
        // — stesso media, ma NON contiguo (60 != 50) — a timeline [50,65).
        let clip_a_id = app.project.alloc_clip_id();
        let clip_a = vv_core::Clip {
            id: clip_a_id,
            source: vv_core::ClipSource::Media(media_id),
            source_in: 0,
            source_out: 50,
            timeline_start: 0,
            effects: vv_core::EffectStack::default(),
            linked: None,
        };
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: VIDEO_TRACK,
                clip: clip_a,
            }),
        );
        let clip_b_id = app.project.alloc_clip_id();
        let clip_b = vv_core::Clip {
            id: clip_b_id,
            source: vv_core::ClipSource::Media(media_id),
            source_in: 60,
            source_out: 75,
            timeline_start: 50,
            effects: vv_core::EffectStack::default(),
            linked: None,
        };
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: VIDEO_TRACK,
                clip: clip_b,
            }),
        );

        app.load_video_clip(clip_a_id);
        let player = app.preview_player.as_mut().expect("player atteso");
        player.seek_to_frame(48);
        player.play();
        app.maintain_next_clip_preload();
        let preload = app
            .next_preload
            .as_ref()
            .expect("doveva precaricare anche se stesso media (non contiguo)");
        assert_eq!(preload.clip_id, clip_b_id);

        let start = std::time::Instant::now();
        loop {
            if app
                .next_preload
                .as_ref()
                .is_some_and(|p| !p.player.cached_source_ranges().is_empty())
            {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "timeout: il preload non ha mai bufferizzato nulla"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        app.advance_playback_past(timeline_id, 50);

        assert_eq!(app.active_clip, Some((VIDEO_TRACK, clip_b_id)));
        assert!(
            app.next_preload.is_none(),
            "il preload consumato deve svuotarsi"
        );
        assert_eq!(
            app.preview_player.as_ref().unwrap().current_source_frame(),
            60
        );
        // Il segnale che distingue davvero "ha usato il preload" da "ha
        // riusato il vecchio player con un seek" (il bug): un seek fresco
        // flusha il decoder e parte da una cache vuota, che il
        // decode-ahead impiega un momento a ripopolare — qui invece deve
        // già avere qualcosa, ereditato dal preload.
        assert!(
            !app.buffered_timeline_ranges().is_empty(),
            "il buffer doveva essere già presente subito dopo il taglio, non ripartire da zero"
        );
    }

    /// Se `gap_playback` ha già un player precaricato (vedi
    /// `begin_gap_playback`), il cambio di controllo a fine vuoto lo usa
    /// direttamente invece di riaprirne uno da zero — verifica che parta
    /// a suonare e che `active_clip`/`preview_player` risultino coerenti.
    #[test]
    fn drive_playback_hands_off_using_the_preloaded_player_when_available() {
        let dir = std::env::temp_dir().join("vv-app-preload-handoff-test");
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
        let _ = timeline_id;
        let media_clip_id =
            app.project.timelines[app.timeline_id.unwrap()].tracks[VIDEO_TRACK].clips[0].id;

        let preloaded = app.preload_player_for_clip(media_clip_id);
        assert!(preloaded.is_some());
        app.gap_playback = Some(GapPlayback {
            started_at: Instant::now() - std::time::Duration::from_secs(10),
            start_frame: 0,
            next_clip_id: media_clip_id,
            next_clip_start: 0,
            preloaded_player: preloaded,
        });

        app.drive_playback();

        assert!(app.gap_playback.is_none());
        assert_eq!(app.active_clip, Some((VIDEO_TRACK, media_clip_id)));
        assert!(
            app.preview_player.as_ref().is_some_and(Player::is_playing),
            "il player precaricato doveva partire a suonare"
        );
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
}
