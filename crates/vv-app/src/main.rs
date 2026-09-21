//! Finestra egui: media pool, viewer, pannello proprietà, timeline. Il
//! viewer compone sulla GPU le clip attive di tutte le track video e passa
//! la texture a egui-wgpu senza readback (REFACTOR_PIPELINE.md B2).

#[macro_use]
extern crate rust_i18n;
rust_i18n::i18n!("locales", fallback = "en");

mod app_menu;
mod export;
mod export_dialog;
mod frame_provider;
mod i18n;
mod keyframe_editor;
mod media_pool;
mod media_pool_ui;
mod mix_buffers;
mod project_io;
mod properties_panel;
mod proxy_worker;
mod render_ahead;
mod settings;
mod settings_dialog;
mod thumbnail_worker;
mod timeline_audio;
mod timeline_ui;
mod transport;
mod viewer_overlay;
mod waveform_worker;
#[cfg(target_os = "linux")]
mod wayland_dnd;
mod worker;

use eframe::wgpu;
use media_pool_ui::*;
use project_io::*;
use properties_panel::*;
use settings::Action;
use timeline_audio::TimelineAudio;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use vv_core::{ClipId, FrameIdx, MediaId, TimelineId, Track, TrackKind};

/// ~6 s di margine a 1080p, ~1,5 s a 4K.
const DEFAULT_CACHE_BUDGET_BYTES: usize = 1_200_000_000;

/// Colore di una clip Solid Color appena creata.
const DEFAULT_SOLID_COLOR: vv_core::Rgba = vv_core::Rgba { r: 1.0, g: 1.0, b: 0.0, a: 1.0 };

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

/// Una clip selezionata, bersaglio del pannello proprietà. `source_frame`
/// è il playhead nello spazio sorgente di *questa* clip.
#[derive(Debug, Clone, Copy)]
struct PanelTarget {
    timeline: TimelineId,
    track_index: usize,
    clip_id: ClipId,
    source_frame: FrameIdx,
    timeline_start: FrameIdx,
    is_solid_color: bool,
    is_text: bool,
}

/// Scheda del pannello proprietà: i parametri di una clip video, quelli
/// della sua parte audio, o l'elenco di tutto quel che è selezionato.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertiesTab {
    Video,
    Audio,
    Selection,
}

/// Sotto-scheda della scheda Video per le clip di testo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoSubTab {
    Title,
    Settings,
}

/// Font di sistema per il pannello Title, letti la prima volta che servono.
#[derive(Default)]
struct FontCatalog {
    families: Option<Vec<String>>,
    faces: HashMap<String, Vec<vv_render::text::FontFace>>,
}

impl FontCatalog {
    fn families(&mut self) -> &[String] {
        self.families.get_or_insert_with(vv_render::text::font_families)
    }

    fn faces(&mut self, family: &str) -> &[vv_render::text::FontFace] {
        self.faces
            .entry(family.to_owned())
            .or_insert_with(|| vv_render::text::font_faces(family))
    }
}

#[derive(Debug, Clone)]
struct ClipPanelInfo {
    is_solid_color: bool,
    /// Risoluzione nativa del media della clip e risoluzione della
    /// timeline: le unità dei parametri in pixel del `Transform` (il crop
    /// nella prima, posizione e anchor nella seconda).
    source_size: (u32, u32),
    timeline_size: (u32, u32),
    /// Stato dei keyframe di ogni parametro, indicizzato da
    /// `TransformParam::index`.
    params: Vec<RowKeyframe>,
    transform: vv_core::Transform,
    gain_kf_here: bool,
    gain: f32,
    /// Keyframe di gain più vicini prima/dopo, in frame sorgente: le frecce
    /// di navigazione della riga Volume.
    gain_prev: Option<FrameIdx>,
    gain_next: Option<FrameIdx>,
    color_kf_here: bool,
    color: vv_core::Rgba,
    title: Option<vv_core::TitleParams>,
    /// Vuota finché non si trascina un filtro sulla clip dal pannello
    /// Effects; poi uno per ciascuno, nell'ordine di applicazione.
    filters: Vec<vv_core::ClipFilter>,
    blend_mode: vv_core::BlendMode,
}

/// Dove atterrano le clip di un drop dal media pool, risolto una volta per
/// l'intero drop (vedi `resolve_drop_tracks`): `extra_audio` è la track
/// audio creata al volo, che ha la precedenza sulle esistenti.
#[derive(Debug, Clone, Copy)]
struct DropTracks {
    /// `None` se nel drop non c'è video: nessuna track video da creare.
    video: Option<usize>,
    extra_audio: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewerFrameKind {
    Video,
    /// La clip sotto la testina punta a un media non più nel media pool.
    Offline,
}

struct VibeVideoApp {
    project: vv_core::Project,
    history: vv_core::History,
    timeline_id: Option<TimelineId>,
    /// Le timeline "sopra" `timeline_id`, dalla radice in giù, quando si è
    /// entrati in una compound clip con un doppio click (vedi
    /// `enter_compound_timeline`): vuoto quando si sta editando la
    /// timeline del progetto. Il breadcrumb sopra la timeline le mostra.
    timeline_stack: Vec<TimelineId>,
    timeline_state: timeline_ui::TimelineState,
    media_pool_state: media_pool::MediaPoolState,
    keyframe_editor: keyframe_editor::KeyframeEditorState,
    /// Media o elementi non importati, mostrati in una finestra a parte
    /// finché l'utente non la chiude.
    import_warnings: Vec<String>,

    preview_meta: Option<vv_core::MediaMeta>,
    preview_error: Option<String>,
    /// Texture del viewer registrata in egui-wgpu una volta sola, poi
    /// aggiornata: registrarne una nuova a ogni frame perderebbe la precedente.
    /// `None` senza device condiviso (test).
    video_texture_id: Option<egui::TextureId>,
    video_display_size: Option<egui::Vec2>,
    /// Cosa mostra il viewer: l'ultima composizione, anche se in questo
    /// frame non se n'è fatta una nuova (niente flash a vuoto).
    last_viewer_frame_kind: Option<ViewerFrameKind>,
    /// Device/queue di egui-wgpu, condivisi col compositor: una texture di un
    /// altro device non si può registrare in egui. `None` nei test.
    egui_render_state: Option<eframe::egui_wgpu::RenderState>,
    /// Buffer video dell'anteprima del media pool: un `RenderAhead` su una
    /// timeline con la sola clip del media, e l'id del media lì dentro.
    browsing_render_ahead: Option<(render_ahead::RenderAhead, MediaId)>,

    /// Budget di memoria (byte) per la cache dei frame decodificati di ogni
    /// `RenderAhead`, configurabile dal menu.
    cache_budget_bytes: usize,

    /// Anteprima dal proxy tutto-intra quando pronto: scrub fluido su sorgenti
    /// long-GOP. L'export usa sempre i sorgenti.
    proxy_enabled: bool,
    /// Genera i proxy anche col toggle spento, così sono pronti quando lo si
    /// riattiva. Creato al primo import.
    proxy_worker: Option<proxy_worker::ProxyWorker>,
    /// L'export ha messo in pausa i proxy e deve riprenderli; `false` se la
    /// pausa era dell'utente.
    proxy_paused_for_export: bool,
    /// Genera in background le waveform dei media con audio. Creato al primo
    /// import.
    waveform_worker: Option<waveform_worker::WaveformWorker>,
    thumbnail_worker: Option<thumbnail_worker::ThumbnailWorker>,
    /// Miniature del media pool per `content_hash`; `None` = richiesta in
    /// corso o fallita (evita di riaccodarla a ogni frame).
    thumbnails: HashMap<u64, Option<egui::TextureHandle>>,
    /// Waveform lette dai file di cache, per `(content_hash, stream_index)`:
    /// la timeline le disegna a ogni frame, rileggerle da disco no.
    waveform_cache: HashMap<(u64, usize), vv_media::Waveform>,
    /// Waveform cercate su disco e non trovate: si riprova solo quando il
    /// worker le segnala pronte, non a ogni frame.
    waveform_missing: std::collections::HashSet<(u64, usize)>,
    /// Compound clip la cui waveform è stata composta con dei sorgenti
    /// non ancora pronti: da rifare quando arrivano.
    waveform_partial: std::collections::HashSet<(u64, usize)>,
    /// Secondi bufferizzati avanti/dietro la testina (menu Playback > Proxy).
    lookahead_secs: f64,
    behind_secs: f64,

    /// Clip video mostrata nel viewer (quella sotto al playhead), da cui si
    /// leggono frame e transform. `None` su un vuoto o durante l'anteprima
    /// dal media pool.
    active_clip: Option<(usize, ClipId)>,
    compositor: vv_render::Compositor,
    /// Aperto alla prima modifica fatta a puntatore premuto, chiuso al
    /// rilascio: un trascinamento è un solo passo di undo.
    edit_drag_group: Option<vv_core::GroupMark>,

    /// Ultimo playhead gestito: distingue quello mosso dal clock (nessun seek)
    /// da quello spostato dall'utente.
    last_synced_playhead: FrameIdx,

    /// Media in anteprima dal pool: il viewer mostra lui invece della timeline,
    /// finché non si interagisce con la timeline.
    browsing_media: Option<MediaId>,
    browse_playhead: FrameIdx,
    /// In/out dell'anteprima: la porzione trascinata dal viewer sulla timeline.
    browse_marks: transport::MarkRange,
    browse_audio_streams: usize,

    /// Buffer video della timeline. `None` finché non c'è una timeline.
    render_ahead: Option<render_ahead::RenderAhead>,
    /// `history.generation()` all'ultimo aggiornamento di `render_ahead`.
    render_ahead_generation: u64,
    /// `history.generation()` all'ultimo `sync_root_timeline_media`.
    root_timeline_media_generation: u64,

    /// Mixer delle track audio e clock del playback della timeline.
    /// `None` finché non serve (nei test si apre solo se usato).
    timeline_audio: Option<TimelineAudio>,

    /// Moltiplicatore di velocità (1/2/4/8x) impostato dal tasto "a"; la
    /// barra spaziatrice mette in pausa e lo riporta a 1x.
    playback_speed: f64,

    /// Spostando il playhead, tagliando o cancellando si seleziona la clip
    /// video sotto al playhead.
    selection_follows_playhead: bool,

    /// Audio durante lo scrub (menu Timeline): attivo di default.
    scrub_audio: bool,

    arrow_hold: Option<ArrowHold>,

    /// Calamita: nei drag le clip si agganciano ai bordi vicini.
    snapping_enabled: bool,
    /// Handle di transform sopra il viewer (pulsante sotto al viewer).
    show_transform_overlay: bool,
    overlay_drag: Option<viewer_overlay::OverlayDrag>,

    /// Zoom X e Y del pannello Transform tenuti insieme (il lucchetto tra i
    /// due campi): preferenza della UI, non del progetto.
    zoom_link: bool,

    /// Scheda aperta nel pannello proprietà.
    properties_tab: PropertiesTab,
    video_subtab: VideoSubTab,
    fonts: FontCatalog,

    export: Option<ExportUiState>,
    export_dialog: Option<export_dialog::ExportDialog>,
    /// Riproposte al prossimo export della sessione.
    last_export_settings: Option<export::ExportSettings>,

    /// `None` finché non salvato: "Salva" si comporta come "Salva con nome".
    current_project_path: Option<PathBuf>,
    /// `history.generation()` all'ultimo salvataggio o apertura.
    saved_generation: u64,
    /// Media importati dopo l'ultimo salvataggio: il media pool cambia
    /// senza passare dalla history.
    unsaved_media: bool,
    /// Apertura, import o uscita in attesa della risposta a "salvare le
    /// modifiche?".
    pending_project_switch: Option<ProjectSwitch>,
    /// L'utente ha già risposto per l'uscita: la prossima richiesta di
    /// chiusura passa.
    quit_confirmed: bool,
    /// Ultimo errore di salvataggio/apertura progetto, mostrato in
    /// toolbar accanto ai pulsanti — separato da `import_warnings` (quelli sono
    /// per l'import media, contesto diverso).
    project_error: Option<String>,
    /// Esito dell'ultimo relink dal menu contestuale del media pool,
    /// mostrato in una finestrella a parte (vedi `show_relink_message`).
    relink_message: Option<String>,
    /// File dialog aperto in un thread a parte: sul thread dell'event loop
    /// GNOME/Wayland segnala l'app come bloccata.
    pending_dialog: Option<PendingDialog>,

    /// Audiometer (toggle in Visualizza): una fascia stretta a destra
    /// della timeline con il livello dell'audio in uscita. Attivo di
    /// default, come nella maggior parte degli NLE.
    audiometer_enabled: bool,
    /// Livelli del meter, con un decadimento: il picco istantaneo farebbe
    /// scendere le barre a scatti.
    audiometer_level: (f32, f32),
    viewer_fullscreen: bool,
    settings: settings::Settings,
    /// `None` nei test: le impostazioni non vengono mai scritte su disco.
    settings_path: Option<PathBuf>,
    settings_dialog: Option<settings_dialog::SettingsDialog>,
    about_open: bool,
    #[cfg(target_os = "linux")]
    wayland_dnd: Option<wayland_dnd::WaylandDnd>,
}

impl Default for VibeVideoApp {
    fn default() -> Self {
        Self {
            project: vv_core::Project::default(),
            history: vv_core::History::default(),
            timeline_id: None,
            timeline_stack: Vec::new(),
            timeline_state: timeline_ui::TimelineState::default(),
            media_pool_state: media_pool::MediaPoolState::default(),
            keyframe_editor: keyframe_editor::KeyframeEditorState::default(),
            import_warnings: Vec::new(),
            preview_meta: None,
            preview_error: None,
            video_texture_id: None,
            video_display_size: None,
            last_viewer_frame_kind: None,
            egui_render_state: None,
            browsing_render_ahead: None,
            cache_budget_bytes: DEFAULT_CACHE_BUDGET_BYTES,
            proxy_enabled: true,
            proxy_worker: None,
            proxy_paused_for_export: false,
            waveform_worker: None,
            thumbnail_worker: None,
            thumbnails: HashMap::new(),
            waveform_cache: HashMap::new(),
            waveform_missing: Default::default(),
            waveform_partial: Default::default(),
            lookahead_secs: render_ahead::DEFAULT_LOOKAHEAD_SECS,
            behind_secs: render_ahead::DEFAULT_BEHIND_SECS,
            active_clip: None,
            compositor: vv_render::Compositor::new_headless(),
            edit_drag_group: None,
            last_synced_playhead: 0,
            browsing_media: None,
            browse_playhead: 0,
            browse_marks: transport::MarkRange::default(),
            browse_audio_streams: 0,
            render_ahead: None,
            render_ahead_generation: 0,
            root_timeline_media_generation: 0,
            timeline_audio: None,
            playback_speed: 1.0,
            selection_follows_playhead: true,
            scrub_audio: true,
            arrow_hold: None,
            snapping_enabled: true,
            show_transform_overlay: true,
            overlay_drag: None,
            zoom_link: true,
            properties_tab: PropertiesTab::Video,
            video_subtab: VideoSubTab::Title,
            fonts: FontCatalog::default(),
            export: None,
            export_dialog: None,
            last_export_settings: None,
            current_project_path: None,
            saved_generation: 0,
            unsaved_media: false,
            pending_project_switch: None,
            quit_confirmed: false,
            project_error: None,
            relink_message: None,
            pending_dialog: None,
            audiometer_enabled: true,
            audiometer_level: (0.0, 0.0),
            viewer_fullscreen: false,
            settings: settings::Settings::default(),
            settings_path: None,
            settings_dialog: None,
            about_open: false,
            #[cfg(target_os = "linux")]
            wayland_dnd: None,
        }
    }
}

impl VibeVideoApp {

    /// Meter stereo del picco d'uscita, con decadimento.
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

        // Repaint finché le barre scendono.
        if level_l > 0.001 || level_r > 0.001 {
            ui.ctx().request_repaint();
        }
    }

    /// Anteprima "grezza" di un media dal media pool, non legata alla
    /// timeline: mostra il primo frame da un decode-ahead dedicato.
    fn preview_media(&mut self, media_id: MediaId) {
        self.browsing_render_ahead = None;
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let (path, meta) = (item.path.clone(), item.meta.clone());
        if self.is_timeline_playing() {
            self.timeline_audio().pause();
        }
        self.reset_playback_speed_to_normal();
        self.browse_audio_streams = meta.audio_stream_count();
        let has_video = meta.has_video;
        self.preview_meta = Some(meta);
        self.preview_error = None;
        self.browse_playhead = 0;
        self.browse_marks = transport::MarkRange::default();
        if !has_video {
            return;
        }
        // Il buffer apre il file per conto suo e salta quelli che non si
        // aprono: l'errore va visto qui.
        if let Err(e) = vv_media::Decoder::open(&path) {
            self.preview_error = Some(e.to_string());
            return;
        }
        self.browsing_render_ahead = Some(self.spawn_browsing_render_ahead(media_id));
    }

    /// Una timeline all'fps del media con solo lui sopra: i frame di
    /// timeline coincidono con quelli sorgente.
    fn spawn_browsing_render_ahead(&self, media_id: MediaId) -> (render_ahead::RenderAhead, MediaId) {
        let item = self.project.media_pool[media_id].clone();
        let meta = item.meta.clone();
        let mut project = vv_core::Project::default();
        let preview_media = project.media_pool.insert(item);
        let mut track = Track::new(TrackKind::Video);
        track.clips.push(vv_core::Clip::from_source_range(
            project.alloc_clip_id(),
            vv_core::ClipSource::Media(preview_media),
            0,
            meta.duration_frames.max(1),
            0,
            vv_core::Rational::one(),
        ));
        let timeline_id = project.timelines.insert(vv_core::Timeline {
            name: "anteprima".into(),
            fps: meta.fps,
            resolution: (meta.width, meta.height),
            tracks: vec![track],
        });
        let render_ahead = render_ahead::RenderAhead::spawn(
            project,
            timeline_id,
            self.cache_budget_bytes,
            self.proxy_enabled,
            self.lookahead_secs,
            self.behind_secs,
        );
        (render_ahead, preview_media)
    }

    /// Il buffer della timeline e quello dell'anteprima, se ci sono.
    fn render_aheads(&self) -> impl Iterator<Item = &render_ahead::RenderAhead> {
        self.render_ahead
            .iter()
            .chain(self.browsing_render_ahead.as_ref().map(|(r, _)| r))
    }

    /// La clip Video attiva (track più in alto tra quelle che ne hanno una
    /// in quel punto, `Timeline::active_video_clip_at`) al frame `frame`,
    /// con la sua track — quella che il viewer mostra.
    fn active_video_clip_at(&self, frame: FrameIdx) -> Option<(usize, ClipId)> {
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .active_video_clip_at(frame)
            .map(|(t, c)| (t, c.id))
    }

    /// Con "selection follows playhead" attivo, seleziona la clip video sotto
    /// al playhead e il suo gruppo (niente su un vuoto).
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
            selected.extend(self.project.timelines[timeline_id].linked_members(track_index, clip_id));
        }
        self.timeline_state
            .set_selection(selected, Some((track_index, clip_id)));
        if let Some(timeline_id) = self.timeline_id {
            self.timeline_state
                .drop_locked(&self.project.timelines[timeline_id]);
        }
    }

    /// Allinea la clip del viewer al playhead e, se l'ha mosso l'utente
    /// (`force_seek` anche in riproduzione), il clock audio.
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
        let fps = self.browse_fps();
        let Some(audio) = &mut self.timeline_audio else {
            return;
        };
        if let Some(media_id) = self.browsing_media {
            if let Some(item) = self.project.media_pool.get(media_id) {
                audio.sync_media(&item.path, self.browse_audio_streams, fps);
            }
        } else if let Some(timeline_id) = self.timeline_id {
            audio.sync(&self.project, timeline_id, self.history.generation());
        }
    }

    fn stop_browsing(&mut self) {
        if self.browsing_media.is_some() && self.is_timeline_playing() {
            self.timeline_audio().pause();
            self.reset_playback_speed_to_normal();
        }
        self.browsing_media = None;
        self.browsing_render_ahead = None;
        self.preview_meta = None;
        // Il clock è rimasto alla posizione dell'anteprima: forza il seek al
        // playhead della timeline.
        self.last_synced_playhead = FrameIdx::MIN;
    }

    fn browse_total_frames(&self) -> FrameIdx {
        self.preview_meta.as_ref().map_or(0, |m| m.duration_frames)
    }

    fn browse_fps(&self) -> f64 {
        self.preview_meta.as_ref().map_or(1.0, |m| m.fps.as_f64().max(1e-9))
    }

    /// Il clock del mixer fa da testina anche per l'anteprima, come per la
    /// timeline.
    fn toggle_browse_playback(&mut self) {
        if self.is_timeline_playing() {
            self.timeline_audio().pause();
            self.reset_playback_speed_to_normal();
            return;
        }
        let total = self.browse_total_frames();
        if total <= 0 {
            return;
        }
        if self.browse_playhead >= total - 1 {
            self.browse_playhead = 0;
        }
        self.seek_browse(self.browse_playhead);
        self.timeline_audio();
        self.sync_timeline_audio();
        self.timeline_audio().play();
    }

    fn seek_browse(&mut self, frame: FrameIdx) {
        let frame = frame.clamp(0, (self.browse_total_frames() - 1).max(0));
        self.browse_playhead = frame;
        let fps = self.browse_fps();
        if let Some(audio) = &mut self.timeline_audio {
            audio.seek_frame(frame, fps);
        }
        if let Some((render_ahead, _)) = &self.browsing_render_ahead {
            render_ahead.set_target(frame);
        }
    }

    fn drive_browse_playback(&mut self) {
        if self.browsing_media.is_none() || !self.is_timeline_playing() {
            return;
        }
        let fps = self.browse_fps();
        let last = (self.browse_total_frames() - 1).max(0);
        let frame = self.timeline_audio().position_frame(fps);
        self.browse_playhead = frame.min(last);
        if frame >= last {
            self.timeline_audio().pause();
            self.reset_playback_speed_to_normal();
            self.timeline_audio().seek_frame(last, fps);
        }
    }

    /// Tasti I/O: sull'anteprima se attiva, altrimenti sulla timeline.
    fn mark_at_playhead(&mut self, is_in: bool) {
        let (marks, frame, total) = if self.browsing_media.is_some() {
            let total = self.browse_total_frames();
            (&mut self.browse_marks, self.browse_playhead, total)
        } else if let Some(timeline_id) = self.timeline_id {
            let total = self.project.timelines[timeline_id].total_frames();
            let state = &mut self.timeline_state;
            (&mut state.export_marks, state.playhead, total)
        } else {
            return;
        };
        if is_in {
            marks.set_in(frame, total);
        } else {
            marks.set_out(frame, total);
        }
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
        if self.browsing_media.is_some() {
            self.toggle_browse_playback();
            return;
        }
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
        if self.browsing_media.is_some() {
            if !self.is_timeline_playing() {
                self.toggle_browse_playback();
            }
            return;
        }
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

    /// Intervalli di timeline già decodificati, per la striscia "buffered".
    fn buffered_timeline_ranges(&self) -> Vec<(FrameIdx, FrameIdx)> {
        let Some(timeline_id) = self.timeline_id else {
            return Vec::new();
        };
        if self.render_ahead.is_none() {
            return Vec::new();
        }
        let mut cached: HashMap<MediaId, Vec<(FrameIdx, FrameIdx)>> = HashMap::new();
        self.cached_ranges_of_timeline(timeline_id, &mut cached, 0)
    }

    /// Intervalli decodificati di `timeline_id`, in frame di quella
    /// timeline. Una compound clip non ha frame in cache propri (non è un
    /// media da decodificare): i suoi valgono quelli della sua timeline
    /// annidata, rimappati attraverso la clip.
    fn cached_ranges_of_timeline(
        &self,
        timeline_id: TimelineId,
        cached: &mut HashMap<MediaId, Vec<(FrameIdx, FrameIdx)>>,
        depth: usize,
    ) -> Vec<(FrameIdx, FrameIdx)> {
        let (Some(render_ahead), Some(timeline)) =
            (&self.render_ahead, self.project.timelines.get(timeline_id))
        else {
            return Vec::new();
        };
        let clips: Vec<(MediaId, &vv_core::Clip)> = timeline
            .tracks_of_kind(TrackKind::Video)
            .flat_map(|(_, track)| track.clips.iter())
            .filter_map(|clip| match clip.source {
                vv_core::ClipSource::Media(media_id) => Some((media_id, clip)),
                _ => None,
            })
            .collect();
        let mut ranges = Vec::new();
        for (media_id, clip) in clips {
            if !cached.contains_key(&media_id) {
                let nested = self
                    .project
                    .media_pool
                    .get(media_id)
                    .and_then(|item| item.compound)
                    .filter(|_| depth < MAX_COMPOUND_WALK_DEPTH);
                let source_ranges = match nested {
                    Some(nested_id) => self.cached_ranges_of_timeline(nested_id, cached, depth + 1),
                    None => render_ahead.cached_ranges_for(media_id),
                };
                cached.insert(media_id, source_ranges);
            }
            ranges.extend(map_source_ranges_to_timeline(clip, &cached[&media_id]));
        }
        ranges
    }

    /// Intervalli di timeline delle clip Media servite dal proxy (tutta la
    /// clip, non solo la parte già decodificata).
    fn proxy_timeline_ranges(&self) -> Vec<(FrameIdx, FrameIdx)> {
        let Some(timeline_id) = self.timeline_id else {
            return Vec::new();
        };
        if !self.proxy_enabled {
            return Vec::new();
        }
        let Some(proxy_worker) = &self.proxy_worker else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        for (_, track) in self.project.timelines[timeline_id].tracks_of_kind(TrackKind::Video) {
            for clip in &track.clips {
                if let vv_core::ClipSource::Media(media_id) = &clip.source
                    && let Some(item) = self.project.media_pool.get(*media_id)
                    && proxy_worker.state(item.content_hash) == Some(proxy_worker::ProxyState::Ready)
                {
                    ranges.push((clip.timeline_start, clip.timeline_end() - 1));
                }
            }
        }
        ranges
    }

    /// Carica in `waveform_cache` le waveform delle clip audio in timeline,
    /// leggendo il file di cache una volta sola per chiave.
    fn ensure_waveforms_loaded(&mut self) {
        if let Some(worker) = &self.waveform_worker {
            let arrived = worker.drain_ready();
            for key in &arrived {
                self.waveform_missing.remove(key);
            }
            // Le compound composte con dei sorgenti ancora mancanti vanno
            // rifatte ora che ne è arrivato qualcuno.
            if !arrived.is_empty() {
                for key in self.waveform_partial.drain() {
                    self.waveform_cache.remove(&key);
                }
            }
        }
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let mut compounds: Vec<vv_core::MediaId> = Vec::new();
        for (_, track) in self.project.timelines[timeline_id].tracks_of_kind(TrackKind::Audio) {
            for clip in &track.clips {
                let vv_core::ClipSource::Media(media_id) = &clip.source else {
                    continue;
                };
                let Some(item) = self.project.media_pool.get(*media_id) else {
                    continue;
                };
                let key = (item.content_hash, clip.audio_stream_index);
                if !item.meta.has_audio
                    || self.waveform_cache.contains_key(&key)
                    || self.waveform_missing.contains(&key)
                {
                    continue;
                }
                if item.compound.is_some() {
                    compounds.push(*media_id);
                    continue;
                }
                match vv_media::waveform::load_waveform(key.0, key.1) {
                    Some(waveform) => {
                        self.waveform_cache.insert(key, waveform);
                    }
                    None => {
                        self.waveform_missing.insert(key);
                    }
                }
            }
        }
        for media_id in compounds {
            self.ensure_compound_waveform(media_id, 0);
        }
    }

    /// Waveform di una compound clip: non c'è un file da decodificare, si
    /// compone da quelle delle clip della sua timeline annidata (vedi
    /// `compose_compound_waveform`), caricandole prima se serve.
    fn ensure_compound_waveform(&mut self, media_id: vv_core::MediaId, depth: usize) {
        if depth > MAX_COMPOUND_WALK_DEPTH {
            return;
        }
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let (Some(nested_id), key) = (item.compound, (item.content_hash, 0)) else {
            return;
        };
        if self.waveform_cache.contains_key(&key) {
            return;
        }
        let sources: Vec<(vv_core::MediaId, usize)> = self.project.timelines[nested_id]
            .tracks_of_kind(TrackKind::Audio)
            .flat_map(|(_, track)| track.clips.iter())
            .filter_map(|clip| match clip.source {
                vv_core::ClipSource::Media(id) => Some((id, clip.audio_stream_index)),
                _ => None,
            })
            .collect();
        for (source_id, stream) in sources {
            let Some(source) = self.project.media_pool.get(source_id) else {
                continue;
            };
            if source.compound.is_some() {
                self.ensure_compound_waveform(source_id, depth + 1);
                continue;
            }
            let source_key = (source.content_hash, stream);
            if self.waveform_cache.contains_key(&source_key)
                || self.waveform_missing.contains(&source_key)
            {
                continue;
            }
            match vv_media::waveform::load_waveform(source_key.0, source_key.1) {
                Some(waveform) => {
                    self.waveform_cache.insert(source_key, waveform);
                }
                None => {
                    self.waveform_missing.insert(source_key);
                }
            }
        }
        let Some((waveform, complete)) =
            compose_compound_waveform(&self.project, &self.waveform_cache, media_id)
        else {
            return;
        };
        if !complete {
            self.waveform_partial.insert(key);
        }
        self.waveform_cache.insert(key, waveform);
    }

    fn ensure_timeline(&mut self) -> TimelineId {
        self.ensure_timeline_with(vv_core::Rational::new(25, 1), (1920, 1080), true)
    }

    /// Come `ensure_timeline`, ma una timeline nuova prende fps e risoluzione
    /// da `meta` (solo media con video).
    fn ensure_timeline_for(&mut self, meta: &vv_core::MediaMeta) -> TimelineId {
        self.ensure_timeline_with(meta.fps, (meta.width, meta.height), true)
    }

    /// Come `ensure_timeline`, ma senza track video: un drop di solo audio non
    /// deve crearne una vuota.
    fn ensure_timeline_audio_only(&mut self) -> TimelineId {
        self.ensure_timeline_with(vv_core::Rational::new(25, 1), (1920, 1080), false)
    }

    fn ensure_timeline_with(
        &mut self,
        fps: vv_core::Rational,
        resolution: (u32, u32),
        include_video_track: bool,
    ) -> TimelineId {
        if let Some(id) = self.timeline_id {
            return id;
        }
        let mut tracks = Vec::new();
        if include_video_track {
            tracks.push(Track::new(TrackKind::Video));
        }
        tracks.push(Track::new(TrackKind::Audio));
        let name: String = "Timeline 1".into();
        let id = self.project.timelines.insert(vv_core::Timeline {
            name: name.clone(),
            fps,
            resolution,
            tracks,
        });
        self.timeline_id = Some(id);
        self.spawn_render_ahead_if_needed(id);
        // La timeline del progetto è a tutti gli effetti una timeline come
        // le altre (vedi doc di `MediaItem::compound`): compare nel media
        // pool esattamente come una compound clip, trascinabile altrove.
        // `meta` iniziale qualunque, `sync_root_timeline_media` la
        // corregge subito al primo giro (chiamata da `update`).
        let media_id = self.project.media_pool.insert(vv_core::MediaItem {
            path: name.into(),
            meta: vv_core::MediaMeta {
                duration_frames: 0,
                fps,
                width: resolution.0,
                height: resolution.1,
                has_video: include_video_track,
                has_audio: true,
                sample_rate: 48_000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 0,
            compound: Some(id),
        });
        self.project.sync_compound_meta(media_id);
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

    /// Apre `nested_id` come se fosse la timeline del progetto: dal doppio
    /// click su una compound clip (`timeline_ui::show_timeline`). Tutta la
    /// UI di editing esistente resta invariata (è già parametrizzata su
    /// `self.timeline_id`), qui serve solo spostare il "quale" e impilare
    /// da dove si viene per il breadcrumb.
    pub(crate) fn enter_compound_timeline(&mut self, nested_id: TimelineId) {
        let Some(current) = self.timeline_id else {
            return;
        };
        if current == nested_id || self.timeline_stack.contains(&nested_id) {
            // Già su questo livello, o già un antenato in pila: un ciclo
            // residuo (vedi `MAX_COMPOUND_DEPTH` in render_ahead.rs) non
            // deve far crescere la pila all'infinito.
            return;
        }
        self.timeline_stack.push(current);
        self.switch_to_timeline(nested_id);
    }

    /// Torna alla timeline in posizione `index` di `timeline_stack` (0 =
    /// la radice): un segmento del breadcrumb cliccato.
    fn exit_to_timeline_stack_index(&mut self, index: usize) {
        let Some(target) = self.timeline_stack.get(index).copied() else {
            return;
        };
        self.timeline_stack.truncate(index);
        self.switch_to_timeline(target);
    }

    /// La parte comune delle due funzioni sopra: azzera lo stato che
    /// appartiene al livello lasciato (selezione, playhead, clip attiva) e
    /// sveglia subito `render_ahead` sulla nuova timeline, ignorando il
    /// generation-gate di `sync_render_ahead` (qui cambia la timeline
    /// stessa, non il suo contenuto — `sync_render_ahead` non se ne
    /// accorgerebbe da sola). La clipboard sopravvive: copiare da una
    /// timeline/compound clip e incollare in un'altra deve funzionare
    /// (`paste_clipboard_at_playhead` già conforma per fps diversi).
    fn switch_to_timeline(&mut self, timeline_id: TimelineId) {
        self.timeline_id = Some(timeline_id);
        let clipboard = std::mem::take(&mut self.timeline_state.clipboard);
        self.timeline_state = timeline_ui::TimelineState::default();
        self.timeline_state.clipboard = clipboard;
        self.active_clip = None;
        if let Some(render_ahead) = &self.render_ahead {
            render_ahead.update_project(&self.project, timeline_id);
            self.render_ahead_generation = self.history.generation();
        }
    }

    /// Nome da mostrare per `id` nel breadcrumb sopra la timeline: quello
    /// della sua voce nel media pool (vedi `MediaItem::compound`), che è
    /// quello che l'utente vede altrove (es. "Compound Clip 2").
    fn timeline_display_name(&self, id: TimelineId) -> String {
        self.project
            .media_pool
            .values()
            .find(|m| m.compound == Some(id))
            .map(|m| file_label(&m.path))
            .unwrap_or_else(|| self.project.timelines[id].name.clone())
    }

    /// Manda a `render_ahead` una copia del progetto solo quando la history è
    /// cambiata.
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

    /// La voce nel media pool della timeline del progetto (vedi
    /// `ensure_timeline_with`) riflette sempre il suo contenuto vero, non
    /// solo quello al momento della creazione: durata, presenza di
    /// video/audio possono cambiare a ogni modifica.
    fn sync_root_timeline_media(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let generation = self.history.generation();
        if generation == self.root_timeline_media_generation {
            return;
        }
        self.root_timeline_media_generation = generation;
        let Some(media_id) = self
            .project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound == Some(timeline_id))
            .map(|(id, _)| id)
        else {
            return;
        };
        self.project.sync_compound_meta(media_id);
    }

    /// `lookahead_secs` scalato per la velocità di riproduzione, entro quanto
    /// sta nel budget di cache (tolto `behind_secs`), mai sotto la base.
    fn effective_lookahead_secs(&self, timeline_id: TimelineId) -> f64 {
        let scaled = self.lookahead_secs * self.playback_speed;
        let timeline = &self.project.timelines[timeline_id];
        let fps = timeline.fps.as_f64().max(1e-9);
        let (w, h) = timeline.resolution;
        let frame_bytes = vv_media::yuv420_frame_bytes(w, h).max(1);
        let max_frames = self.cache_budget_bytes / frame_bytes;
        let max_secs = (max_frames as f64 / fps - self.behind_secs).max(self.lookahead_secs);
        scaled.min(max_secs)
    }

    /// Drop di un effetto dal pannello Effects: la clip generatore va sulla
    /// track video indicata da `target`, a `start`.
    fn add_generator_to_timeline_at(
        &mut self,
        generator: timeline_ui::Generator,
        start: FrameIdx,
        target: timeline_ui::MediaDropTarget,
    ) {
        let timeline_id = self.ensure_timeline();
        let group = self.history.begin_group();
        if let Some(tracks) = self.resolve_drop_tracks(timeline_id, target, true, false) {
            // Un generatore ha sempre video: `resolve_drop_tracks` con
            // `any_video: true` risolve sempre `Some`.
            let video_track = tracks.video.expect("generatore: track video sempre risolta");
            match generator {
                timeline_ui::Generator::SolidColor => {
                    self.insert_solid_color_clip(timeline_id, video_track, start)
                }
                timeline_ui::Generator::Text => {
                    self.insert_text_clip(timeline_id, video_track, start)
                }
            }
        }
        self.history.end_group_as(group, vv_core::CommandLabel::InsertClips);
    }

    fn add_drop_to_timeline_at(
        &mut self,
        drag: &timeline_ui::TimelineDrag,
        start: FrameIdx,
        target: timeline_ui::MediaDropTarget,
    ) {
        match drag {
            timeline_ui::TimelineDrag::Media(set) => {
                self.add_media_set_to_timeline_at(set, start, target)
            }
            timeline_ui::TimelineDrag::Generator(g) => {
                self.add_generator_to_timeline_at(*g, start, target)
            }
        }
    }

    /// Il colore iniziale è grigio medio, modificabile subito dal pannello
    /// proprietà una volta selezionata.
    fn insert_solid_color_clip(&mut self, timeline_id: TimelineId, track_index: usize, start: FrameIdx) {
        let default_len =
            timeline_ui::Generator::SolidColor.default_len(self.project.timelines[timeline_id].fps);

        let effects = vv_core::EffectStack {
            color: Some(vv_core::Keyframed::constant(DEFAULT_SOLID_COLOR)),
            ..Default::default()
        };

        let mut clip = vv_core::Clip::from_source_range(
            self.project.alloc_clip_id(),
            vv_core::ClipSource::SolidColor,
            0,
            default_len,
            start,
            vv_core::Rational::one(),
        );
        clip.effects = effects;
        self.insert_clips_overwriting(timeline_id, vec![(track_index, clip, None)], vv_core::CommandLabel::InsertClips);
    }

    fn insert_text_clip(&mut self, timeline_id: TimelineId, track_index: usize, start: FrameIdx) {
        let len = timeline_ui::Generator::Text.default_len(self.project.timelines[timeline_id].fps);
        let mut clip = vv_core::Clip::from_source_range(
            self.project.alloc_clip_id(),
            vv_core::ClipSource::Text,
            0,
            len,
            start,
            vv_core::Rational::one(),
        );
        clip.effects.title = Some(vv_core::TitleParams::default());
        self.insert_clips_overwriting(timeline_id, vec![(track_index, clip, None)], vv_core::CommandLabel::InsertClips);
    }

    /// Come un vero NLE, le clip già presenti sotto a quelle nuove vengono
    /// accorciate, divise o rimosse invece di restare sovrapposte.
    fn insert_clips_overwriting(
        &mut self,
        timeline_id: TimelineId,
        clips: Vec<(usize, vv_core::Clip, Option<u64>)>,
        label: vv_core::CommandLabel,
    ) {
        let commands = vv_core::insert_overwriting(&mut self.project, timeline_id, clips);
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(label, commands)),
        );
    }

    /// Compone `layers` e mostra la texture risultante nel viewer, senza
    /// readback. Non fa nulla senza device condiviso (test).
    fn show_composited(&mut self, layers: &[vv_render::Layer], output: vv_render::OutputFrame) {
        let Some(render_state) = self.egui_render_state.clone() else {
            return;
        };
        let texture = self.compositor.render_layers_to_texture(layers, output);
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
            Some(egui::vec2(output.width as f32, output.height as f32));
        self.last_viewer_frame_kind = Some(ViewerFrameKind::Video);
    }

    /// I layer da comporre al playhead, dal basso verso l'alto. `None` se il
    /// frame media in cima non è pronto: si tiene quello mostrato; un layer
    /// sotto non pronto si salta. Durante una crossing transition "in cima"
    /// conta se manca anche una sola delle due metà.
    fn timeline_video_layers(&mut self) -> Option<Vec<frame_provider::OwnedLayer>> {
        let timeline = &self.project.timelines[self.timeline_id?];
        let render_ahead = self.render_ahead.as_mut()?;
        let playhead = self.timeline_state.playhead;
        let clips = timeline.active_video_clips_at(playhead);
        let topmost = clips.len().saturating_sub(1);
        let mut layers = Vec::with_capacity(clips.len());
        for (i, &(track_index, clip)) in clips.iter().enumerate() {
            let frame = playhead.max(clip.timeline_start);
            let involved = match timeline.tracks[track_index].crossing_at(frame) {
                Some((left, right, _)) if left.id == clip.id || right.id == clip.id => 2,
                _ => 1,
            };
            let mut provider = frame_provider::GpuCompounds::new(render_ahead, &self.compositor);
            let track_layers = frame_provider::track_layers_at(
                &self.project,
                timeline,
                track_index,
                clip,
                frame,
                timeline.resolution,
                &mut provider,
            )
            .ok()?;
            if i == topmost && track_layers.len() < involved {
                return None;
            }
            layers.extend(track_layers);
        }
        Some(layers)
    }

    #[cfg(test)]
    fn active_clip_effects(&self) -> Option<&vv_core::EffectStack> {
        let (track_index, clip_id) = self.active_clip?;
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .clip(track_index, clip_id)
            .map(|c| &c.effects)
    }

    /// Il frame dell'anteprima del media pool alla sua testina, se già
    /// decodificato.
    fn browsing_video_frame(&mut self) -> Option<std::sync::Arc<vv_media::FrameYuv420>> {
        let (render_ahead, media_id) = self.browsing_render_ahead.as_ref()?;
        render_ahead.set_target(self.browse_playhead);
        render_ahead.get_frame(*media_id, self.browse_playhead)
    }

    /// Aggiunge il media in coda alla track video (e le clip audio accanto).
    #[cfg(test)]
    fn add_media_to_timeline(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let meta = item.meta.clone();
        // Se non esiste ancora una timeline (es. primo drag&drop dal media
        // pool), viene creata al volo ereditando fps/risoluzione da questo
        // media.
        let timeline_id = self.ensure_timeline_for(&meta);
        if self.project.would_create_a_cycle(media_id, timeline_id) {
            return;
        }
        let video_track = self.project.timelines[timeline_id]
            .first_track_index(TrackKind::Video)
            .unwrap_or(0);
        let video_start = track_end(&self.project, timeline_id, video_track);
        self.insert_media_clip(
            timeline_id,
            timeline_ui::MediaDrag::whole(media_id, &meta),
            &meta,
            video_start,
            DropTracks {
                video: Some(video_track),
                extra_audio: None,
            },
        );
    }

    /// Come `add_media_to_timeline`, ma piazza la clip a `start` (posizione
    /// e `target` dal drag&drop sulla timeline, vedi `MediaDropTarget`).
    #[cfg(test)]
    fn add_media_to_timeline_at(
        &mut self,
        drag: timeline_ui::MediaDrag,
        start: FrameIdx,
        target: timeline_ui::MediaDropTarget,
    ) {
        self.add_media_set_to_timeline_at(
            &timeline_ui::MediaDragSet::one(drag),
            start,
            target,
        );
    }

    /// Drop di uno o più media: accodati da `start` nell'ordine del pool. Le
    /// track nuove si creano una volta per tutto il drop.
    fn add_media_set_to_timeline_at(
        &mut self,
        set: &timeline_ui::MediaDragSet,
        start: FrameIdx,
        target: timeline_ui::MediaDropTarget,
    ) {
        let drops: Vec<(timeline_ui::MediaDrag, vv_core::MediaMeta)> = set
            .items
            .iter()
            .filter(|d| d.source_len() > 0)
            .filter_map(|d| {
                let item = self.project.media_pool.get(d.media_id)?;
                Some((*d, item.meta.clone()))
            })
            .collect();
        if drops.is_empty() {
            return;
        }
        // fps e risoluzione dal primo media *video* del set: un audio in testa
        // non ha una risoluzione da dare alla timeline.
        let timeline_id = match drops.iter().find(|(d, meta)| d.takes_video(meta)) {
            Some((_, video_meta)) => self.ensure_timeline_for(video_meta),
            None => self.ensure_timeline_audio_only(),
        };
        // Un drop che chiuderebbe un ciclo (una timeline importata dentro
        // se stessa, direttamente o attraverso una sua compound clip) si
        // scarta invece di corrompere il progetto — vedi
        // `Project::would_create_a_cycle`.
        let drops: Vec<(timeline_ui::MediaDrag, vv_core::MediaMeta)> = drops
            .into_iter()
            .filter(|(d, _)| !self.project.would_create_a_cycle(d.media_id, timeline_id))
            .collect();
        if drops.is_empty() {
            return;
        }
        let any_video = drops.iter().any(|(d, meta)| d.takes_video(meta));
        let any_audio = drops.iter().any(|(d, meta)| d.takes_audio(meta));
        // Un drop solo = un solo Ctrl+Z, anche se dentro sono N clip (una
        // per stream audio di ogni media) più le track create al volo.
        let group = self.history.begin_group();
        let Some(tracks) = self.resolve_drop_tracks(timeline_id, target, any_video, any_audio) else {
            self.history.end_group(group);
            return;
        };

        let timeline_fps = self.project.timelines[timeline_id].fps;
        let mut cursor = start;
        for (drag, meta) in &drops {
            self.insert_media_clip(timeline_id, *drag, meta, cursor, tracks);
            let rate = vv_core::Rational::conform_rate(timeline_fps, meta.fps);
            cursor += drag.timeline_len(rate);
        }
        self.history.end_group_as(group, vv_core::CommandLabel::InsertClips);
    }

    /// Crea le track richieste da `target` (una volta sola per drop) e
    /// restituisce dove andranno le clip.
    fn resolve_drop_tracks(
        &mut self,
        timeline_id: TimelineId,
        target: timeline_ui::MediaDropTarget,
        any_video: bool,
        any_audio: bool,
    ) -> Option<DropTracks> {
        let video = if !any_video {
            None
        } else {
            Some(match target {
                timeline_ui::MediaDropTarget::NewVideoTrack => {
                    timeline_ui::add_track(&mut self.project, &mut self.history, timeline_id, TrackKind::Video)
                }
                timeline_ui::MediaDropTarget::Track(track) => {
                    if self.project.timelines[timeline_id].is_locked(track) {
                        return None;
                    }
                    track
                }
                _ => match self.project.timelines[timeline_id].first_unlocked_track_index(TrackKind::Video) {
                    Some(track) => track,
                    // Nessuna track video libera: se ne crea una.
                    None => {
                        timeline_ui::add_track(&mut self.project, &mut self.history, timeline_id, TrackKind::Video)
                    }
                },
            })
        };
        let extra_audio = if target == timeline_ui::MediaDropTarget::NewAudioTrack && any_audio {
            Some(timeline_ui::add_track(&mut self.project, &mut self.history, timeline_id, TrackKind::Audio))
        } else {
            None
        };
        Some(DropTracks {
            video,
            extra_audio,
        })
    }

    /// Inserisce la clip video e una clip audio per stream a `start`, tutte
    /// nello stesso gruppo collegato; crea le track audio che mancano.
    fn insert_media_clip(
        &mut self,
        timeline_id: TimelineId,
        drag: timeline_ui::MediaDrag,
        meta: &vv_core::MediaMeta,
        start: FrameIdx,
        tracks: DropTracks,
    ) {
        let media_id = drag.media_id;
        let rate = vv_core::Rational::conform_rate(
            self.project.timelines[timeline_id].fps,
            meta.fps,
        );
        let has_audio_tracks = self.project.timelines[timeline_id]
            .first_track_index(TrackKind::Audio)
            .is_some();
        let mut audio_track_indices: Vec<usize> = self.project.timelines[timeline_id]
            .tracks_of_kind(TrackKind::Audio)
            .filter(|(_, t)| !t.locked)
            .map(|(i, _)| i)
            .collect();

        // In testa: il primo stream deve atterrare sulla track appena
        // creata per questo drop, non su una già esistente.
        if let Some(extra) = tracks.extra_audio {
            audio_track_indices.retain(|i| *i != extra);
            audio_track_indices.insert(0, extra);
        }

        // Senza track audio l'audio di un video si scarta, ma un media o un
        // drag solo audio se ne crea una. Se sono tutte bloccate se ne creano di nuove.
        let takes_video = drag.takes_video(meta);
        let num_audio_streams = if !drag.takes_audio(meta) {
            0
        } else if has_audio_tracks || !takes_video {
            meta.audio_stream_count()
        } else {
            0
        };

        while audio_track_indices.len() < num_audio_streams {
            audio_track_indices.push(timeline_ui::add_track(&mut self.project, &mut self.history, timeline_id, TrackKind::Audio));
        }

        let audio_clip_ids: Vec<ClipId> = (0..num_audio_streams)
            .map(|_| self.project.alloc_clip_id())
            .collect();

        let mut new_clips: Vec<(usize, vv_core::Clip, Option<u64>)> = Vec::new();
        if takes_video {
            let video_clip = vv_core::Clip::from_source_range(
                self.project.alloc_clip_id(),
                vv_core::ClipSource::Media(media_id),
                drag.source_in,
                drag.source_out,
                start,
                rate,
            );
            // `tracks.video` è `Some` di sicuro: `resolve_drop_tracks` lo
            // risolve solo se almeno un media del drop ha video, e questo
            // è uno di quelli.
            new_clips.push((
                tracks.video.expect("drop con video ma nessuna track risolta"),
                video_clip,
                Some(0),
            ));
        }
        for (stream_index, (&track_index, &clip_id)) in
            audio_track_indices.iter().zip(audio_clip_ids.iter()).enumerate()
        {
            let mut audio_clip = vv_core::Clip::from_source_range(
                clip_id,
                vv_core::ClipSource::Media(media_id),
                drag.source_in,
                drag.source_out,
                start,
                rate,
            );
            audio_clip.audio_stream_index = stream_index;
            new_clips.push((track_index, audio_clip, Some(0)));
        }
        self.insert_clips_overwriting(timeline_id, new_clips, vv_core::CommandLabel::InsertClips);
    }

    /// Handle di transform della prima clip video selezionata, se è sotto
    /// alla testina e il viewer mostra la timeline ferma.
    fn show_settings_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.settings_dialog else {
            return;
        };
        let response = dialog.show(ctx, &mut self.settings);
        if !response.open {
            self.settings_dialog = None;
        }
        if response.changed {
            self.persist_settings();
        }
    }

    /// Impostazioni utente su disco, incluso il layout dei pannelli: chiamata
    /// dalla finestra Impostazioni e periodicamente/alla chiusura (vedi
    /// `eframe::App::save`).
    fn persist_settings(&mut self) {
        if let Some(path) = &self.settings_path
            && let Err(e) = self.settings.save(path)
        {
            self.project_error = Some(t!("settings.save_failed", error = e).into_owned());
        }
    }

    /// `(total, playhead, (in, out), playing)` della barra di riproduzione.
    fn transport_state(&self) -> (FrameIdx, FrameIdx, (FrameIdx, FrameIdx), bool) {
        let playing = self.is_timeline_playing();
        if self.browsing_media.is_some() {
            let total = self.browse_total_frames();
            (total, self.browse_playhead, self.browse_marks.resolve(total), playing)
        } else {
            let total = self
                .timeline_id
                .map_or(0, |id| self.project.timelines[id].total_frames());
            (
                total,
                self.timeline_state.playhead,
                self.timeline_state.export_marks.resolve(total),
                playing,
            )
        }
    }

    /// Player a tutto schermo sopra al resto dell'interfaccia, che resta
    /// disegnata sotto (e continua a gestire le scorciatoie) ma non riceve
    /// più il mouse. La barra di riproduzione compare solo col mouse in basso.
    fn show_fullscreen_viewer(&mut self, ctx: &egui::Context) -> transport::TransportResponse {
        const BAR_ZONE: f32 = 90.0;
        let screen = ctx.content_rect();
        let (total, playhead, marks, playing) = self.transport_state();
        let mut action = transport::TransportResponse::default();
        egui::Area::new(egui::Id::new("fullscreen_viewer"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.set_min_size(screen.size());
                ui.interact(screen, ui.id().with("block"), egui::Sense::click_and_drag());
                ui.painter().rect_filled(screen, 0.0, egui::Color32::BLACK);

                let image = match self.last_viewer_frame_kind {
                    Some(ViewerFrameKind::Video) => self
                        .video_texture_id
                        .zip(self.video_display_size),
                    Some(ViewerFrameKind::Offline) | None => None,
                };
                if let Some((id, size)) = image {
                    let scale = (screen.width() / size.x).min(screen.height() / size.y);
                    let rect = egui::Rect::from_center_size(screen.center(), size * scale.max(0.0));
                    ui.painter().image(
                        id,
                        rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                } else if self.last_viewer_frame_kind == Some(ViewerFrameKind::Offline) {
                    ui.painter().text(
                        screen.center(),
                        egui::Align2::CENTER_CENTER,
                        t!("viewer.media_offline"),
                        egui::FontId::proportional(24.0),
                        egui::Color32::from_rgb(230, 70, 70),
                    );
                }

                let hovering_bar = ui
                    .input(|i| i.pointer.hover_pos())
                    .is_some_and(|p| p.y >= screen.bottom() - BAR_ZONE);
                if hovering_bar {
                    let bar = egui::Rect::from_min_max(
                        egui::pos2(screen.left(), screen.bottom() - BAR_ZONE),
                        screen.max,
                    );
                    ui.painter().rect_filled(bar, 0.0, egui::Color32::from_black_alpha(170));
                    let inner = bar.shrink2(egui::vec2(24.0, 24.0));
                    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
                        action = transport::show_transport(ui, total, playhead, marks, playing);
                    });
                }
            });
        action
    }

    fn show_viewer_overlay(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        area: egui::Rect,
        video_targets: &[PanelTarget],
        pending: &mut Vec<BoxedCommand>,
    ) {
        let visible = self.show_transform_overlay
            && self.browsing_media.is_none()
            && !self.is_timeline_playing()
            && matches!(
                self.last_viewer_frame_kind,
                Some(ViewerFrameKind::Video)
            );
        let Some((timeline_id, target)) = self.timeline_id.zip(video_targets.first().copied())
        else {
            self.overlay_drag = None;
            return;
        };
        let playhead = self.timeline_state.playhead;
        let under_playhead = self.project.timelines[timeline_id]
            .clip(target.track_index, target.clip_id)
            .is_some_and(|c| c.contains(playhead));
        let Some(info) = self.clip_panel_info(target).filter(|_| visible && under_playhead) else {
            self.overlay_drag = None;
            return;
        };
        let Some(new) = viewer_overlay::show(
            ui,
            rect,
            area,
            info.timeline_size,
            info.source_size,
            &info.transform,
            &mut self.overlay_drag,
        ) else {
            return;
        };
        push_param_changes(
            pending,
            Some(&self.project.timelines[timeline_id]),
            video_targets,
            &vv_core::TransformParam::ALL,
            &new,
            &info.transform,
        );
    }

    /// D: disattiva le clip selezionate, o le riattiva se lo sono già tutte.
    fn toggle_disabled_selected(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let tl = &self.project.timelines[timeline_id];
        let selected: Vec<(usize, ClipId)> = self
            .timeline_state
            .selected
            .iter()
            .copied()
            .filter(|&(track_index, _)| !tl.is_locked(track_index))
            .collect();
        if selected.is_empty() {
            return;
        }
        let all_disabled = selected.iter().all(|&(track_index, clip_id)| {
            tl.clip(track_index, clip_id)
                .is_some_and(|c| c.disabled)
        });
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::SetClipsDisabled::new(timeline_id, selected, !all_disabled)),
        );
    }

    /// Ctrl+A: seleziona tutte le clip della timeline.
    fn select_all_clips(&mut self) {
        self.select_clips(|_| true);
    }

    /// Ctrl+A col media pool a fuoco: seleziona tutti i media del pool.
    fn select_all_media(&mut self) {
        let ids: Vec<MediaId> = self.project.media_pool.keys().collect();
        self.media_pool_state.set_marquee_selection(ids);
    }

    /// Alt+Y: seleziona dalla testina in avanti — la clip sotto alla
    /// testina è inclusa, quelle che finiscono prima restano fuori.
    fn select_clips_from_playhead(&mut self) {
        let playhead = self.timeline_state.playhead;
        self.select_clips(|clip| clip.timeline_end() > playhead);
    }

    /// Seleziona le clip delle track sbloccate per cui `keep` è vero.
    fn select_clips(&mut self, keep: impl Fn(&vv_core::Clip) -> bool) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let selected: BTreeSet<(usize, ClipId)> = self.project.timelines[timeline_id]
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| !track.locked)
            .flat_map(|(track_index, track)| {
                track
                    .clips
                    .iter()
                    .filter(|clip| keep(clip))
                    .map(move |clip| (track_index, clip.id))
            })
            .collect();
        let anchor = selected.iter().next().copied();
        self.timeline_state.set_selection(selected, anchor);
    }

    fn delete_selected(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if let Some(sel) = self.timeline_state.selected_transition {
            let cmd = set_transition_command(&self.project, timeline_id, sel, None);
            self.history.do_command(&mut self.project, cmd);
            self.timeline_state.selected_transition = None;
            return;
        }
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
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::DeleteClips, commands)),
        );
        self.timeline_state.clear_selection();
        self.sync_selection_to_playhead();
    }

    /// Canc/Backspace sul media pool: toglie i media selezionati. Le clip
    /// che li usano restano in timeline e diventano offline (vedi
    /// `vv_core::RemoveMedia`).
    fn delete_selected_media(&mut self) {
        if self.media_pool_state.selected.is_empty() {
            return;
        }
        let commands: Vec<Box<dyn vv_core::Command>> = self
            .media_pool_state
            .selected
            .iter()
            .copied()
            .map(|id| Box::new(vv_core::RemoveMedia::new(id)) as Box<dyn vv_core::Command>)
            .collect();
        if self
            .browsing_media
            .is_some_and(|id| self.media_pool_state.selected.contains(&id))
        {
            self.stop_browsing();
        }
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::RemoveMedia, commands)),
        );
        self.media_pool_state.clear();
        self.leave_removed_timelines();
    }

    /// Cancellare una compound clip ne cancella la timeline annidata
    /// (`vv_core::RemoveMedia`): se la si stava editando si risale al
    /// livello superiore ancora esistente.
    fn leave_removed_timelines(&mut self) {
        while self
            .timeline_id
            .is_some_and(|id| !self.project.timelines.contains_key(id))
        {
            match self.timeline_stack.pop() {
                Some(parent) => self.switch_to_timeline(parent),
                None => {
                    self.timeline_id = None;
                    self.timeline_state = timeline_ui::TimelineState::default();
                    self.active_clip = None;
                    return;
                }
            }
        }
    }

    /// La clip attiva punta a un media non piu' nel media pool.
    fn active_clip_media_offline(&self) -> bool {
        if self.browsing_media.is_some() {
            return false;
        }
        let (Some(timeline_id), Some((track_index, clip_id))) = (self.timeline_id, self.active_clip)
        else {
            return false;
        };
        self.project.timelines[timeline_id]
            .clip(track_index, clip_id)
            .is_some_and(|c| match &c.source {
                vv_core::ClipSource::Media(id) => !self.project.media_pool.contains_key(*id),
                vv_core::ClipSource::SolidColor | vv_core::ClipSource::Text => false,
            })
    }

    /// Copia/taglia/incolla da tastiera. Dopo una copia scrive un segnaposto
    /// nella clipboard di sistema: egui-winit genera `Event::Paste` solo se
    /// quella non è vuota.
    fn handle_clipboard_events(&mut self, ui: &egui::Ui, events: &[egui::Event]) {
        for event in events {
            match event {
                egui::Event::Copy => {
                    self.copy_selected_clips();
                    if !self.timeline_state.clipboard.is_empty() {
                        ui.ctx().copy_text("vibevideo:clip".to_owned());
                    }
                }
                // Ctrl+X in un campo di testo taglia il testo, non le clip.
                egui::Event::Cut if !ui.ctx().egui_wants_keyboard_input() => {
                    if self.timeline_state.selected.is_empty() {
                        continue;
                    }
                    self.copy_selected_clips();
                    ui.ctx().copy_text("vibevideo:clip".to_owned());
                    self.delete_selected();
                }
                egui::Event::Paste(_) => self.paste_clipboard_at_playhead(),
                _ => {}
            }
        }
    }

    /// Copia le clip selezionate (i gruppi collegati sono già tutti dentro la
    /// selezione).
    fn copy_selected_clips(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            return;
        }

        let tl = &self.project.timelines[timeline_id];
        // I gruppi si rimappano su tag locali: al paste servono gruppi nuovi.
        let mut collected: Vec<(Option<vv_core::LinkGroupId>, timeline_ui::ClipboardEntry)> = self
            .timeline_state
            .selected
            .iter()
            .filter_map(|&(track_index, clip_id)| {
                let clip = tl
                    .clip(track_index, clip_id)?;
                Some((
                    clip.linked_group,
                    timeline_ui::ClipboardEntry {
                        track_kind: tl.tracks[track_index].kind,
                        track_number: tl.track_number(track_index),
                        relative_start: clip.timeline_start,
                        clip: clip.clone(),
                        timeline_fps: tl.fps,
                        link_tag: None,
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

    /// Incolla la clipboard al playhead mantenendo le distanze e i gruppi
    /// collegati; le clip incollate sovrascrivono quel che c'era sotto.
    fn paste_clipboard_at_playhead(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.clipboard.is_empty() {
            return;
        }
        let playhead = self.timeline_state.playhead;
        let entries: Vec<timeline_ui::ClipboardEntry> = self
            .timeline_state
            .clipboard
            .iter()
            // Incollare una compound clip copiata dentro la sua stessa
            // timeline annidata (direttamente o attraverso un'altra
            // compound clip) chiuderebbe un ciclo — vedi
            // `Project::would_create_a_cycle`, stesso controllo del drop
            // dal media pool.
            .filter(|e| match &e.clip.source {
                vv_core::ClipSource::Media(media_id) => {
                    !self.project.would_create_a_cycle(*media_id, timeline_id)
                }
                vv_core::ClipSource::SolidColor | vv_core::ClipSource::Text => true,
            })
            .cloned()
            .collect();
        if entries.is_empty() {
            return;
        }

        let mark = self.history.begin_group();
        self.add_tracks_for_clipboard(timeline_id, &entries);

        let tl = &self.project.timelines[timeline_id];
        let entries: Vec<(usize, timeline_ui::ClipboardEntry)> = entries
            .into_iter()
            .filter_map(|e| {
                let index = tl.track_of_kind_numbered(e.track_kind, e.track_number)?;
                (!tl.is_locked(index)).then_some((index, e))
            })
            .collect();
        if entries.is_empty() {
            self.history.end_group(mark);
            return;
        }

        let timeline_fps = self.project.timelines[timeline_id].fps;
        let clips: Vec<(usize, vv_core::Clip, Option<u64>)> = entries
            .iter()
            .map(|(track_index, entry)| {
                let mut clip = entry.clip.clone();
                clip.id = self.project.alloc_clip_id();
                clip.timeline_start = entry.relative_start;
                clip.linked_group = None;
                if entry.timeline_fps != timeline_fps {
                    let rate = match &clip.source {
                        vv_core::ClipSource::Media(media_id) => {
                            self.project.media_pool.get(*media_id).map_or(clip.rate, |item| {
                                vv_core::Rational::conform_rate(timeline_fps, item.meta.fps)
                            })
                        }
                        vv_core::ClipSource::SolidColor | vv_core::ClipSource::Text => clip.rate,
                    };
                    clip.retime(entry.timeline_fps, timeline_fps, rate);
                }
                clip.timeline_start += playhead;
                (*track_index, clip, entry.link_tag)
            })
            .collect();
        let new_selection: BTreeSet<(usize, ClipId)> =
            clips.iter().map(|(track, clip, _)| (*track, clip.id)).collect();
        let end = clips.iter().map(|(_, clip, _)| clip.timeline_end()).max();
        self.insert_clips_overwriting(timeline_id, clips, vv_core::CommandLabel::PasteClips);
        self.history.end_group_as(mark, vv_core::CommandLabel::PasteClips);
        let anchor = new_selection.iter().next().copied();
        self.timeline_state.set_selection(new_selection, anchor);
        if let Some(end) = end {
            self.timeline_state.playhead = end;
            self.ensure_active_clip_matches_playhead(true);
        }
    }

    /// Track mancanti per incollare `entries`: una compound clip di sole
    /// track video incollata in una timeline che ne ha una sola deve
    /// crearsi la V2, non finire sull'audio.
    fn add_tracks_for_clipboard(
        &mut self,
        timeline_id: TimelineId,
        entries: &[timeline_ui::ClipboardEntry],
    ) {
        for kind in [TrackKind::Video, TrackKind::Audio] {
            let wanted = entries
                .iter()
                .filter(|e| e.track_kind == kind)
                .map(|e| e.track_number)
                .max()
                .unwrap_or(0);
            let existing = self.project.timelines[timeline_id].tracks_of_kind(kind).count();
            for _ in existing..wanted {
                self.history
                    .do_command(&mut self.project, Box::new(vv_core::AddTrack::new(timeline_id, kind)));
            }
        }
    }

    /// Ripple delete: rimuove le clip selezionate (e i loro gruppi
    /// collegati) e chiude i buchi su tutte le track, mantenendo il sync A/V.
    fn ripple_delete_selected(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            // Nessuna clip selezionata: si chiude il vuoto selezionato, se c'è.
            if let Some((_, gap_start, gap_end)) = self.timeline_state.selected_gap {
                let mark = self.history.begin_group();
                self.history.do_command(
                    &mut self.project,
                    Box::new(vv_core::RippleDeleteGap::new(
                        timeline_id,
                        gap_start,
                        gap_end - gap_start,
                    )),
                );
                self.cut_remaining_overlaps(timeline_id);
                self.history.end_group(mark);
                self.move_playhead_to_closed_gap(timeline_id, gap_start);
                self.timeline_state.clear_selection();
                self.sync_selection_to_playhead();
            }
            return;
        }
        let selected: Vec<(usize, ClipId)> = self.timeline_state.selected.iter().copied().collect();

        let mut processed: BTreeSet<(usize, ClipId)> = BTreeSet::new();
        let mut removed: Vec<(usize, ClipId, FrameIdx, FrameIdx)> = Vec::new();
        for &(track_index, clip_id) in &selected {
            for (member_track, member_id) in std::iter::once((track_index, clip_id))
                .chain(self.project.timelines[timeline_id].linked_members(track_index, clip_id))
            {
                if self.project.timelines[timeline_id].is_locked(member_track)
                    || !processed.insert((member_track, member_id))
                {
                    continue;
                }
                let Some(clip) = self.project.timelines[timeline_id]
                    .clip(member_track, member_id)
                else {
                    continue;
                };
                removed.push((
                    member_track,
                    member_id,
                    clip.timeline_start,
                    clip.timeline_end(),
                ));
            }
        }

        // Buchi uniti quando si sovrappongono: chiuderli una volta per clip
        // farebbe arretrare il resto del doppio.
        let mut gaps: Vec<(FrameIdx, FrameIdx)> = removed
            .iter()
            .map(|&(_, _, start, end)| (start, end))
            .collect();
        gaps.sort();
        let mut merged: Vec<(FrameIdx, FrameIdx)> = Vec::new();
        for (start, end) in gaps {
            match merged.last_mut() {
                Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
                _ => merged.push((start, end)),
            }
        }
        let leftmost_removed = merged.first().map(|&(start, _)| start);

        let mut commands: Vec<Box<dyn vv_core::Command>> = removed
            .iter()
            .map(|&(track_index, clip_id, _, _)| {
                Box::new(vv_core::LiftDelete::new(timeline_id, track_index, clip_id))
                    as Box<dyn vv_core::Command>
            })
            .collect();
        // Da destra a sinistra: chiudere un buco sposta quel che gli sta
        // dopo, non quel che gli sta prima, quindi i buchi ancora da
        // chiudere restano dove li abbiamo misurati.
        for &(start, end) in merged.iter().rev() {
            commands.push(Box::new(vv_core::RippleDeleteGap::new(
                timeline_id,
                start,
                end - start,
            )));
        }

        let mark = self.history.begin_group();
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::RippleDelete, commands)),
        );
        self.cut_remaining_overlaps(timeline_id);
        self.history.end_group(mark);
        if let Some(position) = leftmost_removed {
            self.move_playhead_to_closed_gap(timeline_id, position);
        }
        self.timeline_state.clear_selection();
        self.sync_selection_to_playhead();
    }

    /// Dopo uno spostamento in blocco, taglia le sovrapposizioni che ne
    /// fossero rimaste (vedi `vv_core::cut_overlaps`): vince sempre la clip
    /// che comincia dopo, quella appena arrivata lì.
    fn cut_remaining_overlaps(&mut self, timeline_id: TimelineId) {
        let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
        vv_core::cut_overlaps(&mut self.project, timeline_id, &mut commands);
        if commands.is_empty() {
            return;
        }
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::TrimClips, commands)),
        );
    }

    /// Porta la testina dove ora comincia la clip scivolata a chiudere il buco;
    /// se non ne è arrivata nessuna resta dov'è.
    fn move_playhead_to_closed_gap(&mut self, timeline_id: vv_core::TimelineId, position: FrameIdx) {
        let landed = self.project.timelines[timeline_id]
            .tracks
            .iter()
            .any(|t| t.clips.iter().any(|c| c.timeline_start == position));
        if landed {
            self.timeline_state.playhead = position;
            self.ensure_active_clip_matches_playhead(true);
        }
    }

    /// Taglia al playhead le clip selezionate che lo coprono o, senza
    /// selezione, tutte. Le metà destre di un gruppo collegato vengono
    /// ricollegate tra loro.
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
            .filter(|(_, track)| !track.locked)
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
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::SplitClips, commands)),
        );

        // Seleziona la metà *sinistra* (quella sotto il playhead sarebbe la
        // destra): dopo un taglio di solito si lavora su ciò che sta prima.
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
            selected.extend(self.project.timelines[timeline_id].linked_members(*video_track, *video_clip_id));
            self.timeline_state
                .set_selection(selected, Some((*video_track, *video_clip_id)));
        }
    }
}

/// Intervalli di cache in frame sorgente → frame di timeline di `clip`,
/// limitati al suo trim.
fn map_source_ranges_to_timeline(
    clip: &vv_core::Clip,
    source_ranges: &[(FrameIdx, FrameIdx)],
) -> Vec<(FrameIdx, FrameIdx)> {
    source_ranges
        .iter()
        .filter_map(|&(s_start, s_end)| {
            let start = s_start.max(clip.source_in());
            let end = s_end.min(clip.source_out() - 1);
            (start <= end).then(|| (clip.timeline_frame_at(start), clip.timeline_frame_at(end)))
        })
        .collect()
}

/// Quanti livelli di compound clip annidate si percorrono (waveform,
/// striscia "buffered"): oltre, si rinuncia — come `MAX_COMPOUND_DEPTH`
/// del render.
const MAX_COMPOUND_WALK_DEPTH: usize = 8;

/// Picchi di una compound clip, composti da quelli delle clip audio della
/// sua timeline annidata: non c'è un file da decodificare, quindi il
/// worker non può generarli. Il `bool` è `false` se qualche sorgente non
/// era in cache — la waveform è parziale e va rifatta più tardi.
fn compose_compound_waveform(
    project: &vv_core::Project,
    cache: &HashMap<(u64, usize), vv_media::Waveform>,
    media_id: vv_core::MediaId,
) -> Option<(vv_media::Waveform, bool)> {
    let item = project.media_pool.get(media_id)?;
    let timeline = project.timelines.get(item.compound?)?;
    let fps = timeline.fps.as_f64();
    let duration_secs = item.meta.duration_frames as f64 / fps;
    if duration_secs <= 0.0 {
        return None;
    }
    let num_peaks = vv_media::recommended_num_peaks(duration_secs);
    let mut peaks = vec![0.0f32; num_peaks];
    let mut complete = true;
    for (_, track) in timeline.tracks_of_kind(TrackKind::Audio) {
        if track.muted {
            continue;
        }
        for clip in track.clips.iter().filter(|c| !c.disabled) {
            let vv_core::ClipSource::Media(source_id) = clip.source else {
                continue;
            };
            let Some(source) = project.media_pool.get(source_id) else {
                continue;
            };
            let Some(source_wf) = cache.get(&(source.content_hash, clip.audio_stream_index)) else {
                complete = false;
                continue;
            };
            if source_wf.peaks.is_empty() || source_wf.audio_duration_secs <= 0.0 {
                continue;
            }
            let bin_of = |frame: FrameIdx| {
                ((frame as f64 / fps) / duration_secs * num_peaks as f64) as usize
            };
            let first = bin_of(clip.timeline_start).min(num_peaks);
            let last = (bin_of(clip.timeline_end()) + 1).min(num_peaks);
            for bin in first..last {
                let secs = (bin as f64 + 0.5) / num_peaks as f64 * duration_secs;
                let frame = (secs * fps) as FrameIdx;
                if !clip.contains(frame) {
                    continue;
                }
                let source_secs = clip.media_secs_at(frame, fps);
                let source_bin = ((source_secs / source_wf.audio_duration_secs
                    * source_wf.peaks.len() as f64) as usize)
                    .min(source_wf.peaks.len() - 1);
                let source_frame = (source_secs * source.meta.fps.as_f64()) as FrameIdx;
                let gain = vv_audio::mixer::db_to_linear(clip.effects.gain_db.value_at(source_frame));
                peaks[bin] = peaks[bin].max(source_wf.peaks[source_bin] * gain);
            }
        }
    }
    Some((
        vv_media::Waveform {
            peaks,
            audio_duration_secs: duration_secs,
        },
        complete,
    ))
}

#[cfg(test)]
fn track_end(project: &vv_core::Project, timeline_id: TimelineId, track_index: usize) -> FrameIdx {
    project.timelines[timeline_id]
        .tracks
        .get(track_index)
        .and_then(|t| t.clips.iter().map(|c| c.timeline_end()).max())
        .unwrap_or(0)
}

/// Durata di un media per la colonna del pannello: MM:SS, con le ore solo
/// quando ci sono.
pub(crate) fn format_duration(duration_frames: vv_core::FrameIdx, fps: f64) -> String {
    let secs = (duration_frames.max(0) as f64 / fps.max(1e-9)).round() as u64;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Estensioni trattate come immagini: il probe di un container non basta a
/// distinguerle.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "bmp", "webp", "tif", "tiff"];

fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.iter().any(|img| img.eq_ignore_ascii_case(ext)))
}

/// Calamita disegnata a mano: su alcune piattaforme (Asahi) i font di egui
/// non hanno il glifo 🧲.
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

/// Riquadro con quattro handle agli angoli e il pivot al centro.
fn transform_overlay_toggle(ui: &mut egui::Ui, enabled: &mut bool) -> egui::Response {
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
        let color = visuals.fg_stroke.color;
        let frame = egui::Rect::from_center_size(rect.center(), egui::vec2(14.0, 11.0));
        painter.rect_stroke(frame, 0.0, egui::Stroke::new(1.2, color), egui::StrokeKind::Middle);
        for corner in [frame.left_top(), frame.right_top(), frame.left_bottom(), frame.right_bottom()] {
            painter.rect_filled(egui::Rect::from_center_size(corner, egui::vec2(4.0, 4.0)), 0.0, color);
        }
        painter.circle_stroke(frame.center(), 2.0, egui::Stroke::new(1.2, color));
    }
    response
}

/// Icone pellicola e waveform in basso sul viewer: trascinandole si porta in
/// timeline solo il video o solo l'audio.
fn show_stream_drag_handles(
    ui: &egui::Ui,
    viewer: egui::Rect,
) -> [(egui::Response, timeline_ui::DragStreams); 2] {
    const SIZE: egui::Vec2 = egui::vec2(30.0, 26.0);
    const GAP: f32 = 6.0;
    let center = egui::pos2(viewer.center().x, viewer.bottom() - 12.0 - SIZE.y / 2.0);
    let offset = egui::vec2((SIZE.x + GAP) / 2.0, 0.0);
    let video_rect = egui::Rect::from_center_size(center - offset, SIZE);
    let audio_rect = egui::Rect::from_center_size(center + offset, SIZE);

    let video = ui
        .interact(video_rect, ui.id().with("viewer_drag_video_only"), egui::Sense::drag())
        .on_hover_text(t!("viewer.drag_video_only"));
    let audio = ui
        .interact(audio_rect, ui.id().with("viewer_drag_audio_only"), egui::Sense::drag())
        .on_hover_text(t!("viewer.drag_audio_only"));

    let visible = ui.rect_contains_pointer(viewer) || video.dragged() || audio.dragged();
    if visible {
        let painter = ui.painter();
        for resp in [&video, &audio] {
            let alpha = if resp.hovered() || resp.dragged() { 220 } else { 150 };
            painter.rect_filled(resp.rect, 4.0, egui::Color32::from_black_alpha(alpha));
        }
        let color = egui::Color32::from_gray(230);

        let film = egui::Rect::from_center_size(video_rect.center(), egui::vec2(18.0, 14.0));
        painter.rect_stroke(film, 1.0, egui::Stroke::new(1.3, color), egui::StrokeKind::Middle);
        for i in 0..4 {
            let x = film.left() + 3.0 + i as f32 * 4.0;
            for y in [film.top() + 2.0, film.bottom() - 2.0] {
                painter.rect_filled(
                    egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(2.0, 2.0)),
                    0.0,
                    color,
                );
            }
        }

        let heights = [4.0, 9.0, 14.0, 7.0, 12.0, 5.0, 8.0];
        let c = audio_rect.center();
        for (i, h) in heights.iter().enumerate() {
            let x = c.x + (i as f32 - 3.0) * 2.8;
            painter.line_segment(
                [egui::pos2(x, c.y - h / 2.0), egui::pos2(x, c.y + h / 2.0)],
                egui::Stroke::new(1.6, color),
            );
        }
    }

    [
        (video, timeline_ui::DragStreams::VideoOnly),
        (audio, timeline_ui::DragStreams::AudioOnly),
    ]
}

/// Su Wayland winit manda `Started` per lo scroll ad alta risoluzione ma
/// quasi mai `Ended`: egui tiene Alt premuto e lo zoom resta bloccato.
/// Trattato come `Move`, ogni evento usa i modificatori correnti.
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
        #[cfg(target_os = "linux")]
        if let Some(dnd) = &self.wayland_dnd {
            dnd.feed(raw_input);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_close_request(&ui.ctx().clone());
        self.poll_pending_dialog(&ui.ctx().clone());
        self.poll_dropped_files(&ui.ctx().clone());
        self.poll_thumbnails(&ui.ctx().clone());
        if self.thumbnail_worker.as_ref().is_some_and(|w| w.has_pending()) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        if let Some(audio) = &mut self.timeline_audio {
            audio.tick();
            self.playback_speed = audio.speed();
            if audio.is_scrub_snippet_active() {
                ui.ctx().request_repaint();
            }
        }
        self.sync_timeline_audio();

        self.handle_shortcuts(ui);

        self.show_menu_bar(ui);

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.toggle_value(&mut self.settings.panels.media_pool_open, t!("toolbar.media_pool"));
                ui.toggle_value(&mut self.settings.panels.effects_open, t!("toolbar.effects"));
                ui.toggle_value(
                    &mut self.settings.panels.keyframe_editor_open,
                    t!("toolbar.keyframe_editor"),
                );
                if let Some(err) = &self.project_error {
                    ui.separator();
                    ui.colored_label(egui::Color32::RED, err);
                }
                // Sopra al pannello che apre, come i toggle a sinistra.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.toggle_value(&mut self.settings.panels.inspector_open, t!("menu.inspector"));
                });
            });
        });

        self.show_export_dialog(ui);
        self.show_settings_dialog(ui.ctx());
        self.show_about_dialog(ui.ctx());
        self.show_export_progress(ui);
        self.show_import_warnings(ui);
        self.show_relink_message(ui);
        self.show_unsaved_changes_dialog(ui);

        // Bersagli del pannello: le clip selezionate per tipo di track, in ordine
        // (track, inizio). Il frame di ciascuna è il playhead nel suo spazio
        // sorgente; la prima dà i valori mostrati, le modifiche vanno a tutte.
        let mut video_targets: Vec<PanelTarget> = Vec::new();
        let mut audio_targets: Vec<PanelTarget> = Vec::new();
        if let Some(timeline_id) = self.timeline_id {
            let tl = &self.project.timelines[timeline_id];
            for &(track_index, clip_id) in &self.timeline_state.selected {
                let Some(track) = tl.tracks.get(track_index) else {
                    continue;
                };
                let Some(clip) = track.clip(clip_id) else {
                    continue;
                };
                let local = (self.timeline_state.playhead - clip.timeline_start)
                    .clamp(0, clip.timeline_len.saturating_sub(1));
                let target = PanelTarget {
                    timeline: timeline_id,
                    track_index,
                    clip_id,
                    source_frame: clip.source_frame_at(clip.timeline_start + local),
                    timeline_start: clip.timeline_start,
                    is_solid_color: matches!(clip.source, vv_core::ClipSource::SolidColor),
                    is_text: matches!(clip.source, vv_core::ClipSource::Text),
                };
                match track.kind {
                    vv_core::TrackKind::Video => video_targets.push(target),
                    vv_core::TrackKind::Audio => audio_targets.push(target),
                }
            }
        }
        for targets in [&mut video_targets, &mut audio_targets] {
            targets.sort_by_key(|t| (t.track_index, t.timeline_start));
        }

        let mut preview_action = None;
        if self.settings.panels.media_pool_open || self.settings.panels.effects_open {
            egui::Panel::left("left_column")
                .default_size(self.settings.panels.left_column_width)
                .show(ui, |ui| {
                    if self.settings.panels.media_pool_open {
                        let pool = if self.settings.panels.effects_open {
                            egui::Panel::top("media_pool")
                                .exact_size(ui.available_height() / 2.0)
                                .resizable(false)
                                .show(ui, |ui| self.show_media_pool(ui, &mut preview_action))
                                .response
                        } else {
                            ui.scope(|ui| self.show_media_pool(ui, &mut preview_action)).response
                        };
                        // Chi ha ricevuto l'ultimo click decide a chi va Canc/Backspace.
                        if let Some(pos) = ui.ctx().input(|i| {
                            i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten()
                        }) {
                            self.media_pool_state.focused = pool.rect.contains(pos);
                        }
                        // Evidenzia il pool mentre si trascina un file dal file manager.
                        if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
                            ui.ctx()
                                .layer_painter(egui::LayerId::new(
                                    egui::Order::Foreground,
                                    egui::Id::new("media_pool_drop_highlight"),
                                ))
                                .rect_stroke(
                                    pool.rect,
                                    4.0,
                                    egui::Stroke::new(2.0, ui.visuals().selection.bg_fill),
                                    egui::StrokeKind::Inside,
                                );
                        }
                    }
                    if self.settings.panels.effects_open {
                        Self::show_effects_list(ui);
                    }
                });
            if let Some(state) = egui::PanelState::load(ui.ctx(), egui::Id::new("left_column")) {
                self.settings.panels.left_column_width = state.size().x;
            }
        }
        if !self.settings.panels.media_pool_open {
            self.media_pool_state.focused = false;
        }

        let selected_before_timeline_ui = self.timeline_state.selected.clone();
        let playhead_before_timeline_ui = self.timeline_state.playhead;
        // Senza timeline non c'è una scala per posizionare il drop: si crea la
        // timeline e si accoda a 0.
        let mut media_drop: Option<(
            timeline_ui::TimelineDrag,
            FrameIdx,
            timeline_ui::MediaDropTarget,
        )> =
            None;
        let mut dropped_on_empty_timeline: Option<timeline_ui::TimelineDrag> = None;
        egui::Panel::bottom("timeline")
            .default_size(self.settings.panels.timeline_height)
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
                    if !self.timeline_stack.is_empty() {
                        let mut jump_to = None;
                        ui.horizontal(|ui| {
                            for i in 0..self.timeline_stack.len() {
                                let name = self.timeline_display_name(self.timeline_stack[i]);
                                if ui.link(name).clicked() {
                                    jump_to = Some(i);
                                }
                                ui.label(">");
                            }
                            ui.label(self.timeline_display_name(timeline_id));
                        });
                        if let Some(index) = jump_to {
                            self.exit_to_timeline_stack_index(index);
                        }
                    }
                    let labels: HashMap<MediaId, String> = self
                        .project
                        .media_pool
                        .iter()
                        .map(|(id, item)| (id, file_label(&item.path)))
                        .collect();
                    let buffered_ranges = self.buffered_timeline_ranges();
                    let proxy_ranges = self.proxy_timeline_ranges();
                    // Il buffer avanza su un altro thread: senza repaint la striscia
                    // "buffered" non si aggiornerebbe.
                    if self.render_ahead.as_ref().is_some_and(|r| !r.is_caught_up()) {
                        ui.ctx().request_repaint();
                    }
                    // Picchi audio già in memoria (caricati dal file di cache
                    // la prima volta che servono, vedi `ensure_waveforms_loaded`):
                    // la timeline li disegna come waveform sulle clip audio.
                    self.ensure_waveforms_loaded();
                    let is_playing = self.is_timeline_playing();
                    let (drop, enter_compound) = timeline_ui::show_timeline(
                        ui,
                        &mut self.project,
                        &mut self.history,
                        timeline_id,
                        &|id| labels.get(&id).cloned().unwrap_or_default(),
                        &mut self.timeline_state,
                        self.snapping_enabled,
                        self.settings.kinetic_scroll,
                        &buffered_ranges,
                        &proxy_ranges,
                        &self.waveform_cache,
                        is_playing,
                    );
                    media_drop = drop;
                    if let Some(nested_id) = enter_compound {
                        self.enter_compound_timeline(nested_id);
                    }
                } else {
                    let drop_rect = ui.available_rect_before_wrap();
                    let drop_id = ui.id().with("timeline_drop_zone_empty");
                    let drop_resp = ui.interact(drop_rect, drop_id, egui::Sense::hover());
                    dropped_on_empty_timeline = timeline_ui::TimelineDrag::released(&drop_resp);
                    ui.label(t!("timeline.empty_hint"));
                }
            });
        if let Some(state) = egui::PanelState::load(ui.ctx(), egui::Id::new("timeline")) {
            self.settings.panels.timeline_height = state.size().y;
        }
        if let Some(drag) = dropped_on_empty_timeline {
            self.add_drop_to_timeline_at(&drag, 0, timeline_ui::MediaDropTarget::Default);
        }
        if let Some((drag, start, target)) = media_drop {
            self.add_drop_to_timeline_at(&drag, start, target);
        }

        // Uno scrub dell'utente va seguito anche durante la riproduzione, a
        // differenza del playhead mosso da `drive_playback`.
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
            // La selezione segue anche il playhead mosso dalla riproduzione, ma solo
            // se si è mosso davvero: non tocca una selezione fatta in questo frame.
            let playhead_before_playback = self.timeline_state.playhead;
            self.drive_playback();
            if self.timeline_state.playhead != playhead_before_playback {
                self.sync_selection_to_playhead();
            }
        }
        self.drive_browse_playback();
        // Il buffer della timeline resta caldo anche durante l'anteprima del pool.
        self.sync_render_ahead();
        self.sync_root_timeline_media();

        let (mut pending_effects, mut pending_playhead) =
            self.show_properties_panel(ui, &video_targets, &audio_targets);

        if self.settings.panels.keyframe_editor_open {
            let target = video_targets
                .first()
                .or(audio_targets.first())
                .map(|t| (t.timeline, t.track_index, t.clip_id));
            let editor = keyframe_editor::show_keyframe_editor(
                ui.ctx(),
                &mut self.settings.panels.keyframe_editor_open,
                &mut self.keyframe_editor,
                &self.project,
                target,
                self.timeline_state.playhead,
            );
            pending_effects.extend(editor.commands);
            pending_playhead = editor.playhead.or(pending_playhead);
        }

        if let Some(id) = preview_action {
            self.preview_media(id);
            // Anteprima del pool: nessuna clip attiva, e il playhead non deve
            // toglierla al frame dopo.
            self.active_clip = None;
            self.browsing_media = Some(id);
        }
        if let Some(frame) = pending_playhead {
            self.timeline_state.playhead = frame.max(0);
        }

        // Una modifica dal pannello può toccare più clip (selezione
        // multipla): un solo comando composito, così l'undo le riporta
        // indietro tutte insieme.
        // Un menu a tendina aperto sta applicando l'anteprima delle voci:
        // le sue modifiche vanno in un solo passo di undo, come un drag.
        let holding = ui.input(|i| i.pointer.any_down()) || preview_combo_open(ui.ctx());
        self.apply_effect_changes(pending_effects, holding);

        let mut transport_action = transport::TransportResponse::default();
        let mut viewer_rect = None;
        // Tutto il riquadro del player, bande comprese: gli handle vicini al
        // bordo del frame devono restare afferrabili anche fuori.
        let mut viewer_area = None;
        let mut overlay_effects: Vec<BoxedCommand> = Vec::new();
        egui::CentralPanel::default().show(ui, |ui| {
            // Dentro il CentralPanel: occupa solo la colonna del viewer.
            egui::Panel::bottom("view_toggles")
                .default_size(28.0)
                .resizable(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        magnet_toggle(ui, &mut self.snapping_enabled)
                            .on_hover_text(t!("toolbar.snapping"));
                        transform_overlay_toggle(ui, &mut self.show_transform_overlay)
                            .on_hover_text(t!("toolbar.transform_overlay"));
                    });
                });
            let (total, playhead, marks, playing) = self.transport_state();
            egui::Panel::bottom("transport")
                .resizable(false)
                .show(ui, |ui| {
                    transport_action = transport::show_transport(ui, total, playhead, marks, playing);
                });
            let media_offline = self.active_clip_media_offline();
            // Sfogliando un media "grezzo" dal media pool non c'è nessuna
            // timeline da comporre: un layer solo, il suo frame com'è.
            let layers = if media_offline || self.browsing_media.is_some() {
                None
            } else {
                self.timeline_video_layers()
            };

            if media_offline {
                self.last_viewer_frame_kind = Some(ViewerFrameKind::Offline);
            } else if self.browsing_media.is_some() {
                if self.preview_meta.as_ref().is_some_and(|m| !m.has_video) {
                    self.last_viewer_frame_kind = None;
                } else if let Some(frame) = self.browsing_video_frame() {
                    let layer = vv_render::Layer::Video {
                        frame: frame_provider::as_render_yuv_frame(&frame),
                        transform: vv_core::Transform::default(),
                        source_size: (frame.width, frame.height),
                        opacity: 1.0,
                        filters: &[],
                        blend: vv_core::BlendMode::Normal,
                    };
                    self.show_composited(
                        &[layer],
                        vv_render::OutputFrame::exact(frame.width, frame.height),
                    );
                }
            } else if let Some(layers) = layers {
                let video_size = layers
                    .iter()
                    .filter_map(|l| match l {
                        frame_provider::OwnedLayer::Video { frame, .. } => {
                            Some((frame.width, frame.height))
                        }
                        _ => None,
                    })
                    .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)));
                let timeline_size = self
                    .timeline_id
                    .map(|id| self.project.timelines[id].resolution);
                // Con sole clip SolidColor si compone alla risoluzione
                // della timeline.
                let composite_size = video_size.or(timeline_size.filter(|_| !layers.is_empty()));

                match composite_size {
                    Some(size) => {
                        // Alla risoluzione del frame decodificato, allargata all'aspect della
                        // timeline: le bande si vedono senza upscalare.
                        let timeline_size = timeline_size.unwrap_or(size);
                        let (out_w, out_h) = vv_render::fit_output_size(size, timeline_size);
                        let render_layers: Vec<vv_render::Layer> =
                            layers.iter().map(frame_provider::OwnedLayer::as_render).collect();
                        self.show_composited(
                            &render_layers,
                            vv_render::OutputFrame::scaled(out_w, out_h, timeline_size),
                        );
                    }
                    // Vuoto: nero, non l'ultimo frame rimasto. Basta una
                    // texture minuscola con l'aspect della timeline.
                    None => {
                        let (w, h) = timeline_size.unwrap_or((16, 9));
                        let step = (w.max(h) / 64).max(1);
                        self.show_composited(
                            &[],
                            vv_render::OutputFrame::scaled(
                                (w / step).max(1),
                                (h / step).max(1),
                                (w, h),
                            ),
                        );
                    }
                }
            }

            match self.last_viewer_frame_kind {
                Some(ViewerFrameKind::Video) => {
                    if let (Some(id), Some(tex_size)) =
                        (self.video_texture_id, self.video_display_size)
                    {
                        let available = ui.available_size();
                        let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                        let display_size = tex_size * scale.max(0.0);
                        let area = ui
                            .centered_and_justified(|ui| {
                                ui.add(
                                    egui::Image::new(egui::load::SizedTexture::new(id, tex_size))
                                        .fit_to_exact_size(display_size),
                                )
                            })
                            .inner
                            .rect;
                        viewer_area = Some(area);
                        viewer_rect =
                            Some(egui::Rect::from_center_size(area.center(), display_size));
                    }
                }
                Some(ViewerFrameKind::Offline) => {
                    ui.centered_and_justified(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(230, 70, 70),
                            egui::RichText::new(t!("viewer.media_offline")).size(24.0),
                        );
                    });
                }
                None => {
                    if let Some(err) = &self.preview_error {
                        ui.colored_label(egui::Color32::RED, t!("viewer.player_error", error = err));
                    } else {
                        let audio_only = self.browsing_media.is_some()
                            && self.preview_meta.as_ref().is_some_and(|m| !m.has_video);
                        let label = ui.centered_and_justified(|ui| {
                            ui.label(if audio_only {
                                t!("viewer.audio_only")
                            } else if self.browsing_media.is_some() || self.active_clip.is_some() {
                                t!("viewer.decoding")
                            } else {
                                t!("viewer.empty_hint")
                            })
                        });
                        // Si trascina in timeline anche senza immagine.
                        if audio_only {
                            viewer_rect = Some(label.inner.rect);
                        }
                    }
                }
            }

            if let (Some(rect), Some(area)) = (viewer_rect, viewer_area) {
                self.show_viewer_overlay(ui, rect, area, &video_targets, &mut overlay_effects);
            }

            if let (Some(media_id), Some(rect)) = (self.browsing_media, viewer_rect) {
                let (source_in, source_out) =
                    self.browse_marks.resolve(self.browse_total_frames());
                let drag_id = ui.id().with("viewer_media_drag");
                let resp = ui
                    .interact(rect, drag_id, egui::Sense::drag())
                    .on_hover_text(t!("viewer.drag_in_out"));
                let mut drags = vec![(resp, timeline_ui::DragStreams::All)];
                if self.preview_meta.as_ref().is_some_and(|m| m.has_video && m.has_audio) {
                    drags.extend(show_stream_drag_handles(ui, rect));
                }
                for (resp, streams) in drags {
                    resp.dnd_set_drag_payload(timeline_ui::MediaDragSet::one(
                        timeline_ui::MediaDrag {
                            media_id,
                            source_in,
                            source_out,
                            streams,
                        },
                    ));
                    if resp.dragged()
                        && let Some(item) = self.project.media_pool.get(media_id)
                    {
                        let name = file_label(&item.path);
                        let ghost = match streams {
                            timeline_ui::DragStreams::All => name,
                            timeline_ui::DragStreams::VideoOnly => {
                                t!("viewer.ghost_video_only", name = name).into_owned()
                            }
                            timeline_ui::DragStreams::AudioOnly => {
                                t!("viewer.ghost_audio_only", name = name).into_owned()
                            }
                        };
                        show_drag_ghost(ui, resp.id, &ghost);
                    }
                }
            }
        });

        if self.viewer_fullscreen {
            transport_action = self.show_fullscreen_viewer(ui.ctx());
        }

        let pointer_down = ui.input(|i| i.pointer.any_down());
        self.apply_effect_changes(overlay_effects, pointer_down);

        if transport_action.toggle_play {
            self.toggle_playback();
        }
        if let Some(frame) = transport_action.seek {
            if self.browsing_media.is_some() {
                self.seek_browse(frame);
            } else {
                self.timeline_state.playhead = frame;
                self.ensure_active_clip_matches_playhead(true);
                self.sync_selection_to_playhead();
                self.play_scrub_audio();
            }
            ui.ctx().request_repaint();
        }

        if self.is_timeline_playing() {
            ui.ctx().request_repaint();
        }

        // Repaint durante i drag, altrimenti poco fluidi a player fermo.
        if ui
            .ctx()
            .input(|i| i.pointer.any_down() || i.pointer.any_released())
        {
            ui.ctx().request_repaint();
        }
    }

    /// Chiamata da eframe alla chiusura e periodicamente (vedi
    /// `auto_save_interval`): il layout dei pannelli aggiornato a ogni frame
    /// in `ui()` finisce così su disco senza scriverlo ad ogni resize.
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist_settings();
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Argomento opzionale: path di un video da importare subito all'avvio
    // (comodo per debug/smoke test, oltre che per l'uso da riga di comando).
    let startup_path = std::env::args().nth(1).map(PathBuf::from);
    std::thread::spawn(vv_render::text::warm_up);
    // La prima verifica di NVENC inizializza CUDA: meglio non nella UI.
    std::thread::spawn(|| vv_media::VideoCodec::Nvenc.is_available());

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
            app.settings_path = settings::Settings::default_path();
            if let Some(path) = &app.settings_path {
                app.settings = settings::Settings::load(path);
            }
            app.settings.language.apply();
            // Aperto subito: aprire lo stream audio blocca per centinaia di ms.
            app.timeline_audio = Some(TimelineAudio::new());
            if let Some(render_state) = cc.wgpu_render_state.clone() {
                app.compositor = vv_render::Compositor::new(
                    std::sync::Arc::new(render_state.device.clone()),
                    std::sync::Arc::new(render_state.queue.clone()),
                );
                app.egui_render_state = Some(render_state);
            }
            #[cfg(target_os = "linux")]
            {
                app.wayland_dnd = wayland_dnd::WaylandDnd::start(cc);
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
        let clip = vv_core::Clip::from_source_range(
            clip_id,
            vv_core::ClipSource::SolidColor,
            0,
            len,
            start,
            vv_core::Rational::one(),
        );
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

    fn insert_compound_media(app: &mut VibeVideoApp, nested_id: TimelineId) -> MediaId {
        app.project.media_pool.insert(vv_core::MediaItem {
            path: "Compound Clip 1".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 10,
                fps: vv_core::Rational::new(25, 1),
                width: 1920,
                height: 1080,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: Some(nested_id),
        })
    }

    /// Copiare da una compound clip di sole track video e incollare nella
    /// timeline: V2 deve restare video, non finire sulla track audio che
    /// lì ha lo stesso indice assoluto.
    #[test]
    fn pasting_from_a_video_only_compound_keeps_the_clips_on_video_tracks() {
        let mut app = VibeVideoApp::default();
        let root_id = app.ensure_timeline();
        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Video)],
        });
        insert_compound_media(&mut app, nested_id);
        app.enter_compound_timeline(nested_id);
        for track_index in 0..2 {
            let clip_id = app.project.alloc_clip_id();
            let clip = vv_core::Clip::from_source_range(
                clip_id,
                vv_core::ClipSource::SolidColor,
                0,
                20,
                0,
                vv_core::Rational::one(),
            );
            app.project.timelines[nested_id].tracks[track_index].insert_sorted(clip);
            app.timeline_state.selected.insert((track_index, clip_id));
        }
        app.copy_selected_clips();

        app.exit_to_timeline_stack_index(0);
        app.timeline_state.playhead = 0;
        app.paste_clipboard_at_playhead();

        let tl = &app.project.timelines[root_id];
        let video_tracks: Vec<usize> = tl.tracks_of_kind(TrackKind::Video).map(|(i, _)| i).collect();
        assert_eq!(video_tracks.len(), 2, "la V2 mancante viene creata");
        for &index in &video_tracks {
            assert_eq!(tl.tracks[index].clips.len(), 1);
        }
        for (_, track) in tl.tracks_of_kind(TrackKind::Audio) {
            assert!(track.clips.is_empty(), "niente clip video sull'audio");
        }
    }

    #[test]
    fn deleting_a_compound_clip_while_editing_it_goes_back_to_the_parent_timeline() {
        let mut app = VibeVideoApp::default();
        let root_id = app.ensure_timeline();
        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video)],
        });
        let media = insert_compound_media(&mut app, nested_id);
        app.enter_compound_timeline(nested_id);

        app.media_pool_state.selected.insert(media);
        app.delete_selected_media();

        assert_eq!(app.timeline_id, Some(root_id));
        assert!(app.timeline_stack.is_empty());
        assert!(!app.project.timelines.contains_key(nested_id));
    }

    /// Una compound clip non ha un file da decodificare: i picchi si
    /// compongono da quelli delle sue clip audio annidate.
    #[test]
    fn a_compound_waveform_is_composed_from_the_nested_clips() {
        let mut app = VibeVideoApp::default();
        let source = app.project.media_pool.insert(vv_core::MediaItem {
            path: "a.wav".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 50,
                fps: vv_core::Rational::new(25, 1),
                width: 0,
                height: 0,
                has_video: false,
                has_audio: true,
                sample_rate: 48_000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 7,
            compound: None,
        });
        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Audio)],
        });
        let clip = vv_core::Clip::from_source_range(
            app.project.alloc_clip_id(),
            vv_core::ClipSource::Media(source),
            0,
            25,
            0,
            vv_core::Rational::one(),
        );
        app.project.timelines[nested_id].tracks[0].insert_sorted(clip);
        let compound = insert_compound_media(&mut app, nested_id);
        app.project.media_pool[compound].meta.has_audio = true;
        app.project.media_pool[compound].meta.duration_frames = 50;

        app.waveform_cache.insert(
            (7, 0),
            vv_media::Waveform {
                peaks: vec![1.0; 100],
                audio_duration_secs: 2.0,
            },
        );
        let (waveform, complete) =
            compose_compound_waveform(&app.project, &app.waveform_cache, compound).unwrap();

        assert!(complete);
        assert_eq!(waveform.audio_duration_secs, 2.0);
        let half = waveform.peaks.len() / 2;
        assert!(waveform.peaks[..half].iter().all(|&p| p > 0.0), "prima metà suona");
        assert!(waveform.peaks[half + 1..].iter().all(|&p| p == 0.0), "seconda metà muta");
    }

    #[test]
    fn entering_and_exiting_a_compound_timeline_switches_the_active_one_and_resets_ui_state() {
        let mut app = VibeVideoApp::default();
        let root_id = app.ensure_timeline();
        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video)],
        });
        insert_compound_media(&mut app, nested_id);
        app.timeline_state.playhead = 42;
        app.timeline_state.selected = BTreeSet::from([(0, ClipId(999))]);

        app.enter_compound_timeline(nested_id);

        assert_eq!(app.timeline_id, Some(nested_id));
        assert_eq!(app.timeline_stack, vec![root_id], "la radice resta in pila per il breadcrumb");
        assert_eq!(app.timeline_state.playhead, 0, "playhead azzerato entrando in un livello nuovo");
        assert!(app.timeline_state.selected.is_empty(), "selezione azzerata entrando in un livello nuovo");

        app.exit_to_timeline_stack_index(0);

        assert_eq!(app.timeline_id, Some(root_id));
        assert!(app.timeline_stack.is_empty(), "tornati alla radice, la pila si svuota");
    }

    #[test]
    fn entering_the_current_timeline_or_an_ancestor_already_in_the_stack_is_a_no_op() {
        let mut app = VibeVideoApp::default();
        let root_id = app.ensure_timeline();

        // Voluto o per un ciclo residuo (vedi MAX_COMPOUND_DEPTH): non deve
        // impilare la timeline corrente su se stessa.
        app.enter_compound_timeline(root_id);
        assert_eq!(app.timeline_id, Some(root_id));
        assert!(app.timeline_stack.is_empty());

        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video)],
        });
        insert_compound_media(&mut app, nested_id);
        app.enter_compound_timeline(nested_id);
        assert_eq!(app.timeline_stack, vec![root_id]);

        // La radice è già un antenato in pila: rientrarci non deve
        // impilare `nested_id` una seconda volta sopra se stessa.
        app.enter_compound_timeline(root_id);
        assert_eq!(app.timeline_id, Some(nested_id), "resta dov'era, il tentativo è ignorato");
        assert_eq!(app.timeline_stack, vec![root_id], "la pila non cresce");
    }

    /// Copiare in una timeline, entrare in una compound clip e incollare
    /// lì deve funzionare: la clipboard non è per-timeline.
    #[test]
    fn clipboard_survives_navigating_into_a_compound_timeline_and_pastes_there() {
        let mut app = VibeVideoApp::default();
        let root_id = app.ensure_timeline();
        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video)],
        });
        insert_compound_media(&mut app, nested_id);

        app.timeline_state.clipboard = vec![timeline_ui::ClipboardEntry {
            track_kind: TrackKind::Video,
            track_number: 1,
            relative_start: 0,
            clip: vv_core::Clip::from_source_range(
                ClipId(1),
                vv_core::ClipSource::SolidColor,
                0,
                20,
                0,
                vv_core::Rational::one(),
            ),
            timeline_fps: vv_core::Rational::new(25, 1),
            link_tag: None,
        }];

        app.enter_compound_timeline(nested_id);
        assert_eq!(app.timeline_state.clipboard.len(), 1, "la clipboard sopravvive alla navigazione");

        app.timeline_state.playhead = 0;
        app.paste_clipboard_at_playhead();

        assert_eq!(
            app.project.timelines[nested_id].tracks[0].clips.len(),
            1,
            "incollata nella timeline annidata"
        );
        assert!(app.project.timelines[root_id].tracks[0].clips.is_empty(), "non nella radice");
    }

    /// Bug segnalato dall'utente: trascinare la voce di una timeline (la
    /// timeline del progetto, o una compound clip) dal media pool dentro
    /// se stessa deve essere rifiutato, non solo fermato durante il
    /// rendering (`MAX_COMPOUND_DEPTH` in render_ahead.rs è solo la rete
    /// di sicurezza, non deve mai scattare in uso normale).
    #[test]
    fn dropping_a_timelines_own_media_into_itself_is_refused() {
        let mut app = VibeVideoApp::default();
        let root_id = app.ensure_timeline();
        let root_media = app
            .project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound == Some(root_id))
            .map(|(id, _)| id)
            .expect("la timeline del progetto ha una voce nel pool");

        app.add_media_to_timeline(root_media);

        assert!(
            app.project.timelines[root_id].tracks.iter().all(|t| t.clips.is_empty()),
            "il drop è stato rifiutato, non deve comparire nessuna clip"
        );
    }

    /// Come sopra, ma indiretto: B contiene già una clip che referenzia A,
    /// trascinare B dentro A chiuderebbe il ciclo A -> B -> A.
    #[test]
    fn dropping_a_compound_clip_that_would_close_an_indirect_cycle_is_refused() {
        let mut app = VibeVideoApp::default();
        let root_id = app.ensure_timeline();
        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video)],
        });
        let compound_media = insert_compound_media(&mut app, nested_id);
        let root_media = app
            .project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound == Some(root_id))
            .map(|(id, _)| id)
            .unwrap();
        // La timeline annidata contiene già una clip che referenzia la
        // radice del progetto.
        app.project.timelines[nested_id].tracks[0].clips.push(vv_core::Clip::from_source_range(
            ClipId(1),
            vv_core::ClipSource::Media(root_media),
            0,
            10,
            0,
            vv_core::Rational::one(),
        ));

        // Trascinare la compound clip (che porta a `nested_id`, che porta
        // già alla radice) dentro la radice chiuderebbe il ciclo.
        app.add_media_to_timeline(compound_media);

        assert!(
            app.project.timelines[root_id].tracks[0].clips.is_empty(),
            "il drop indiretto è stato rifiutato"
        );
    }

    /// Due track video sovrapposte (REFACTOR_PIPELINE.md B4): la seconda
    /// (aggiunta con `AddTrack`, quindi in coda a `tracks` — più in alto
    /// di quella di default) ha una clip più corta al centro di quella
    /// della prima. `active_video_clip_at` deve vedere quella in cima dove
    /// c'è, e tornare a quella sotto appena finisce — la stessa
    /// regola che il viewer usa per seguire il playhead.
    /// Timeline a 30 fps + media a 29,97: la clip inserita porta il
    /// `rate` di conformazione e dura in timeline il tempo reale del
    /// media, non i suoi frame contati 1:1.
    fn app_with_media_at(timeline_fps: vv_core::Rational, media_fps: vv_core::Rational, duration_frames: FrameIdx) -> (VibeVideoApp, MediaId) {
        let mut app = VibeVideoApp::default();
        let media_id = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/vv-conform-test.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames,
                fps: media_fps,
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: None,
        });
        let timeline_id = app.project.timelines.insert(vv_core::Timeline {
            name: "T".into(),
            fps: timeline_fps,
            resolution: (320, 240),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        app.timeline_id = Some(timeline_id);
        (app, media_id)
    }

    #[test]
    fn dropping_several_media_appends_them_in_pool_order() {
        let (mut app, media_a) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            50,
        );
        let media_b = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/vv-b.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 30,
                fps: vv_core::Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 2,
            compound: None,
        });
        let timeline_id = app.timeline_id.unwrap();
        let drag = |app: &VibeVideoApp, id: MediaId| {
            timeline_ui::MediaDrag::whole(id, &app.project.media_pool[id].meta)
        };
        let set = timeline_ui::MediaDragSet {
            items: vec![drag(&app, media_a), drag(&app, media_b)],
        };

        app.add_media_set_to_timeline_at(&set, 100, timeline_ui::MediaDropTarget::Default);

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].timeline_start, 100);
        assert_eq!(clips[0].timeline_len, 50);
        assert_eq!(
            clips[1].timeline_start, 150,
            "il secondo media parte dove finisce il primo"
        );
        assert_eq!(clips[1].timeline_len, 30);
        assert!(matches!(clips[0].source, vv_core::ClipSource::Media(id) if id == media_a));
        assert!(matches!(clips[1].source, vv_core::ClipSource::Media(id) if id == media_b));
    }

    /// Un drop = un solo Ctrl+Z, anche con più media, più stream audio e
    /// una track creata al volo.
    #[test]
    fn dropping_several_media_is_a_single_undo_step() {
        let (mut app, media_a) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            50,
        );
        let media_b = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/vv-b.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 30,
                fps: vv_core::Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: true,
                sample_rate: 48000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 2,
            compound: None,
        });
        let timeline_id = app.timeline_id.unwrap();
        let tracks_before = app.project.timelines[timeline_id].tracks.len();
        let drag = |app: &VibeVideoApp, id: MediaId| {
            timeline_ui::MediaDrag::whole(id, &app.project.media_pool[id].meta)
        };
        let set = timeline_ui::MediaDragSet {
            items: vec![drag(&app, media_a), drag(&app, media_b)],
        };

        app.add_media_set_to_timeline_at(&set, 0, timeline_ui::MediaDropTarget::NewVideoTrack);
        let clips_after_drop: usize = app.project.timelines[timeline_id]
            .tracks
            .iter()
            .map(|t| t.clips.len())
            .sum();
        assert!(clips_after_drop >= 3, "video + audio di entrambi i media");

        app.history.undo(&mut app.project);

        let tl = &app.project.timelines[timeline_id];
        assert!(
            tl.tracks.iter().all(|t| t.clips.is_empty()),
            "un solo Ctrl+Z deve togliere tutte le clip del drop"
        );
        assert_eq!(tl.tracks.len(), tracks_before, "e anche la track creata dal drop");
    }

    /// Drop multiplo sulla fascia "nuova track video": la track si crea una
    /// volta sola per l'intero drop, non una per media.
    #[test]
    fn dropping_several_media_on_the_new_track_zone_creates_one_track() {
        let (mut app, media_a) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            50,
        );
        let media_b = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/vv-b.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 30,
                fps: vv_core::Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 2,
            compound: None,
        });
        let timeline_id = app.timeline_id.unwrap();
        let tracks_before = app.project.timelines[timeline_id].tracks.len();
        let drag = |app: &VibeVideoApp, id: MediaId| {
            timeline_ui::MediaDrag::whole(id, &app.project.media_pool[id].meta)
        };
        let set = timeline_ui::MediaDragSet {
            items: vec![drag(&app, media_a), drag(&app, media_b)],
        };

        app.add_media_set_to_timeline_at(&set, 0, timeline_ui::MediaDropTarget::NewVideoTrack);

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks.len(), tracks_before + 1);
        assert_eq!(tl.tracks[tracks_before].clips.len(), 2);
    }

    #[test]
    fn deleting_a_media_leaves_its_clip_in_timeline_but_offline() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            100,
        );
        let timeline_id = app.timeline_id.unwrap();
        let meta = app.project.media_pool[media_id].meta.clone();
        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;
        app.active_clip = Some((0, clip_id));

        app.media_pool_state.selected = BTreeSet::from([media_id]);
        app.delete_selected_media();

        assert!(app.project.media_pool.is_empty());
        assert_eq!(app.project.timelines[timeline_id].tracks[0].clips.len(), 1);
        assert!(app.active_clip_media_offline());
        assert!(app.media_pool_state.selected.is_empty());

        app.history.undo(&mut app.project);
        assert!(
            !app.active_clip_media_offline(),
            "l'undo deve riagganciare la clip al media reinserito"
        );
    }

    #[test]
    fn select_all_media_selects_every_media_in_the_pool() {
        let (mut app, media_a) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            50,
        );
        let media_b = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/vv-b.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 30,
                fps: vv_core::Rational::new(25, 1),
                width: 320,
                height: 240,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 2,
            compound: None,
        });

        app.select_all_media();

        assert_eq!(app.media_pool_state.selected, BTreeSet::from([media_a, media_b]));
    }

    /// Simula il cambio di postazione: il media punta a un percorso che
    /// non esiste più, ma sotto una nuova cartella base c'è un file con
    /// lo stesso nome, in una sottocartella qualsiasi.
    #[test]
    fn relink_media_finds_offline_files_by_name_under_the_base_folder() {
        let dir = std::env::temp_dir().join(format!("vv-app-relink-test-{}", std::process::id()));
        let nested = dir.join("progetto").join("clip");
        std::fs::create_dir_all(&nested).unwrap();
        let found_path = nested.join("intervista.mp4");
        std::fs::write(&found_path, b"contenuto video").unwrap();
        let already_ok_path = dir.join("gia-raggiungibile.mp4");
        std::fs::write(&already_ok_path, b"altro contenuto").unwrap();

        let mut app = VibeVideoApp::default();
        let meta = vv_core::MediaMeta {
            duration_frames: 10,
            fps: vv_core::Rational::new(25, 1),
            width: 320,
            height: 240,
            has_video: true,
            has_audio: false,
            sample_rate: 0,
            channels: 0,
            audio_streams: 0,
        };
        let offline = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/questo/percorso/non/esiste/piu/intervista.mp4".into(),
            meta: meta.clone(),
            content_hash: 1,
            compound: None,
        });
        let unresolvable = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/altro/percorso/inesistente/fantasma.mp4".into(),
            meta: meta.clone(),
            content_hash: 3,
            compound: None,
        });
        let already_ok = app.project.media_pool.insert(vv_core::MediaItem {
            path: already_ok_path.clone(),
            meta,
            content_hash: 2,
            compound: None,
        });
        app.relink_media(&dir, &[offline, unresolvable, already_ok]);

        assert_eq!(app.project.media_pool[offline].path, found_path);
        assert_ne!(
            app.project.media_pool[offline].content_hash, 1,
            "l'hash va ricalcolato sul nuovo percorso"
        );
        assert_eq!(
            app.project.media_pool[unresolvable].path,
            PathBuf::from("/altro/percorso/inesistente/fantasma.mp4"),
            "senza un file corrispondente il percorso resta quello vecchio"
        );
        assert_eq!(
            app.project.media_pool[already_ok].path, already_ok_path,
            "un media già raggiungibile al suo percorso non va toccato"
        );
        assert_eq!(app.project.media_pool[already_ok].content_hash, 2);
        assert_eq!(
            app.relink_message,
            Some("Relinked 1 media, 1 not found.".to_string())
        );

        app.history.undo(&mut app.project);
        assert_eq!(
            app.project.media_pool[offline].path,
            PathBuf::from("/questo/percorso/non/esiste/piu/intervista.mp4"),
            "l'undo deve riportare il percorso a quello di prima del relink"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `relink_media` tocca solo i `targets` passati esplicitamente, mai
    /// il resto del pool — anche se ricollegabile.
    #[test]
    fn relink_media_with_a_selection_only_touches_the_selected_media() {
        let dir = std::env::temp_dir().join(format!("vv-app-relink-selection-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let found_path = dir.join("a.mp4");
        std::fs::write(&found_path, b"a").unwrap();
        let other_found_path = dir.join("b.mp4");
        std::fs::write(&other_found_path, b"b").unwrap();

        let mut app = VibeVideoApp::default();
        let meta = vv_core::MediaMeta {
            duration_frames: 10,
            fps: vv_core::Rational::new(25, 1),
            width: 320,
            height: 240,
            has_video: true,
            has_audio: false,
            sample_rate: 0,
            channels: 0,
            audio_streams: 0,
        };
        let selected = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/mancante/a.mp4".into(),
            meta: meta.clone(),
            content_hash: 1,
            compound: None,
        });
        let not_selected = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/mancante/b.mp4".into(),
            meta,
            content_hash: 2,
            compound: None,
        });
        app.relink_media(&dir, &[selected]);

        assert_eq!(app.project.media_pool[selected].path, found_path);
        assert_eq!(
            app.project.media_pool[not_selected].path,
            PathBuf::from("/mancante/b.mp4"),
            "senza essere selezionato non viene ricollegato anche se trovabile"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Solo `relink_media_dialog` (l'ingresso dal menu contestuale) salta
    /// il file dialog senza selezione: `relink_media` di per sé opera
    /// sempre sulla selezione data, vuota compresa (nessun target, quindi
    /// nessun comando e nessun messaggio).
    #[test]
    fn relink_media_dialog_is_a_no_op_without_a_selection() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            10,
        );
        let original_path = app.project.media_pool[media_id].path.clone();

        app.relink_media_dialog();

        assert_eq!(app.project.media_pool[media_id].path, original_path);
        assert!(app.relink_message.is_none());
    }

    #[test]
    fn deleting_a_media_being_previewed_stops_the_preview() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            100,
        );
        app.browsing_media = Some(media_id);
        app.media_pool_state.selected = BTreeSet::from([media_id]);
        app.delete_selected_media();
        assert_eq!(app.browsing_media, None);
    }

    #[test]
    fn inserting_a_media_at_another_fps_conforms_it_to_the_timeline() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(30, 1),
            vv_core::Rational::new(30_000, 1001),
            3000,
        );
        let timeline_id = app.timeline_id.unwrap();
        let meta = app.project.media_pool[media_id].meta.clone();

        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(clip.rate, vv_core::Rational::new(1001, 1000));
        assert_eq!(clip.source_len(), 3000);
        assert_eq!(clip.timeline_len, 3003, "100,1 s a 30 fps");
        assert_eq!(clip.source_frame_at(clip.timeline_end() - 1), 2999);
    }

    /// Allungare il bordo di una clip sopra la vicina la sovrascrive: la
    /// vicina viene tagliata dove arriva il nuovo bordo, non spostata —
    /// è quel che fa la UI al rilascio del trim (vedi
    /// `PendingAction::Trim`), qui riprodotto con gli stessi comandi.
    #[test]
    fn extending_a_clip_over_its_neighbor_cuts_the_neighbor() {
        let mut app = VibeVideoApp::default();
        let a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let b = make_timeline_with_clip(&mut app, 0, 10, 10);
        let timeline_id = app.timeline_id.unwrap();

        apply_trim_with_overwrite(&mut app, timeline_id, 0, a, vv_core::TrimEdge::End, 15);

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].id, a);
        assert_eq!(clips[0].timeline_end(), 15);
        assert_eq!(clips[1].id, b);
        assert_eq!(clips[1].timeline_start, 15, "la vicina è tagliata, non spostata");
        assert_eq!(clips[1].timeline_end(), 20);
    }

    /// Se l'allungamento copre la vicina per intero, la vicina sparisce.
    #[test]
    fn extending_a_clip_over_a_whole_neighbor_removes_it() {
        let mut app = VibeVideoApp::default();
        let a = make_timeline_with_clip(&mut app, 0, 0, 10);
        make_timeline_with_clip(&mut app, 0, 10, 10);
        let c = make_timeline_with_clip(&mut app, 0, 20, 10);
        let timeline_id = app.timeline_id.unwrap();

        apply_trim_with_overwrite(&mut app, timeline_id, 0, a, vv_core::TrimEdge::End, 20);

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].id, a);
        assert_eq!(clips[0].timeline_end(), 20);
        assert_eq!(clips[1].id, c, "la clip coperta per intero è sparita");
        assert_eq!(clips[1].timeline_start, 20, "quella dopo resta dov'è");
    }

    /// Allungando il bordo *sinistro* all'indietro vale la stessa regola.
    #[test]
    fn extending_a_clip_backwards_cuts_the_previous_neighbor() {
        let mut app = VibeVideoApp::default();
        let a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let timeline_id = app.timeline_id.unwrap();
        // b nasce con source_in 8: ha davvero 8 frame di margine per
        // risalire sopra la vicina.
        let b = app.project.alloc_clip_id();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip: vv_core::Clip::from_source_range(
                    b,
                    vv_core::ClipSource::SolidColor,
                    8,
                    18,
                    12,
                    vv_core::Rational::one(),
                ),
            }),
        );

        apply_trim_with_overwrite(&mut app, timeline_id, 0, b, vv_core::TrimEdge::Start, 6);

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].id, a);
        assert_eq!(clips[0].timeline_end(), 6);
        assert_eq!(clips[1].id, b);
        assert_eq!(clips[1].timeline_start, 6);
    }

    /// Il trim di un bordo con l'overwrite di quel che incontra, come lo
    /// compone la UI: prima si libera il tratto guadagnato, poi si trimma.
    fn apply_trim_with_overwrite(
        app: &mut VibeVideoApp,
        timeline_id: TimelineId,
        track_index: usize,
        clip_id: ClipId,
        edge: vv_core::TrimEdge,
        new_value: FrameIdx,
    ) {
        let clip = app.project.timelines[timeline_id].tracks[track_index]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .unwrap()
            .clone();
        let range = match edge {
            vv_core::TrimEdge::Start if new_value < clip.timeline_start => {
                Some((track_index, new_value, clip.timeline_start))
            }
            vv_core::TrimEdge::End if new_value > clip.timeline_end() => {
                Some((track_index, clip.timeline_end(), new_value))
            }
            _ => None,
        };
        let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
        vv_core::make_room_for_ranges(
            &mut app.project,
            timeline_id,
            range.as_slice(),
            &[(track_index, clip_id)],
            &mut commands,
        );
        commands.push(Box::new(vv_core::TrimClip::new(
            timeline_id,
            track_index,
            clip_id,
            edge,
            new_value,
        )));
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::TrimClips, commands)),
        );
    }

    /// Copia/incolla di una clip conformata: la stessa durata di timeline,
    /// e il tratto liberato per lei (`make_room_for_ranges`) è quello che
    /// occuperà davvero.
    #[test]
    fn pasting_a_conformed_clip_keeps_its_timeline_duration() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(30, 1),
            vv_core::Rational::new(30_000, 1001),
            3000,
        );
        let timeline_id = app.timeline_id.unwrap();
        let meta = app.project.media_pool[media_id].meta.clone();
        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);
        app.copy_selected_clips();
        app.timeline_state.playhead = 5000;
        app.paste_clipboard_at_playhead();

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        let pasted = clips.iter().find(|c| c.timeline_start == 5000).unwrap();
        assert_eq!(pasted.rate, vv_core::Rational::new(1001, 1000));
        assert_eq!(pasted.timeline_len, 3003);
    }

    /// Incollare su una timeline a un altro fps conserva i secondi, e la
    /// clip si conforma al nuovo fps.
    #[test]
    fn pasting_into_a_timeline_at_another_fps_keeps_the_duration_in_seconds() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(30, 1),
            vv_core::Rational::new(30, 1),
            300,
        );
        let timeline_id = app.timeline_id.unwrap();
        let meta = app.project.media_pool[media_id].meta.clone();
        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;
        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);
        app.copy_selected_clips();

        let other = app.project.timelines.insert(vv_core::Timeline {
            name: "T25".into(),
            fps: vv_core::Rational::new(25, 1),
            resolution: (320, 240),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        app.timeline_id = Some(other);
        app.timeline_state.playhead = 50;
        app.paste_clipboard_at_playhead();

        let pasted = &app.project.timelines[other].tracks[0].clips[0];
        assert_eq!(pasted.timeline_start, 50);
        assert_eq!(pasted.timeline_len, 250, "10 secondi a 25 fps");
        assert_eq!(pasted.rate, vv_core::Rational::new(5, 6));
        assert_eq!((pasted.source_in(), pasted.source_out()), (0, 300));
    }

    /// Overwrite di una clip conformata (incollare sopra la sua coda):
    /// il taglio deve cadere dove cade davvero sulla timeline, non a
    /// `source_in + delta` (frame sorgente contati come di timeline).
    #[test]
    fn overwriting_the_tail_of_a_conformed_clip_trims_it_at_the_right_spot() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(30, 1),
            vv_core::Rational::new(30_000, 1001),
            3000,
        );
        let timeline_id = app.timeline_id.unwrap();
        let meta = app.project.media_pool[media_id].meta.clone();
        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );

        let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
        vv_core::make_room_for_ranges(
            &mut app.project,
            timeline_id,
            &[(0, 2000, 4000)],
            &[],
            &mut commands,
        );
        for command in commands {
            app.history.do_command(&mut app.project, command);
        }

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(
            clips[0].timeline_end(),
            2000,
            "accorciata esattamente fino al tratto liberato"
        );
        assert_eq!(clips[0].source_out(), 1998, "2000 frame di timeline a 29,97");
    }

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
        let _audio_a = make_timeline_with_clip(&mut app, 1, 0, 10);
        let audio_b = make_timeline_with_clip(&mut app, 1, 10, 10);
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
        // Arrivando lì copre per intero quella che ci stava: vince chi
        // arriva (vedi `cut_remaining_overlaps`), niente clip accatastate.
        assert_eq!(tl.tracks[1].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips[0].id, audio_b);
        assert_eq!(tl.tracks[1].clips[0].timeline_start, 0);
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

    /// Spostare una clip sopra un'altra la sovrascrive, come incollarcela
    /// o allungarci un bordo sopra: è la stessa regola per tutti i modi di
    /// piazzare una clip (`make_room_for_ranges`).
    #[test]
    fn moving_a_clip_onto_another_cuts_the_one_underneath() {
        let mut app = VibeVideoApp::default();
        let target = make_timeline_with_clip(&mut app, 0, 0, 20);
        let moved = make_timeline_with_clip(&mut app, 1, 0, 10);
        let timeline_id = app.timeline_id.unwrap();

        let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
        vv_core::make_room_for_ranges(
            &mut app.project,
            timeline_id,
            &[(0, 10, 20)],
            &[(1, moved), (0, moved)],
            &mut commands,
        );
        commands.push(Box::new(vv_core::MoveClips::new(
            timeline_id,
            vec![(moved, 1, 0, 10)],
        )));
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::MoveClips, commands)),
        );

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].id, target);
        assert_eq!(clips[0].timeline_end(), 10, "tagliata dove arriva l'altra");
        assert_eq!(clips[1].id, moved);
        assert_eq!(clips[1].timeline_start, 10);
    }

    /// Due clip *non* collegate che coprono lo stesso tratto su track
    /// diverse (il caso che nasce dividendo al playhead dopo aver
    /// sovrascritto solo la parte video): quel tratto va chiuso una volta
    /// sola, non una per clip — altrimenti il resto arretra del doppio e
    /// finisce sopra a quel che c'era prima.
    #[test]
    fn ripple_delete_of_two_unlinked_clips_on_the_same_range_closes_it_once() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_mid = make_timeline_with_clip(&mut app, 0, 10, 10);
        let video_last = make_timeline_with_clip(&mut app, 0, 20, 10);
        make_timeline_with_clip(&mut app, 1, 0, 10);
        let audio_mid = make_timeline_with_clip(&mut app, 1, 10, 10);
        let audio_last = make_timeline_with_clip(&mut app, 1, 20, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = BTreeSet::from([(0, video_mid), (1, audio_mid)]);
        app.ripple_delete_selected();

        let tl = &app.project.timelines[timeline_id];
        for track in 0..2 {
            assert_eq!(tl.tracks[track].clips.len(), 2);
            assert_eq!(tl.tracks[track].clips[0].timeline_start, 0);
            assert_eq!(
                tl.tracks[track].clips[1].timeline_start, 10,
                "arretrate di 10, non di 20"
            );
        }
        assert_eq!(tl.tracks[0].clips[1].id, video_last);
        assert_eq!(tl.tracks[1].clips[1].id, audio_last);
    }

    /// Se uno spostamento in blocco lascia comunque una sovrapposizione,
    /// vince chi arriva: la clip sotto viene tagliata dove comincia
    /// l'altra, non lasciata accatastata.
    #[test]
    fn ripple_delete_cuts_a_clip_the_shift_landed_on() {
        let mut app = VibeVideoApp::default();
        let video = make_timeline_with_clip(&mut app, 0, 0, 10);
        let audio_long = make_timeline_with_clip(&mut app, 1, 0, 30);
        let audio_late = make_timeline_with_clip(&mut app, 1, 30, 10);
        let timeline_id = app.timeline_id.unwrap();

        // Togliendo la clip video [0,10) tutto arretra di 10: l'audio
        // lungo resta dov'è (comincia a 0) e quello dopo gli finisce
        // sopra, da 20 invece che da 30.
        app.timeline_state.selected = BTreeSet::from([(0, video)]);
        app.ripple_delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert!(tl.tracks[0].clips.is_empty());
        assert_eq!(tl.tracks[1].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[0].id, audio_long);
        assert_eq!(
            tl.tracks[1].clips[0].timeline_end(),
            20,
            "tagliata dove comincia quella arrivata sopra"
        );
        assert_eq!(tl.tracks[1].clips[1].id, audio_late);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 20);
    }

    /// Dopo un ripple delete la testina si sposta dove è appena arrivata
    /// la clip che ha chiuso il buco, così il prossimo play riparte dal
    /// punto di giunzione invece che da dove stava prima.
    #[test]
    fn ripple_delete_selected_moves_playhead_to_the_clip_that_slid_back() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let b = make_timeline_with_clip(&mut app, 0, 10, 10);
        let c = make_timeline_with_clip(&mut app, 0, 20, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.playhead = 25;
        app.timeline_state.selected = BTreeSet::from([(0, b)]);
        app.ripple_delete_selected();

        assert_eq!(app.timeline_state.playhead, 10);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips[1].id, c);
        assert_eq!(tl.tracks[0].clips[1].timeline_start, 10);
    }

    /// Senza nessuna clip che scivoli indietro (si è tolta l'ultima) non
    /// c'è nessun punto di giunzione: la testina non va spostata nel vuoto.
    #[test]
    fn ripple_delete_selected_keeps_the_playhead_when_nothing_slides_back() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let b = make_timeline_with_clip(&mut app, 0, 10, 10);

        app.timeline_state.playhead = 5;
        app.timeline_state.selected = BTreeSet::from([(0, b)]);
        app.ripple_delete_selected();

        assert_eq!(app.timeline_state.playhead, 5);
    }

    /// Stessa regola per il ripple delete di un vuoto selezionato.
    #[test]
    fn ripple_delete_of_a_gap_moves_playhead_to_the_closed_gap() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let c = make_timeline_with_clip(&mut app, 0, 20, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.playhead = 0;
        app.timeline_state.selected_gap = Some((0, 10, 20));
        app.ripple_delete_selected();

        assert_eq!(app.timeline_state.playhead, 10);
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips[1].id, c);
        assert_eq!(tl.tracks[0].clips[1].timeline_start, 10);
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
        vv_media::test_support::ffmpeg(
            &[
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
            ],
            &path,
        );

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let timeline_id = app.timeline_id.expect("import doveva creare la timeline");
        let media_id = app
            .project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound.is_none())
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
            Box::new(vv_core::set_clip_gain(
                timeline_id,
                track_index,
                clip_id,
                db,
            )),
        );
    }

    /// Il diamante di keyframe deve stare sempre alla stessa distanza dal
    /// bordo della riga, che le frecce di navigazione ci siano o no
    /// (altrimenti balla a ogni spostamento della testina).
    #[test]
    fn keyframe_arrow_reserves_the_same_width_when_there_is_nowhere_to_go() {
        let ctx = egui::Context::default();
        let width_with = arrow_row_width(&ctx, Some(10));
        let width_without = arrow_row_width(&ctx, None);
        assert_eq!(width_with, width_without);
    }

    fn arrow_row_width(ctx: &egui::Context, target: Option<FrameIdx>) -> f32 {
        let mut width = 0.0;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    let before = ui.cursor().min.x;
                    keyframe_arrow(ui, "◀", target, "test");
                    width = ui.cursor().min.x - before;
                });
            });
        });
        output.textures_delta.clear();
        width
    }

    #[test]
    fn keyframe_arrows_ignore_keyframes_outside_the_clip() {
        let mut app = VibeVideoApp::default();
        let id = make_timeline_with_clip(&mut app, 0, 0, 100);
        let timeline_id = app.timeline_id.unwrap();
        let clip = &mut app.project.timelines[timeline_id].tracks[0].clips[0];
        clip.source_offset = 50;
        clip.timeline_len = 50;
        let zoom = clip.effects.transform.track_mut(vv_core::TransformParam::ZoomX);
        zoom.upsert(10, 2.0, vv_core::Interpolation::Linear);
        zoom.upsert(40, 1.0, vv_core::Interpolation::Linear);
        let target = PanelTarget {
            timeline: timeline_id,
            track_index: 0,
            clip_id: id,
            source_frame: 70,
            timeline_start: 0,
            is_solid_color: true,
            is_text: false,
        };
        let info = app.clip_panel_info(target).unwrap();
        assert_eq!(info.params[vv_core::TransformParam::ZoomX.index()].prev, None);
    }

    /// Selezione multipla: il pannello costruisce un comando per clip e li
    /// applica come uno solo, così l'undo le riporta indietro insieme.
    #[test]
    fn effect_changes_on_several_clips_are_one_undo_step() {
        let mut app = VibeVideoApp::default();
        let first = make_timeline_with_clip(&mut app, 0, 0, 20);
        let second = make_timeline_with_clip(&mut app, 0, 30, 20);
        let timeline_id = app.timeline_id.unwrap();

        let commands: Vec<Box<dyn vv_core::Command>> = [first, second]
            .into_iter()
            .map(|clip_id| {
                set_transform_param_default((timeline_id, 0, clip_id), vv_core::TransformParam::PositionX, 120.0)
            })
            .collect();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::Transform, commands)),
        );

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert!(
            clips
                .iter()
                .all(|c| c.effects.transform.value_at(0).position == [120.0, 0.0]),
            "la modifica va a tutte le clip selezionate"
        );

        app.history.undo(&mut app.project);
        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert!(
            clips
                .iter()
                .all(|c| c.effects.transform.value_at(0).position == [0.0, 0.0]),
            "un solo undo le riporta indietro tutte"
        );
    }

    #[test]
    fn editing_one_param_on_several_clips_keeps_their_other_values_and_moves_position_by_delta() {
        use vv_core::TransformParam as P;
        let mut app = VibeVideoApp::default();
        let first = make_timeline_with_clip(&mut app, 0, 0, 20);
        let second = make_timeline_with_clip(&mut app, 0, 30, 20);
        let timeline_id = app.timeline_id.unwrap();
        let target = |clip_id, timeline_start| PanelTarget {
            timeline: timeline_id,
            track_index: 0,
            clip_id,
            source_frame: 0,
            timeline_start,
            is_solid_color: false,
            is_text: false,
        };
        let targets = [target(first, 0), target(second, 30)];
        let cmd = set_transform_param_default((timeline_id, 0, second), P::PositionX, 50.0);
        app.history.do_command(&mut app.project, cmd);
        // Il secondo ha Y animata: la modifica va in un keyframe.
        let cmd = upsert_transform_keyframe((timeline_id, 0, second), 10, P::PositionY, 5.0);
        app.history.do_command(&mut app.project, cmd);

        let before = app.project.timelines[timeline_id].tracks[0].clips[0]
            .effects
            .transform
            .value_at(0);
        let mut after = before;
        after.position[1] = 80.0;
        let mut pending = Vec::new();
        push_param_changes(
            &mut pending,
            Some(&app.project.timelines[timeline_id]),
            &targets,
            &[P::PositionX, P::PositionY],
            &after,
            &before,
        );
        app.apply_effect_changes(pending, false);

        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips[0].effects.transform.value_at(0).position, [0.0, 80.0]);
        assert_eq!(clips[1].effects.transform.value_at(0).position, [50.0, 85.0]);
        assert_eq!(clips[1].effects.transform.value_at(10).position, [50.0, 5.0]);

        // Spostata di nuovo con coordinate diverse: stesso incremento a tutte.
        let before = clips[0].effects.transform.value_at(0);
        let mut after = before;
        after.position[0] += 10.0;
        let mut pending = Vec::new();
        push_param_changes(
            &mut pending,
            Some(&app.project.timelines[timeline_id]),
            &targets,
            &[P::PositionX, P::PositionY],
            &after,
            &before,
        );
        app.apply_effect_changes(pending, false);
        let clips = &app.project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips[0].effects.transform.value_at(0).position, [10.0, 80.0]);
        assert_eq!(clips[1].effects.transform.value_at(0).position, [60.0, 85.0]);
    }

    #[test]
    fn dragging_a_value_is_a_single_undo_step() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();
        let set_x = |v| {
            vec![set_transform_param_default((timeline_id, 0, clip_id), vv_core::TransformParam::PositionX, v)]
        };
        let x = |app: &VibeVideoApp| {
            app.project.timelines[timeline_id].tracks[0].clips[0]
                .effects
                .transform
                .value_at(0)
                .position[0]
        };

        for v in [1.0, 2.0, 3.0] {
            app.apply_effect_changes(set_x(v), true);
        }
        app.apply_effect_changes(set_x(4.0), false);
        assert_eq!(x(&app), 4.0);

        app.history.undo(&mut app.project);
        assert_eq!(x(&app), 0.0, "un solo undo per tutto il trascinamento");

        app.history.redo(&mut app.project);
        app.apply_effect_changes(set_x(9.0), false);
        app.history.undo(&mut app.project);
        assert_eq!(x(&app), 4.0, "senza trascinamento ogni modifica è a sé");
    }

    #[test]
    fn effect_command_upsert_gain_keyframe_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        let cmd = upsert_gain_keyframe((timeline_id, 0, clip_id), 5, -9.0);
        app.history.do_command(&mut app.project, cmd);

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(
            clip.effects.gain_db.keyframe_at(5),
            Some((-9.0, vv_core::Interpolation::Linear))
        );
    }

    #[test]
    fn effect_command_remove_transform_keyframe_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        app.history.do_command(
            &mut app.project,
            upsert_transform_keyframe((timeline_id, 0, clip_id), 3, vv_core::TransformParam::ZoomX, 2.5),
        );
        app.history.do_command(
            &mut app.project,
            remove_transform_keyframe((timeline_id, 0, clip_id), 3, vv_core::TransformParam::ZoomX),
        );

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(clip.effects.transform.is_constant());
    }

    #[test]
    fn effect_command_set_defaults_applies_correctly() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        let timeline_id = app.timeline_id.unwrap();

        app.history.do_command(
            &mut app.project,
            set_gain_default((timeline_id, 0, clip_id), -3.0),
        );
        app.history.do_command(
            &mut app.project,
            set_transform_param_default((timeline_id, 0, clip_id), vv_core::TransformParam::ZoomX, 1.5),
        );

        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert_eq!(clip.effects.gain_db.default, -3.0);
        assert_eq!(clip.effects.transform.value_at(0).zoom, [1.5, 1.0]);
    }

    #[test]
    fn dropping_solid_color_creates_timeline_and_initialized_color() {
        let mut app = VibeVideoApp::default();
        assert!(app.timeline_id.is_none());

        app.add_generator_to_timeline_at(
            timeline_ui::Generator::SolidColor,
            0,
            timeline_ui::MediaDropTarget::Default,
        );

        let timeline_id = app.timeline_id.expect("doveva crearsi una timeline");
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(matches!(clip.source, vv_core::ClipSource::SolidColor));
        assert!(clip.effects.color.is_some());
        assert_eq!(clip.timeline_len, 125); // 5s a 25fps di default
    }

    #[test]
    fn dropping_solid_color_on_new_video_track_places_it_at_the_drop_frame() {
        let mut app = VibeVideoApp::default();
        let timeline_id = app.ensure_timeline();
        let tracks_before = app.project.timelines[timeline_id].tracks.len();

        app.add_generator_to_timeline_at(
            timeline_ui::Generator::SolidColor,
            50,
            timeline_ui::MediaDropTarget::NewVideoTrack,
        );

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks.len(), tracks_before + 1);
        let clip = &tl.tracks[tracks_before].clips[0];
        assert!(matches!(clip.source, vv_core::ClipSource::SolidColor));
        assert_eq!(clip.timeline_start, 50);
        // Track nuova e clip: un solo Ctrl+Z.
        app.history.undo(&mut app.project);
        assert_eq!(app.project.timelines[timeline_id].tracks.len(), tracks_before);
    }

    #[test]
    fn dropping_solid_color_on_a_track_overwrites_what_is_under_it() {
        let mut app = VibeVideoApp::default();
        let timeline_id = app.ensure_timeline();
        let drop = |app: &mut VibeVideoApp, start| {
            app.add_generator_to_timeline_at(
                timeline_ui::Generator::SolidColor,
                start,
                timeline_ui::MediaDropTarget::Track(0),
            )
        };
        drop(&mut app, 0); // [0, 125)
        drop(&mut app, 50); // [50, 175): taglia la coda della prima

        let spans: Vec<(FrameIdx, FrameIdx)> = app.project.timelines[timeline_id].tracks[0]
            .clips
            .iter()
            .map(|c| (c.timeline_start, c.timeline_end()))
            .collect();
        assert_eq!(spans, vec![(0, 50), (50, 175)]);

        app.history.undo(&mut app.project);
        assert_eq!(app.project.timelines[timeline_id].tracks[0].clips[0].timeline_end(), 125);
    }

    #[test]
    fn dropping_text_creates_a_text_clip_with_default_title() {
        let mut app = VibeVideoApp::default();
        app.add_generator_to_timeline_at(
            timeline_ui::Generator::Text,
            10,
            timeline_ui::MediaDropTarget::Default,
        );
        let timeline_id = app.timeline_id.unwrap();
        let clip = &app.project.timelines[timeline_id].tracks[0].clips[0];
        assert!(matches!(clip.source, vv_core::ClipSource::Text));
        assert_eq!(clip.timeline_start, 10);
        assert_eq!(clip.effects.title, Some(vv_core::TitleParams::default()));
    }

    #[test]
    fn title_edit_only_carries_the_changed_fields_to_other_clips() {
        let before = vv_core::TitleParams::default();
        let after = vv_core::TitleParams {
            size: 40.0,
            ..before.clone()
        };
        let other = vv_core::TitleParams {
            content: "Altro".into(),
            ..before.clone()
        };
        let merged = apply_title_edit(&other, &before, &after);
        assert_eq!(merged.content, "Altro");
        assert_eq!(merged.size, 40.0);

        // Dentro l'ombra, solo il campo toccato.
        let mut after = before.clone();
        after.shadow.blur = 30.0;
        let mut other = before.clone();
        other.shadow.opacity = 10.0;
        let merged = apply_title_edit(&other, &before, &after);
        assert_eq!((merged.shadow.blur, merged.shadow.opacity), (30.0, 10.0));
    }

    #[test]
    fn solid_color_clip_under_the_playhead_becomes_the_active_clip() {
        let mut app = VibeVideoApp::default();
        app.add_generator_to_timeline_at(
            timeline_ui::Generator::SolidColor,
            0,
            timeline_ui::MediaDropTarget::Default,
        );
        let timeline_id = app.timeline_id.unwrap();
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.ensure_active_clip_matches_playhead(false);

        assert_eq!(app.active_clip, Some((0, clip_id)));
    }

    #[test]
    fn effect_command_color_upsert_and_remove_round_trip() {
        let mut app = VibeVideoApp::default();
        app.add_generator_to_timeline_at(
            timeline_ui::Generator::SolidColor,
            0,
            timeline_ui::MediaDropTarget::Default,
        );
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
            upsert_color_keyframe((timeline_id, 0, clip_id), 10, red),
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
            remove_color_keyframe((timeline_id, 0, clip_id), 10),
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
        vv_media::test_support::ffmpeg(
            &[
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
            ],
            &path,
        );

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let media_id = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).unwrap().0;
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
            Box::new(vv_core::MoveClips::new(timeline_id, vec![(audio_clip, audio_track, audio_track, 50)])),
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
            Box::new(vv_core::MoveClips::new(timeline_id, vec![(audio_clip, audio_track, audio_track, 55)])),
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

    fn lock_track(app: &mut VibeVideoApp, track_index: usize) {
        let timeline_id = app.timeline_id.unwrap();
        app.history.do_command(
            &mut app.project,
            Box::new(vv_core::SetTrackFlag::new(
                timeline_id,
                track_index,
                vv_core::TrackFlag::Locked,
                true,
            )),
        );
    }

    #[test]
    fn locked_tracks_are_not_split_selected_or_pasted_on() {
        let mut app = VibeVideoApp::default();
        let video_id = make_timeline_with_clip(&mut app, 0, 0, 20);
        make_timeline_with_clip(&mut app, 1, 0, 20);
        let timeline_id = app.timeline_id.unwrap();
        lock_track(&mut app, 1);

        app.timeline_state.playhead = 8;
        app.split_at_playhead();
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips.len(), 1, "track bloccata intatta");

        app.select_all_clips();
        assert!(app.timeline_state.selected.iter().all(|&(t, _)| t == 0));

        app.timeline_state.clipboard = vec![timeline_ui::ClipboardEntry {
            track_kind: TrackKind::Audio,
            track_number: 1,
            relative_start: 0,
            clip: app.project.timelines[timeline_id].tracks[0].clips[0].clone(),
            timeline_fps: vv_core::Rational::new(25, 1),
            link_tag: None,
        }];
        app.timeline_state.playhead = 40;
        app.paste_clipboard_at_playhead();
        assert_eq!(app.project.timelines[timeline_id].tracks[1].clips.len(), 1);

        app.timeline_state
            .set_selection(BTreeSet::from([(0, video_id)]), Some((0, video_id)));
        app.ripple_delete_selected();
        assert_eq!(
            app.project.timelines[timeline_id].tracks[1].clips[0].timeline_start,
            0,
            "il ripple non sposta la track bloccata"
        );
    }

    #[test]
    fn d_disables_the_selection_and_enables_it_again() {
        let mut app = VibeVideoApp::default();
        let a = make_timeline_with_clip(&mut app, 0, 0, 20);
        let b = make_timeline_with_clip(&mut app, 0, 20, 20);
        let timeline_id = app.timeline_id.unwrap();
        let disabled = |app: &VibeVideoApp| -> Vec<bool> {
            app.project.timelines[timeline_id].tracks[0]
                .clips
                .iter()
                .map(|c| c.disabled)
                .collect()
        };

        app.timeline_state
            .set_selection(BTreeSet::from([(0, a)]), Some((0, a)));
        app.toggle_disabled_selected();
        assert_eq!(disabled(&app), vec![true, false]);

        // Selezione mista: si disattiva tutto.
        app.timeline_state
            .set_selection(BTreeSet::from([(0, a), (0, b)]), Some((0, a)));
        app.toggle_disabled_selected();
        assert_eq!(disabled(&app), vec![true, true]);

        app.toggle_disabled_selected();
        assert_eq!(disabled(&app), vec![false, false]);
    }

    #[test]
    fn dropping_media_skips_locked_tracks() {
        let (mut app, media_id) = app_with_media_at(
            vv_core::Rational::new(25, 1),
            vv_core::Rational::new(25, 1),
            50,
        );
        app.add_media_to_timeline(media_id);
        let timeline_id = app.timeline_id.unwrap();
        let video = app.project.timelines[timeline_id]
            .first_track_index(TrackKind::Video)
            .unwrap();
        lock_track(&mut app, video);

        let meta = app.project.media_pool[media_id].meta.clone();
        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );
        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[video].clips.len(), 1, "niente sulla track bloccata");
        let new_video = tl.first_unlocked_track_index(TrackKind::Video).unwrap();
        assert_ne!(new_video, video);
        assert_eq!(tl.tracks[new_video].clips.len(), 1);
    }

    #[test]
    fn select_all_clips_takes_every_track() {
        let mut app = VibeVideoApp::default();
        let a = make_timeline_with_clip(&mut app, 0, 0, 20);
        let b = make_timeline_with_clip(&mut app, 1, 30, 20);
        app.select_all_clips();
        assert_eq!(
            app.timeline_state.selected,
            BTreeSet::from([(0, a), (1, b)])
        );
    }

    #[test]
    fn select_clips_from_playhead_skips_the_ones_that_already_ended() {
        let mut app = VibeVideoApp::default();
        let before = make_timeline_with_clip(&mut app, 0, 0, 20);
        let under = make_timeline_with_clip(&mut app, 1, 20, 20);
        let after = make_timeline_with_clip(&mut app, 0, 50, 20);
        app.timeline_state.playhead = 25;
        app.select_clips_from_playhead();
        assert_eq!(
            app.timeline_state.selected,
            BTreeSet::from([(0, after), (1, under)])
        );
        assert!(!app.timeline_state.selected.contains(&(0, before)));
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
        let clip = vv_core::Clip::from_source_range(
            ClipId(0),
            vv_core::ClipSource::SolidColor,
            100,
            150,
            20,
            vv_core::Rational::one(),
        );

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
        assert_eq!(pasted.timeline_len, 10);
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

    #[test]
    fn cut_copies_the_selected_clips_and_removes_them() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        app.timeline_state.selected = BTreeSet::from([(0, clip_id)]);

        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.handle_clipboard_events(ui, &[egui::Event::Cut]);
        });
        output.textures_delta.clear();

        assert_eq!(app.timeline_state.clipboard.len(), 1);
        let timeline_id = app.timeline_id.unwrap();
        assert!(app.project.timelines[timeline_id].tracks[0].clips.is_empty());
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
    /// visivamente, ma l'anteprima continuava a riprodurre quella
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
        vv_media::test_support::ffmpeg(
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

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let media_id = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).unwrap().0;
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

    /// La striscia "buffered" non deve saltare le compound clip: i loro
    /// frame in cache sono quelli dei media della timeline annidata.
    #[test]
    fn buffered_timeline_ranges_covers_a_compound_clip_through_its_nested_timeline() {
        let dir = std::env::temp_dir().join("vv-app-buffered-ranges-compound-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        vv_media::test_support::ffmpeg(
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

        let mut app = VibeVideoApp::default();
        app.import_media(path);
        let media_id = app
            .project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound.is_none())
            .unwrap()
            .0;
        app.add_media_to_timeline(media_id);
        let timeline_id = app.timeline_id.unwrap();

        // La clip importata diventa il contenuto di una compound clip, e
        // in timeline resta solo quest'ultima.
        let inner = app.project.timelines[timeline_id].tracks[0].clips.remove(0);
        let len = inner.timeline_len;
        let nested_id = app.project.timelines.insert(vv_core::Timeline {
            name: "Nested".into(),
            fps: app.project.timelines[timeline_id].fps,
            resolution: (320, 240),
            tracks: vec![Track::new(TrackKind::Video)],
        });
        app.project.timelines[nested_id].tracks[0].insert_sorted(inner);
        let compound = insert_compound_media(&mut app, nested_id);
        app.project.media_pool[compound].meta.duration_frames = len;
        let compound_clip = vv_core::Clip::from_source_range(
            app.project.alloc_clip_id(),
            vv_core::ClipSource::Media(compound),
            0,
            len,
            0,
            vv_core::Rational::one(),
        );
        let (start, end) = (compound_clip.timeline_start, compound_clip.timeline_end());
        app.project.timelines[timeline_id].tracks[0].insert_sorted(compound_clip);
        app.sync_render_ahead();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ranges = app.buffered_timeline_ranges();
            if !ranges.is_empty() {
                for (s, e) in ranges {
                    assert!(
                        s >= start && e < end,
                        "range {s}..{e} fuori dalla compound ({start}..{end})"
                    );
                }
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timeout: la compound clip non risulta mai bufferizzata"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
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
        vv_media::test_support::ffmpeg(
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
        let media_id = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).unwrap().0;
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

    #[test]
    fn import_media_files_imports_every_file_and_queues_their_proxies() {
        let dir = std::env::temp_dir().join("vv-app-multi-import-test");
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<PathBuf> = ["a.mp4", "b.mp4"]
            .iter()
            .map(|name| {
                let path = dir.join(name);
                vv_media::test_support::ffmpeg(
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
                path
            })
            .collect();

        let mut app = VibeVideoApp::default();
        let mut with_bad = paths.clone();
        with_bad.push(dir.join("inesistente.mp4"));
        app.import_media_files(with_bad);

        // +1: la timeline del progetto compare anche lei nel pool.
        assert_eq!(app.project.media_pool.values().filter(|m| m.compound.is_none()).count(), 2);
        let warnings = &app.import_warnings;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("inesistente.mp4"), "{warnings:?}");
        let worker = app.proxy_worker.as_ref().unwrap();
        assert_eq!(worker.progress().total, 2);
        for item in app.project.media_pool.values().filter(|m| m.compound.is_none()) {
            assert!(worker.state(item.content_hash).is_some());
            assert!(app.thumbnails.contains_key(&item.content_hash));
        }
    }

    /// Controparte minima di `egui::DroppedFile` per simulare un
    /// trascinamento dal file manager senza un vero backend
    /// windowing — solo `path()` serve a `poll_dropped_files`.
    #[derive(Debug)]
    struct TestDroppedFile(PathBuf);
    impl egui::DroppedFile for TestDroppedFile {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Err("non serve in questo test".into())
        }
    }

    /// Drag & drop dal file manager: un file rilasciato sulla finestra
    /// (`i.raw.dropped_files`) si importa nel media pool esattamente come
    /// dal file dialog.
    #[test]
    fn dropping_a_file_from_the_file_manager_imports_it_into_the_pool() {
        let path = make_wav("dropped.wav");
        let mut app = VibeVideoApp::default();

        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.dropped_files = vec![std::sync::Arc::new(TestDroppedFile(path.clone()))];
        let mut output = ctx.run_ui(input, |ui| app.poll_dropped_files(ui.ctx()));
        output.textures_delta.clear();

        assert!(app.import_warnings.is_empty(), "{:?}", app.import_warnings);
        // +1: la timeline del progetto, creata al volo dall'import (vedi
        // `ensure_timeline_for`), compare anche lei nel pool.
        assert_eq!(app.project.media_pool.values().filter(|m| m.compound.is_none()).count(), 1);
        let item = app.project.media_pool.values().find(|m| m.compound.is_none()).unwrap();
        assert_eq!(item.path, path);
    }

    /// L'export mette in pausa la generazione dei proxy (che altrimenti
    /// gli contende CPU e ffmpeg, tenendolo fermo) e la riprende alla
    /// fine — ma non riprende una pausa scelta dall'utente.
    #[test]
    fn export_pauses_the_proxy_queue_and_resumes_it_afterwards() {
        let mut app = VibeVideoApp::default();
        app.proxy_worker = Some(proxy_worker::ProxyWorker::spawn());

        app.pause_proxies_for_export();
        assert!(app.proxy_worker.as_ref().unwrap().is_paused());
        app.resume_proxies_after_export();
        assert!(!app.proxy_worker.as_ref().unwrap().is_paused());

        app.proxy_worker.as_ref().unwrap().set_paused(true);
        app.pause_proxies_for_export();
        app.resume_proxies_after_export();
        assert!(
            app.proxy_worker.as_ref().unwrap().is_paused(),
            "una pausa dell'utente non va annullata dalla fine dell'export"
        );
    }

    fn browse_fixture(name: &str) -> (VibeVideoApp, MediaId) {
        let dir = std::env::temp_dir().join("vv-app-browse-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=25:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ],
            &path,
        );
        let mut app = VibeVideoApp::default();
        let media_id = app.add_media_to_pool(path).unwrap();
        app.preview_media(media_id);
        app.browsing_media = Some(media_id);
        (app, media_id)
    }

    #[test]
    fn space_plays_the_media_pool_preview_instead_of_leaving_it() {
        let (mut app, media_id) = browse_fixture("space.mp4");
        app.toggle_playback();
        assert_eq!(app.browsing_media, Some(media_id));
        assert!(app.is_timeline_playing());

        std::thread::sleep(std::time::Duration::from_millis(200));
        app.drive_browse_playback();
        assert!(app.browse_playhead > 0, "la testina dell'anteprima doveva avanzare");

        app.toggle_playback();
        assert!(!app.is_timeline_playing());
    }

    #[test]
    fn preview_plays_the_media_audio_and_leaving_it_restores_the_timeline_mix() {
        let (mut app, _) = browse_fixture("audio.mp4");
        let fps = app.browse_fps();
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
        let peak = |app: &VibeVideoApp| {
            app.timeline_audio
                .as_ref()
                .unwrap()
                .render(10, fps, 5)
                .iter()
                .fold(0.0f32, |m, s| m.max(s.abs()))
        };
        assert!(peak(&app) > 0.1, "l'anteprima deve suonare");

        app.stop_browsing();
        app.sync_timeline_audio();
        assert_eq!(peak(&app), 0.0, "fuori dall'anteprima torna il mix della timeline, qui vuota");
    }

    #[test]
    fn in_out_marks_on_the_preview_trim_the_clip_dropped_on_the_timeline() {
        let (mut app, media_id) = browse_fixture("marks.mp4");
        app.seek_browse(10);
        app.mark_at_playhead(true);
        app.seek_browse(29);
        app.mark_at_playhead(false);
        let (source_in, source_out) = app.browse_marks.resolve(app.browse_total_frames());
        assert_eq!((source_in, source_out), (10, 30));

        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag {
                media_id,
                source_in,
                source_out,
                streams: timeline_ui::DragStreams::All,
            },
            5,
            timeline_ui::MediaDropTarget::Default,
        );
        let timeline = &app.project.timelines[app.timeline_id.unwrap()];
        let clips: Vec<_> = timeline.tracks.iter().flat_map(|t| &t.clips).collect();
        assert_eq!(clips.len(), 2, "video + audio");
        for clip in clips {
            assert_eq!((clip.source_in(), clip.source_out(), clip.timeline_start), (10, 30, 5));
        }
    }

    #[test]
    fn video_only_and_audio_only_drags_insert_just_that_stream() {
        for (streams, kind) in [
            (timeline_ui::DragStreams::VideoOnly, TrackKind::Video),
            (timeline_ui::DragStreams::AudioOnly, TrackKind::Audio),
        ] {
            let (mut app, media_id) = browse_fixture("streams.mp4");
            let meta = app.project.media_pool[media_id].meta.clone();
            app.add_media_to_timeline_at(
                timeline_ui::MediaDrag {
                    streams,
                    ..timeline_ui::MediaDrag::whole(media_id, &meta)
                },
                0,
                timeline_ui::MediaDropTarget::Default,
            );
            let timeline = &app.project.timelines[app.timeline_id.unwrap()];
            let kinds: Vec<TrackKind> = timeline
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter().map(|_| t.kind))
                .collect();
            assert_eq!(kinds, vec![kind], "{streams:?}");
        }
    }

    #[test]
    fn in_out_keys_on_the_timeline_set_the_export_range() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 100);
        app.timeline_state.playhead = 20;
        app.mark_at_playhead(true);
        app.timeline_state.playhead = 59;
        app.mark_at_playhead(false);
        assert_eq!(app.timeline_state.export_marks.resolve(100), (20, 60));
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
            vv_media::test_support::ffmpeg(
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("testsrc=size=320x240:rate=25:duration={duration}"),
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                ],
                &path,
            );
        }

        let mut app = VibeVideoApp::default();
        app.import_media(path_a);
        app.import_media(path_b);
        let mut media_ids = app.project.media_pool.iter().filter(|(_, item)| item.compound.is_none()).map(|(id, _)| id);
        let media_a = media_ids.next().unwrap();
        let media_b = media_ids.next().unwrap();
        drop(media_ids);

        let whole = |app: &VibeVideoApp, id| {
            timeline_ui::MediaDrag::whole(id, &app.project.media_pool[id].meta)
        };
        app.add_media_to_timeline_at(whole(&app, media_a), 0, timeline_ui::MediaDropTarget::Default); // [0,50)
        app.add_media_to_timeline_at(whole(&app, media_b), 50, timeline_ui::MediaDropTarget::Default); // [50,75), adiacente
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

    #[test]
    fn export_otio_to_writes_the_file_and_reports_failures() {
        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let timeline_id = app.timeline_id.unwrap();
        let dir = std::env::temp_dir().join("vv-app-otio-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("timeline.otio");

        app.export_otio_to(timeline_id, &path);
        assert!(app.project_error.is_none());
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"Timeline.1\""));

        app.export_otio_to(timeline_id, &dir.join("nope/timeline.otio"));
        assert!(app.project_error.is_some());
    }

    #[test]
    fn import_otio_from_replaces_the_project_and_reports_skipped_clips() {
        let dir = std::env::temp_dir().join("vv-app-otio-import-test");
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("clip.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            &media,
        );
        let range = |start: f64, duration: f64| {
            serde_json::json!({
                "OTIO_SCHEMA": "TimeRange.1",
                "start_time": { "OTIO_SCHEMA": "RationalTime.1", "value": start, "rate": 25.0 },
                "duration": { "OTIO_SCHEMA": "RationalTime.1", "value": duration, "rate": 25.0 },
            })
        };
        let clip = |url: &str| {
            serde_json::json!({
                "OTIO_SCHEMA": "Clip.1",
                "name": url,
                "source_range": range(5.0, 20.0),
                "media_reference": { "OTIO_SCHEMA": "ExternalReference.1", "target_url": url },
            })
        };
        let otio = serde_json::json!({
            "OTIO_SCHEMA": "Timeline.1",
            "name": "Importata",
            "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [{
                "OTIO_SCHEMA": "Track.1",
                "kind": "Video",
                "children": [clip("clip.mp4"), clip("sparito.mp4")],
            }]},
        });
        let otio_path = dir.join("timeline.otio");
        std::fs::write(&otio_path, otio.to_string()).unwrap();

        let mut app = VibeVideoApp::default();
        app.current_project_path = Some(dir.join("vecchio.vvproj"));
        app.import_otio_from(&otio_path);

        assert!(app.project_error.is_none(), "{:?}", app.project_error);
        assert_eq!(app.current_project_path, None);
        let timeline = &app.project.timelines[app.timeline_id.unwrap()];
        assert_eq!(timeline.name, "Importata");
        let clips = &timeline.tracks[0].clips;
        assert_eq!(clips.len(), 1);
        let clip = &clips[0];
        assert_eq!((clip.timeline_start, clip.timeline_len, clip.source_in()), (0, 20, 5));
        let warnings = &app.import_warnings;
        assert!(warnings.iter().any(|w| w.contains("sparito.mp4")), "{warnings:?}");
    }

    #[test]
    fn unsaved_changes_follow_edits_and_saves() {
        let mut app = VibeVideoApp::default();
        assert!(!app.has_unsaved_changes(), "progetto vuoto");
        make_timeline_with_clip(&mut app, 0, 0, 10);
        assert!(app.has_unsaved_changes());

        let dir = std::env::temp_dir().join("vv-app-unsaved-test");
        std::fs::create_dir_all(&dir).unwrap();
        app.save_project_to(&dir.join("p.vvproj"));
        assert!(!app.has_unsaved_changes());

        app.unsaved_media = true;
        assert!(app.has_unsaved_changes(), "media importato dopo il salvataggio");
    }

    /// Con modifiche non salvate l'apertura aspetta la risposta; "Annulla"
    /// e un salvataggio fallito lasciano il progetto com'è.
    #[test]
    fn switching_project_with_unsaved_changes_waits_and_keeps_the_project_on_failure() {
        let mut app = VibeVideoApp::default();
        let clip_id = make_timeline_with_clip(&mut app, 0, 0, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.request_project_switch(ProjectSwitch::ImportOtio);
        assert_eq!(app.pending_project_switch, Some(ProjectSwitch::ImportOtio));
        app.resolve_unsaved_changes(UnsavedChoice::Cancel);
        assert_eq!(app.pending_project_switch, None);

        app.current_project_path = Some(std::env::temp_dir().join("vv-app-nope/dir/p.vvproj"));
        app.request_project_switch(ProjectSwitch::Open);
        app.resolve_unsaved_changes(UnsavedChoice::Save);
        assert!(app.project_error.is_some(), "salvataggio fallito");
        assert!(app.has_unsaved_changes());
        assert_eq!(app.project.timelines[timeline_id].tracks[0].clips[0].id, clip_id);
    }

    /// La cache delle waveform può mancare (cancellata, altra macchina):
    /// aprire il progetto la rigenera.
    #[test]
    fn opening_a_project_regenerates_missing_waveforms() {
        let dir = std::env::temp_dir().join("vv-app-waveform-on-load-test");
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("tono.mp4");
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=25:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ],
            &media,
        );

        // Hash mai visto: nessuna waveform in cache per questo media.
        let content_hash = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let mut project = vv_core::Project::default();
        project.media_pool.insert(vv_core::MediaItem {
            path: media.clone(),
            meta: vv_media::probe(&media).unwrap(),
            content_hash,
            compound: None,
        });
        let project_path = dir.join("p.vvproj");
        vv_core::save_project(&project, &project_path).unwrap();
        assert!(!vv_media::waveform::waveform_exists(content_hash, 0));

        let mut app = VibeVideoApp::default();
        app.load_project_from(project_path);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !vv_media::waveform::waveform_exists(content_hash, 0) {
            assert!(std::time::Instant::now() < deadline, "waveform non generata");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(vv_media::waveform::waveform_path_for(content_hash, 0));
    }

    fn make_wav(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("vv-app-audio-only-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=2",
            ],
            &path,
        );
        path
    }

    /// Un media solo audio entra nel pool senza proxy né miniatura, e in
    /// timeline diventa solo una clip audio (creando la track se manca).
    #[test]
    fn an_audio_only_file_imports_and_drops_as_an_audio_clip() {
        let path = make_wav("tono.wav");
        let mut app = VibeVideoApp::default();
        app.import_media_files(vec![path]);
        assert!(app.import_warnings.is_empty(), "{:?}", app.import_warnings);
        let (media_id, item) = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).unwrap();
        assert!(!item.meta.has_video);
        assert!(app.proxy_worker.is_none(), "niente proxy per un media solo audio");
        assert!(app.thumbnails.is_empty());

        let timeline_id = app.timeline_id.unwrap();
        let timeline = &app.project.timelines[timeline_id];
        assert_eq!(timeline.resolution, (1920, 1080), "timeline di default");
        assert!(
            timeline.tracks.iter().all(|t| t.kind == TrackKind::Audio),
            "un import solo audio non deve creare nessuna track video, nemmeno vuota"
        );
        {
            let timeline = &mut app.project.timelines[timeline_id];
            timeline.tracks.clear();
        }

        let meta = item.meta.clone();
        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            10,
            timeline_ui::MediaDropTarget::Default,
        );
        let timeline = &app.project.timelines[timeline_id];
        assert!(
            timeline.tracks.iter().all(|t| t.kind == TrackKind::Audio),
            "il drop non deve aver creato nessuna track video"
        );
        let (_, audio) = timeline.tracks_of_kind(TrackKind::Audio).next().expect("track creata");
        assert_eq!(audio.clips.len(), 1);
        let clip = &audio.clips[0];
        assert_eq!(clip.timeline_start, 10);
        assert_eq!(clip.linked_group, None);
        assert_eq!(clip.timeline_len, 50, "2 s a 25 fps");
    }

    fn make_png(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("vv-app-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        vv_media::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:size=640x360:rate=1:duration=1",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &path,
        );
        path
    }

    /// Un'immagine entra nel pool come media video senza audio, senza
    /// proxy, con `duration_frames` il sentinel di
    /// `vv_core::IMAGE_DURATION_FRAMES` — e trascinata "intera" sulla
    /// timeline produce una clip da 5s di default (accorciabile/
    /// allungabile come una clip qualunque), non una lunga quanto il
    /// sentinel.
    #[test]
    fn an_image_file_imports_and_drops_as_a_five_second_clip_without_audio() {
        let path = make_png("still.png");
        let mut app = VibeVideoApp::default();
        app.import_media_files(vec![path.clone()]);
        assert!(app.import_warnings.is_empty(), "{:?}", app.import_warnings);

        let (media_id, item) = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).unwrap();
        assert!(item.meta.is_image());
        assert!(item.meta.has_video);
        assert!(!item.meta.has_audio);
        assert_eq!((item.meta.width, item.meta.height), (640, 360));
        assert_eq!(item.meta.duration_frames, vv_core::IMAGE_DURATION_FRAMES);
        assert!(app.proxy_worker.is_none(), "niente proxy per un'immagine");

        let timeline_id = app.timeline_id.expect("un'immagine crea la timeline come un video");
        let meta = item.meta.clone();
        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );
        let timeline = &app.project.timelines[timeline_id];
        let (_, video) = timeline.tracks_of_kind(TrackKind::Video).next().expect("track creata");
        assert_eq!(video.clips.len(), 1);
        let clip = &video.clips[0];
        assert_eq!(clip.timeline_len, 5 * 25, "5 s di default a 25 fps");
        assert!(
            timeline.tracks.iter().all(|t| t.kind == TrackKind::Video || t.clips.is_empty()),
            "un'immagine non ha audio: nessuna clip audio deve comparire"
        );
    }

    /// Regressione: trascinare un media solo audio sulla timeline non
    /// deve mai creare (né riusare da vuota) una track video — nemmeno
    /// la prima volta, quando è anche lui a far nascere la timeline.
    #[test]
    fn dropping_audio_only_media_creates_no_video_track() {
        let mut app = VibeVideoApp::default();
        let media_id = app.project.media_pool.insert(vv_core::MediaItem {
            path: "/tmp/vv-audio-only.wav".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 50,
                fps: vv_media::AUDIO_ONLY_FPS,
                width: 0,
                height: 0,
                has_video: false,
                has_audio: true,
                sample_rate: 48000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 1,
            compound: None,
        });
        let meta = app.project.media_pool[media_id].meta.clone();

        app.add_media_to_timeline_at(
            timeline_ui::MediaDrag::whole(media_id, &meta),
            0,
            timeline_ui::MediaDropTarget::Default,
        );

        let timeline_id = app.timeline_id.expect("il drop crea la timeline al volo");
        let timeline = &app.project.timelines[timeline_id];
        assert!(
            timeline.tracks.iter().all(|t| t.kind == TrackKind::Audio),
            "niente track video per un drop di solo audio su un progetto vuoto: {:?}",
            timeline.tracks.iter().map(|t| t.kind).collect::<Vec<_>>()
        );
    }

    fn close_request_commands(app: &mut VibeVideoApp) -> Vec<egui::ViewportCommand> {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ui| app.handle_close_request(ui.ctx()));
        output.textures_delta.clear();
        output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default()
    }

    /// Chiudere la finestra con modifiche non salvate si ferma sul dialog;
    /// senza modifiche, o dopo "Non salvare", si esce.
    #[test]
    fn closing_the_window_asks_to_save_unsaved_changes() {
        let mut app = VibeVideoApp::default();
        let commands = close_request_commands(&mut app);
        assert!(!commands.contains(&egui::ViewportCommand::CancelClose), "niente da salvare");

        let mut app = VibeVideoApp::default();
        make_timeline_with_clip(&mut app, 0, 0, 10);
        let commands = close_request_commands(&mut app);
        assert!(commands.contains(&egui::ViewportCommand::CancelClose));
        assert_eq!(app.pending_project_switch, Some(ProjectSwitch::Quit));

        app.resolve_unsaved_changes(UnsavedChoice::Cancel);
        assert!(!app.quit_confirmed);

        close_request_commands(&mut app);
        app.resolve_unsaved_changes(UnsavedChoice::Discard);
        assert!(app.quit_confirmed);
        let commands = close_request_commands(&mut app);
        assert!(commands.contains(&egui::ViewportCommand::Close));
        assert!(!commands.contains(&egui::ViewportCommand::CancelClose));
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

        vv_media::test_support::ffmpeg(
            &[
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
            ],
            &path,
        );

        let mut app = VibeVideoApp::default();
        app.import_media(path.clone());
        let media_id = app.project.media_pool.iter().find(|(_, item)| item.compound.is_none()).unwrap().0;
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
