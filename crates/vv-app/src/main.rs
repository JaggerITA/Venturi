//! egui window: media pool, viewer, properties panel, timeline. The
//! viewer composes on the GPU the active clips of all the video tracks and passes
//! the texture to egui-wgpu without readback (plans/REFACTOR_PIPELINE.md B2).

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
mod new_timeline_dialog;
mod paste_attributes;
mod project_io;
mod properties_panel;
mod proxy_worker;
mod render_ahead;
mod settings;
mod settings_dialog;
mod import_worker;
mod thumbnail_worker;
mod timeline_audio;
mod timeline_ui;
mod transport;
mod viewer_overlay;
mod viewer_zoom;
mod waveform_worker;
#[cfg(target_os = "linux")]
mod wayland_dnd;
mod worker;

use eframe::wgpu;
use media_pool_ui::*;
use new_timeline_dialog::NewTimelineDialog;
use paste_attributes::PasteAttributesDialog;
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

/// ~6 s of margin at 1080p, ~1.5 s at 4K.
const DEFAULT_CACHE_BUDGET_BYTES: usize = 1_200_000_000;

/// Floor of the shrunk lookahead: below this playback stutters anyway.
const MIN_LOOKAHEAD_SECS: f64 = 0.5;

/// Color of a freshly created Solid Color clip.
const DEFAULT_SOLID_COLOR: vv_core::Rgba = vv_core::Rgba { r: 1.0, g: 1.0, b: 0.0, a: 1.0 };

/// After how long (s) an arrow held down stops doing the single
/// step and starts scrolling at `ARROW_HOLD_SPEED`.
const ARROW_HOLD_DELAY_SECS: f64 = 0.3;
const ARROW_HOLD_SPEED: f64 = 0.5;

/// Left/right arrow held down (`VenturiApp::arrow_hold`).
struct ArrowHold {
    direction: FrameIdx,
    pressed_at: f64,
    start_frame: FrameIdx,
}

/// Playhead position with an arrow held for `elapsed` seconds:
/// one frame immediately, then continuous scrolling after `ARROW_HOLD_DELAY_SECS`.
fn arrow_hold_target(hold: &ArrowHold, elapsed: f64, fps: f64) -> FrameIdx {
    let scrolled = ((elapsed - ARROW_HOLD_DELAY_SECS).max(0.0) * fps * ARROW_HOLD_SPEED) as FrameIdx;
    (hold.start_frame + hold.direction * (1 + scrolled)).max(0)
}

/// The two fast playback speeds reachable with the "a" key
/// (see `VenturiApp::handle_fast_playback_key`) — not a free `f64`:
/// only these two factors are ever requested of `vv_audio::stretch_samples`.
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

    /// The tier reached by pressing "a" one more time relative to this one —
    /// see `VenturiApp::handle_fast_playback_key`. It stays at X8 past that.
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

/// A selected clip, target of the properties panel. `source_frame`
/// is the playhead in the source space of *this* clip.
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

/// Tab of the properties panel: the parameters of a video clip, those
/// of its audio part, or the list of everything that is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertiesTab {
    Video,
    Audio,
    Selection,
}

/// Sub-tab of the Video tab for text clips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoSubTab {
    Title,
    Settings,
}

/// System fonts for the Title panel, read the first time they are needed.
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
    /// Native resolution of the clip's media and resolution of the
    /// timeline: the units of the pixel parameters of the `Transform` (the crop
    /// in the former, position and anchor in the latter).
    source_size: (u32, u32),
    timeline_size: (u32, u32),
    /// Keyframe state of every parameter, indexed by
    /// `TransformParam::index`.
    params: Vec<RowKeyframe>,
    transform: vv_core::Transform,
    gain_kf_here: bool,
    gain: f32,
    /// Nearest gain keyframes before/after, in source frames: the navigation
    /// arrows of the Volume row.
    gain_prev: Option<FrameIdx>,
    gain_next: Option<FrameIdx>,
    color_kf_here: bool,
    color: vv_core::Rgba,
    title: Option<vv_core::TitleParams>,
    /// Empty until a filter is dragged onto the clip from the Effects
    /// panel; then one per filter, in order of application.
    filters: Vec<vv_core::ClipFilter>,
    blend_mode: vv_core::BlendMode,
}

/// Where the clips of a drop from the media pool land, resolved once for
/// the whole drop (see `resolve_drop_tracks`): `extra_audio` is the audio
/// track created on the fly, which takes precedence over the existing ones.
#[derive(Debug, Clone, Copy)]
struct DropTracks {
    /// `None` if there is no video in the drop: no video track to create.
    video: Option<usize>,
    extra_audio: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewerFrameKind {
    Video,
    /// The clip under the playhead points at a media no longer in the media pool.
    Offline,
}

struct VenturiApp {
    project: vv_core::Project,
    history: vv_core::History,
    timeline_id: Option<TimelineId>,
    /// The timelines "above" `timeline_id`, from the root down, when one has
    /// entered a compound clip with a double click (see
    /// `enter_compound_timeline`): empty when editing the
    /// project timeline. The breadcrumb above the timeline shows them.
    timeline_stack: Vec<TimelineId>,
    timeline_state: timeline_ui::TimelineState,
    media_pool_state: media_pool::MediaPoolState,
    keyframe_editor: keyframe_editor::KeyframeEditorState,
    /// Media or elements not imported, shown in a separate window
    /// until the user closes it.
    import_warnings: Vec<String>,
    /// Probing of the files of a multiple import, in progress in the background.
    pending_import: Option<project_io::PendingImport>,
    /// Files chosen while an import was already in progress: they start afterwards.
    import_queue: Vec<PathBuf>,

    preview_meta: Option<vv_core::MediaMeta>,
    preview_error: Option<String>,
    /// Viewer texture registered in egui-wgpu once, then
    /// updated: registering a new one on every frame would lose the previous one.
    /// `None` without a shared device (tests).
    video_texture_id: Option<egui::TextureId>,
    /// Frame size in pixels at 100% zoom (the timeline resolution), not
    /// the texture's: that can be smaller, see `fit_output_size`.
    video_display_size: Option<egui::Vec2>,
    /// Layers and output of the texture's last composition: an identical
    /// one is skipped instead of re-uploading every plane on each repaint.
    viewer_content: Option<(Vec<frame_provider::OwnedLayer>, vv_render::OutputFrame)>,
    viewer_zoom: viewer_zoom::ViewerZoom,
    /// Player box and frame size of the last draw, for the zoom shortcuts.
    viewer_geometry: Option<(egui::Rect, egui::Vec2)>,
    /// What the viewer shows: the last composition, even if in this
    /// frame no new one was made (no empty flash).
    last_viewer_frame_kind: Option<ViewerFrameKind>,
    /// egui-wgpu device/queue, shared with the compositor: a texture from
    /// another device cannot be registered in egui. `None` in the tests.
    egui_render_state: Option<eframe::egui_wgpu::RenderState>,
    /// Video buffer of the media pool preview: a `RenderAhead` on a
    /// timeline with only the clip of the media, and the id of the media in there.
    browsing_render_ahead: Option<(render_ahead::RenderAhead, MediaId)>,

    /// Created on the first import; thrown away when the user turns the proxies off.
    proxy_worker: Option<proxy_worker::ProxyWorker>,
    /// The export paused the proxies and must resume them; `false` if the
    /// pause was the user's.
    proxy_paused_for_export: bool,
    /// Generates in the background the waveforms of the media with audio. Created on the first
    /// import.
    waveform_worker: Option<waveform_worker::WaveformWorker>,
    thumbnail_worker: Option<thumbnail_worker::ThumbnailWorker>,
    /// Media pool thumbnails per `content_hash`; `None` = request in
    /// progress or failed (avoids requeuing it on every frame).
    thumbnails: HashMap<u64, Option<egui::TextureHandle>>,
    /// Waveforms read from the cache files, per `(content_hash, stream_index)`:
    /// the timeline draws them on every frame, re-reading them from disk it does not.
    waveform_cache: HashMap<(u64, usize), vv_media::Waveform>,
    /// Waveforms looked for on disk and not found: retried only when the
    /// worker reports them ready, not on every frame.
    waveform_missing: std::collections::HashSet<(u64, usize)>,
    /// Compound clips whose waveform was composed with sources
    /// not ready yet: to be redone when they arrive.
    waveform_partial: std::collections::HashSet<(u64, usize)>,

    /// Video clip shown in the viewer (the one under the playhead), from which
    /// frames and transform are read. `None` on a gap or during the preview
    /// from the media pool.
    active_clip: Option<(usize, ClipId)>,
    compositor: vv_render::Compositor,
    /// Opened on the first change made with the pointer down, closed on
    /// release: a drag is a single undo step.
    edit_drag_group: Option<vv_core::GroupMark>,

    /// Last handled playhead: it tells the one moved by the clock (no seek)
    /// from the one moved by the user.
    last_synced_playhead: FrameIdx,

    /// Media previewed from the pool: the viewer shows it instead of the timeline,
    /// until the timeline is interacted with.
    browsing_media: Option<MediaId>,
    browse_playhead: FrameIdx,
    /// In/out of the preview: the portion dragged from the viewer onto the timeline.
    browse_marks: transport::MarkRange,
    browse_audio_streams: usize,

    /// Video buffer of the timeline. `None` until there is a timeline.
    render_ahead: Option<render_ahead::RenderAhead>,
    /// `history.generation()` at the last update of `render_ahead`.
    render_ahead_generation: u64,
    /// `history.generation()` at the last `sync_root_timeline_media`.
    root_timeline_media_generation: u64,

    /// Mixer of the audio tracks and playback clock of the timeline.
    /// `None` until needed (in the tests it opens only if used).
    timeline_audio: Option<TimelineAudio>,

    /// Speed multiplier (1/2/4/8x) set by the "a" key; the
    /// space bar pauses and brings it back to 1x.
    playback_speed: f64,

    /// Moving the playhead, cutting or deleting selects the video clip
    /// under the playhead.
    selection_follows_playhead: bool,

    /// Audio during scrubbing (menu Timeline): on by default.
    scrub_audio: bool,

    arrow_hold: Option<ArrowHold>,

    /// Snapping: in drags the clips snap to the nearby edges.
    snapping_enabled: bool,
    /// Transform handles over the viewer (button under the viewer).
    show_transform_overlay: bool,
    overlay_drag: Option<viewer_overlay::OverlayDrag>,

    /// X and Y zoom of the Transform panel kept together (the lock between the
    /// two fields): a UI preference, not a project one.
    zoom_link: bool,

    /// Tab open in the properties panel.
    properties_tab: PropertiesTab,
    video_subtab: VideoSubTab,
    fonts: FontCatalog,

    export: Option<ExportUiState>,
    export_dialog: Option<export_dialog::ExportDialog>,
    new_timeline_dialog: Option<NewTimelineDialog>,
    paste_attributes: Option<PasteAttributesDialog>,
    /// Attributes ticked in the last "paste attributes": proposed again the
    /// next time, as in the other NLEs.
    paste_attributes_selection: std::collections::HashSet<paste_attributes::Attribute>,
    paste_attributes_keyframe_mode: paste_attributes::KeyframeMode,
    /// Proposed again at the next export of the session.
    last_export_settings: Option<export::ExportSettings>,

    /// `None` until saved: "Save" behaves like "Save as".
    current_project_path: Option<PathBuf>,
    /// Last title sent to the compositor: the command is sent only when it changes.
    window_title: String,
    about_icon: Option<egui::TextureHandle>,
    /// `history.generation()` at the last save or open.
    saved_generation: u64,
    /// Media imported after the last save: the media pool changes
    /// without going through the history.
    unsaved_media: bool,
    /// Open, import or exit waiting for the answer to "save the
    /// changes?".
    pending_project_switch: Option<ProjectSwitch>,
    /// The user already answered for the exit: the next close request
    /// goes through.
    quit_confirmed: bool,
    /// Last project save/open error, shown in the
    /// toolbar next to the buttons — separate from `import_warnings` (those are
    /// for the media import, a different context).
    project_error: Option<String>,
    /// Outcome of the last relink from the media pool context menu,
    /// shown in a separate small window (see `show_relink_message`).
    relink_message: Option<String>,
    /// File dialog opened on a separate thread: on the GNOME/Wayland event
    /// loop thread it marks the app as unresponsive.
    pending_dialog: Option<PendingDialog>,

    /// Audio meter (toggle in View): a narrow band on the right
    /// of the timeline with the level of the outgoing audio. On by
    /// default, as in most NLEs.
    audiometer_enabled: bool,
    /// Meter levels, with a decay: the instantaneous peak would make
    /// the bars drop abruptly.
    audiometer_level: (f32, f32),
    viewer_fullscreen: bool,
    settings: settings::Settings,
    /// `None` in the tests: the settings are never written to disk.
    settings_path: Option<PathBuf>,
    settings_dialog: Option<settings_dialog::SettingsDialog>,
    about_open: bool,
    #[cfg(target_os = "linux")]
    wayland_dnd: Option<wayland_dnd::WaylandDnd>,
    #[cfg(target_os = "macos")]
    iso_key_down: bool,
}

impl Default for VenturiApp {
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
            pending_import: None,
            import_queue: Vec::new(),
            preview_meta: None,
            preview_error: None,
            video_texture_id: None,
            video_display_size: None,
            viewer_content: None,
            viewer_zoom: viewer_zoom::ViewerZoom::default(),
            viewer_geometry: None,
            last_viewer_frame_kind: None,
            egui_render_state: None,
            browsing_render_ahead: None,
            proxy_worker: None,
            proxy_paused_for_export: false,
            waveform_worker: None,
            thumbnail_worker: None,
            thumbnails: HashMap::new(),
            waveform_cache: HashMap::new(),
            waveform_missing: Default::default(),
            waveform_partial: Default::default(),
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
            new_timeline_dialog: None,
            paste_attributes: None,
            paste_attributes_selection: Default::default(),
            paste_attributes_keyframe_mode: paste_attributes::KeyframeMode::MaintainTiming,
            last_export_settings: None,
            current_project_path: None,
            window_title: String::new(),
            about_icon: None,
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
            #[cfg(target_os = "macos")]
            iso_key_down: false,
        }
    }
}

impl VenturiApp {

    /// Stereo output peak meter, with decay.
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
                // Green up to 70%, yellow up to 90%, red past that
                // (near clipping) — the same convention as a common
                // VU meter.
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

        // Repaint while the bars are falling.
        if level_l > 0.001 || level_r > 0.001 {
            ui.ctx().request_repaint();
        }
    }

    /// "Raw" preview of a media from the media pool, not tied to the
    /// timeline: it shows the first frame from a dedicated decode-ahead.
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
        // The buffer opens the file on its own and skips the ones that do not
        // open: the error must be seen here.
        if let Err(e) = vv_media::Decoder::open(&path) {
            self.preview_error = Some(e.to_string());
            return;
        }
        self.browsing_render_ahead = Some(self.spawn_browsing_render_ahead(media_id));
    }

    /// A timeline at the media fps with only it on top: the timeline frames
    /// coincide with the source ones.
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
            name: "preview".into(),
            fps: meta.fps,
            resolution: (meta.width, meta.height),
            tracks: vec![track],
        });
        let render_ahead = render_ahead::RenderAhead::spawn(
            project,
            timeline_id,
            self.settings.cache_budget_bytes,
            self.settings.proxy(),
            self.settings.lookahead_secs,
            self.settings.behind_secs,
        );
        (render_ahead, preview_media)
    }

    /// The timeline buffer and the preview one, if they exist.
    fn render_aheads(&self) -> impl Iterator<Item = &render_ahead::RenderAhead> {
        self.render_ahead
            .iter()
            .chain(self.browsing_render_ahead.as_ref().map(|(r, _)| r))
    }

    /// The active Video clip (topmost track among those having one
    /// at that point, `Timeline::active_video_clip_at`) at frame `frame`,
    /// with its track — the one the viewer shows.
    fn active_video_clip_at(&self, frame: FrameIdx) -> Option<(usize, ClipId)> {
        let timeline_id = self.timeline_id?;
        self.project.timelines[timeline_id]
            .active_video_clip_at(frame)
            .map(|(t, c)| (t, c.id))
    }

    /// With "selection follows playhead" on, it selects the video clip under
    /// the playhead and its group (nothing on a gap).
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

    /// Aligns the viewer clip to the playhead and, if the user moved it
    /// (`force_seek` even during playback), the audio clock.
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
        // The clock stayed at the preview position: force the seek to the
        // timeline playhead.
        self.last_synced_playhead = FrameIdx::MIN;
    }

    fn browse_total_frames(&self) -> FrameIdx {
        self.preview_meta.as_ref().map_or(0, |m| m.duration_frames)
    }

    fn browse_fps(&self) -> f64 {
        self.preview_meta.as_ref().map_or(1.0, |m| m.fps.as_f64().max(1e-9))
    }

    /// The mixer clock acts as the playhead for the preview too, as for the
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

    /// I/O keys: on the preview if active, otherwise on the timeline.
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

    /// Left/right arrows: one frame back/forward, holding them down
    /// scrolls at `ARROW_HOLD_SPEED`. It pauses if playing.
    /// Returns `true` while an arrow is held.
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
        let hold = self.arrow_hold.as_ref().expect("set above");
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

    /// Play/pause of the timeline from the playhead, with no need for a selection:
    /// gaps included (they play silence), stopping only past the end of the
    /// content.
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
            // The space bar always pauses at 1x: a later "a"
            // restarts at normal speed.
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

    /// During playback the playhead follows the mixer clock; at the end of the
    /// content it stops.
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

    /// "a" key: when stopped like the space bar (1x), during playback it
    /// accelerates 2x -> 4x -> 8x. Only the space bar pauses.
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

    /// 1x immediately; the fast speeds when their stretched audio
    /// is ready (see `TimelineAudio::request_speed`).
    fn request_playback_speed(&mut self, speed: f64) {
        match &mut self.timeline_audio {
            Some(audio) => {
                audio.request_speed(speed);
                self.playback_speed = audio.speed();
            }
            None => self.playback_speed = speed,
        }
    }

    /// Timeline intervals already decoded, for the "buffered" strip.
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

    /// Decoded intervals of `timeline_id`, in frames of that
    /// timeline. A compound clip has no cached frames of its own (it is not a
    /// media to decode): its intervals are those of its nested
    /// timeline, remapped through the clip.
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

    /// Timeline intervals of the Media clips served by the proxy (the whole
    /// clip, not only the part already decoded).
    fn proxy_timeline_ranges(&self) -> Vec<(FrameIdx, FrameIdx)> {
        let Some(timeline_id) = self.timeline_id else {
            return Vec::new();
        };
        if !self.settings.proxy_enabled {
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

    /// Loads into `waveform_cache` the waveforms of the audio clips on the timeline,
    /// reading the cache file once per key.
    fn ensure_waveforms_loaded(&mut self) {
        if let Some(worker) = &self.waveform_worker {
            let arrived = worker.drain_ready();
            for key in &arrived {
                self.waveform_missing.remove(key);
            }
            // The compounds composed with still missing sources must be
            // redone now that some of them have arrived.
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

    /// Waveform of a compound clip: there is no file to decode, it is
    /// composed from those of the clips of its nested timeline (see
    /// `compose_compound_waveform`), loading them first if needed.
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

    /// Like `ensure_timeline`, but a new timeline takes fps and resolution
    /// from `meta` (media with video only).
    fn ensure_timeline_for(&mut self, meta: &vv_core::MediaMeta) -> TimelineId {
        self.ensure_timeline_with(meta.fps, (meta.width, meta.height), true)
    }

    /// Like `ensure_timeline`, but without a video track: a drop of audio only must
    /// not create an empty one.
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
        // The project timeline is to all effects a timeline like
        // the others (see the docs of `MediaItem::compound`): it shows up in the media
        // pool exactly like a compound clip, draggable elsewhere.
        // Any initial `meta`, `sync_root_timeline_media` corrects it
        // immediately on the first round (called by `update`).
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

    /// New timeline of the project: like the initial one it also shows up in
    /// the media pool as a compound clip (see `ensure_timeline_with`).
    pub(crate) fn create_timeline(
        &mut self,
        name: String,
        fps: vv_core::Rational,
        resolution: (u32, u32),
    ) -> TimelineId {
        let id = self.project.timelines.insert(vv_core::Timeline {
            name: name.clone(),
            fps,
            resolution,
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        let media_id = self.project.media_pool.insert(vv_core::MediaItem {
            path: name.into(),
            meta: vv_core::MediaMeta {
                duration_frames: 0,
                fps,
                width: resolution.0,
                height: resolution.1,
                has_video: true,
                has_audio: true,
                sample_rate: 48_000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 0,
            compound: Some(id),
        });
        self.project.sync_compound_meta(media_id);
        // Creating a timeline does not go through the history (like a media
        // import): without this, Ctrl+S would not be offered.
        self.unsaved_media = true;
        id
    }

    /// Opens a timeline as the top level one: the breadcrumb of the compound
    /// clips one was inside no longer applies.
    pub(crate) fn open_timeline(&mut self, timeline_id: TimelineId) {
        if self.timeline_id == Some(timeline_id) {
            return;
        }
        self.timeline_stack.clear();
        self.switch_to_timeline(timeline_id);
    }

    /// Spawns `render_ahead` the first time a timeline exists (one
    /// per session: it is not recreated afterwards, only updated via
    /// `sync_render_ahead`/`RenderAhead::update_project`).
    fn spawn_render_ahead_if_needed(&mut self, timeline_id: TimelineId) {
        if self.render_ahead.is_none() {
            self.render_ahead = Some(render_ahead::RenderAhead::spawn(
                self.project.clone(),
                timeline_id,
                self.settings.cache_budget_bytes,
                self.settings.proxy(),
                self.settings.lookahead_secs,
                self.settings.behind_secs,
            ));
            self.render_ahead_generation = self.history.generation();
        }
    }

    /// Opens `nested_id` as if it were the project timeline: from the double
    /// click on a compound clip (`timeline_ui::show_timeline`). The whole
    /// existing editing UI stays unchanged (it is already parameterized on
    /// `self.timeline_id`), here only the "which" has to be moved and
    /// where one came from stacked for the breadcrumb.
    pub(crate) fn enter_compound_timeline(&mut self, nested_id: TimelineId) {
        let Some(current) = self.timeline_id else {
            return;
        };
        if current == nested_id || self.timeline_stack.contains(&nested_id) {
            // Already on this level, or already an ancestor on the stack: a
            // residual cycle (see `vv_core::MAX_COMPOUND_DEPTH`) must
            // not make the stack grow indefinitely.
            return;
        }
        self.timeline_stack.push(current);
        self.switch_to_timeline(nested_id);
    }

    /// Goes back to the timeline at position `index` of `timeline_stack` (0 =
    /// the root): a clicked breadcrumb segment.
    fn exit_to_timeline_stack_index(&mut self, index: usize) {
        let Some(target) = self.timeline_stack.get(index).copied() else {
            return;
        };
        self.timeline_stack.truncate(index);
        self.switch_to_timeline(target);
    }

    /// The part common to the two functions above: it clears the state
    /// belonging to the level being left (selection, playhead, active clip) and
    /// immediately wakes `render_ahead` on the new timeline, ignoring the
    /// generation gate of `sync_render_ahead` (here the timeline itself
    /// changes, not its content — `sync_render_ahead` would not
    /// notice on its own). The clipboard survives: copying from one
    /// timeline/compound clip and pasting into another must work
    /// (`paste_clipboard_at_playhead` already conforms for different fps).
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

    /// Name to show for `id` in the breadcrumb above the timeline: that
    /// of its entry in the media pool (see `MediaItem::compound`), which is
    /// what the user sees elsewhere (e.g. "Compound Clip 2").
    fn timeline_display_name(&self, id: TimelineId) -> String {
        self.project
            .media_pool
            .values()
            .find(|m| m.compound == Some(id))
            .map(|m| file_label(&m.path))
            .unwrap_or_else(|| self.project.timelines[id].name.clone())
    }

    /// Sends `render_ahead` a copy of the project only when the history has
    /// changed.
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
        // At speeds >1x the playhead advances faster in real time: more margin
        // ahead is needed, and it must still fit the budget (see the docs of
        // `effective_window_secs`).
        let (ahead_secs, behind_secs) = self.effective_window_secs(timeline_id);
        render_ahead.set_lookahead_secs(ahead_secs);
        render_ahead.set_behind_secs(behind_secs);
        render_ahead.set_target(self.timeline_state.playhead);
    }

    /// The media pool entry of the project timeline (see
    /// `ensure_timeline_with`) always reflects its real content, not
    /// just the one at creation time: duration, presence of
    /// video/audio can change on every modification.
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

    /// Prefetch window that actually fits the cache budget: `lookahead_secs`
    /// scaled by the playback speed and `behind_secs`, shrunk proportionally
    /// when the media playing together would not fit. Asking for more than the
    /// budget does not buy a longer buffer: the worker saturates in the middle
    /// of the window and leaves holes on one of the tracks, i.e. black flashes
    /// during playback.
    fn effective_window_secs(&self, timeline_id: TimelineId) -> (f64, f64) {
        let timeline = &self.project.timelines[timeline_id];
        let fps = timeline.fps.as_f64().max(1e-9);
        let wanted_ahead = self.settings.lookahead_secs * self.playback_speed;
        let playhead = self.timeline_state.playhead;
        let from = (playhead - (self.settings.behind_secs * fps).ceil() as FrameIdx).max(0);
        let to = playhead + (wanted_ahead * fps).ceil() as FrameIdx;
        let frame_bytes = self.window_frame_bytes(timeline_id, from, to).max(1);
        // Headroom: the transit frames decoded before a segment sit in the
        // cache until the next reconcile.
        let affordable =
            self.settings.cache_budget_bytes as f64 * 0.85 / (frame_bytes as f64 * fps);
        let wanted_total = wanted_ahead + self.settings.behind_secs;
        if wanted_total <= affordable {
            return (wanted_ahead, self.settings.behind_secs);
        }
        let scale = affordable / wanted_total;
        (
            (wanted_ahead * scale).max(MIN_LOOKAHEAD_SECS),
            self.settings.behind_secs * scale,
        )
    }

    /// Bytes one frame of each distinct media playing in `[from, to)` takes in
    /// the cache. Source resolution, not the timeline one: the cache holds
    /// decoded frames, before any scaling.
    fn window_frame_bytes(&self, timeline_id: TimelineId, from: FrameIdx, to: FrameIdx) -> usize {
        let timeline = &self.project.timelines[timeline_id];
        let mut seen = std::collections::HashSet::new();
        let mut bytes = 0;
        for (_, track) in timeline.tracks_of_kind(TrackKind::Video) {
            if track.muted {
                continue;
            }
            for clip in track.clips.iter().filter(|c| !c.disabled) {
                let vv_core::ClipSource::Media(media_id) = clip.source else {
                    continue;
                };
                if clip.timeline_end() <= from || clip.timeline_start >= to {
                    continue;
                }
                if !seen.insert(media_id) {
                    continue;
                }
                if let Some(item) = self.project.media_pool.get(media_id) {
                    bytes += vv_media::yuv420_frame_bytes(item.meta.width, item.meta.height);
                }
            }
        }
        bytes
    }

    /// Drop of an effect from the Effects panel: the generator clip goes on the
    /// video track indicated by `target`, at `start`.
    fn add_generator_to_timeline_at(
        &mut self,
        generator: timeline_ui::Generator,
        start: FrameIdx,
        target: timeline_ui::MediaDropTarget,
    ) {
        let timeline_id = self.ensure_timeline();
        let group = self.history.begin_group();
        if let Some(tracks) = self.resolve_drop_tracks(timeline_id, target, true, false) {
            // A generator always has video: `resolve_drop_tracks` with
            // `any_video: true` always resolves to `Some`.
            let video_track = tracks.video.expect("generator: video track always resolved");
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

    /// The initial color is mid grey, editable right away from the properties
    /// panel once selected.
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

    /// As in a real NLE, the clips already present under the new ones are
    /// shortened, split or removed instead of staying overlapped.
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

    /// Composes `layers` and shows the resulting texture in the viewer, without
    /// readback. It does nothing without a shared device (tests).
    fn show_composited(&mut self, layers: Vec<frame_provider::OwnedLayer>, output: vv_render::OutputFrame) {
        let unchanged = self.video_texture_id.is_some()
            && self
                .viewer_content
                .as_ref()
                .is_some_and(|(shown, shown_output)| {
                    *shown_output == output && frame_provider::renders_same(shown, &layers)
                });
        if unchanged {
            self.last_viewer_frame_kind = Some(ViewerFrameKind::Video);
            return;
        }
        let Some(render_state) = self.egui_render_state.clone() else {
            return;
        };
        let render_layers: Vec<vv_render::Layer> =
            layers.iter().map(frame_provider::OwnedLayer::as_render).collect();
        let texture = self.compositor.render_layers_to_texture(&render_layers, output);
        drop(render_layers);
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
        let (width, height) = output.timeline_size;
        self.video_display_size = Some(egui::vec2(width as f32, height as f32));
        self.last_viewer_frame_kind = Some(ViewerFrameKind::Video);
        // Not kept with a compound texture: never reusable, and holding it
        // would keep it out of the scratch pool.
        let reusable = !layers
            .iter()
            .any(|l| matches!(l, frame_provider::OwnedLayer::Texture { .. }));
        self.viewer_content = reusable.then_some((layers, output));
    }

    /// The layers to compose at the playhead, from bottom to top. `None` if a
    /// media frame is not ready: the frame already shown is kept, composing
    /// without that layer would flash black where the clip should be. Once the
    /// buffer says it is caught up the missing frame will never arrive (media
    /// that does not decode): the layer is dropped instead of freezing the
    /// preview. During a crossing transition both halves must be ready.
    fn timeline_video_layers(&mut self) -> Option<Vec<frame_provider::OwnedLayer>> {
        let timeline = &self.project.timelines[self.timeline_id?];
        let still_filling = self.render_ahead.as_ref().is_some_and(|r| !r.is_caught_up());
        let render_ahead = self.render_ahead.as_mut()?;
        let playhead = self.timeline_state.playhead;
        let clips = timeline.active_video_clips_at(playhead);
        let mut layers = Vec::with_capacity(clips.len());
        for &(track_index, clip) in &clips {
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
            if track_layers.len() < involved {
                if std::env::var("VV_DEBUG_RENDER_AHEAD").is_ok()
                    && let Some((media_id, source_frame)) =
                        frame_provider::media_source_frame(clip, frame)
                {
                    eprintln!(
                        "[viewer] MISSING-LAYER playhead={playhead} track={track_index} media={media_id:?} source_frame={source_frame} caught_up={} cached_ranges={:?}",
                        !still_filling,
                        render_ahead.cached_ranges_for(media_id),
                    );
                }
                if still_filling {
                    return None;
                }
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

    /// The frame of the media pool preview at its playhead, if already
    /// decoded.
    fn browsing_video_frame(&mut self) -> Option<std::sync::Arc<vv_media::FrameYuv420>> {
        let (render_ahead, media_id) = self.browsing_render_ahead.as_ref()?;
        render_ahead.set_target(self.browse_playhead);
        render_ahead.get_frame(*media_id, self.browse_playhead)
    }

    /// Appends the media to the video track (and the audio clips alongside).
    #[cfg(test)]
    fn add_media_to_timeline(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let meta = item.meta.clone();
        // If a timeline does not exist yet (e.g. first drag&drop from the media
        // pool), it is created on the fly inheriting fps/resolution from this
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

    /// Like `add_media_to_timeline`, but places the clip at `start` (position
    /// and `target` from the drag&drop onto the timeline, see `MediaDropTarget`).
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

    /// Drop of one or more media: appended from `start` in pool order. The
    /// new tracks are created once for the whole drop.
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
        // fps and resolution from the first *video* media of the set: an audio at the
        // head has no resolution to give the timeline.
        let timeline_id = match drops.iter().find(|(d, meta)| d.takes_video(meta)) {
            Some((_, video_meta)) => self.ensure_timeline_for(video_meta),
            None => self.ensure_timeline_audio_only(),
        };
        // A drop that would close a cycle (a timeline imported inside
        // itself, directly or through one of its compound clips) is
        // discarded instead of corrupting the project — see
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
        // One drop = one Ctrl+Z, even if inside there are N clips (one
        // per audio stream of each media) plus the tracks created on the fly.
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

    /// Creates the tracks required by `target` (once per drop) and
    /// returns where the clips will go.
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
                    // No free video track: one is created.
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

    /// Inserts the video clip and one audio clip per stream at `start`, all
    /// in the same linked group; creates the missing audio tracks.
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

        // At the head: the first stream must land on the track just
        // created for this drop, not on an already existing one.
        if let Some(extra) = tracks.extra_audio {
            audio_track_indices.retain(|i| *i != extra);
            audio_track_indices.insert(0, extra);
        }

        // Without an audio track the audio of a video is discarded, but a media or an
        // audio-only drag creates one. If they are all locked, new ones are created.
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
            // `tracks.video` is certainly `Some`: `resolve_drop_tracks`
            // resolves it only if at least one media of the drop has video, and this
            // is one of those.
            new_clips.push((
                tracks.video.expect("drop with video but no track resolved"),
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

    /// Transform handles of the first selected video clip, if it is under
    /// the playhead and the viewer shows the timeline stopped.
    fn open_settings(&mut self, section: settings_dialog::Section) {
        self.settings_dialog = Some(settings_dialog::SettingsDialog::new(section));
    }

    fn show_settings_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.settings_dialog else {
            return;
        };
        let before = self.settings.clone();
        let response = dialog.show(ctx, &mut self.settings);
        if !response.open {
            self.settings_dialog = None;
        }
        if !response.changed {
            return;
        }
        if self.settings.proxy() != before.proxy() {
            self.apply_proxy_settings();
        }
        // The timeline one is then scaled on every sync (`effective_window_secs`).
        for render_ahead in self.render_aheads() {
            render_ahead.set_lookahead_secs(self.settings.lookahead_secs);
            render_ahead.set_behind_secs(self.settings.behind_secs);
            render_ahead.set_cache_budget_bytes(self.settings.cache_budget_bytes);
        }
        self.persist_settings();
    }

    /// User settings on disk, including the panel layout: called
    /// from the Settings window and periodically/on close (see
    /// `eframe::App::save`).
    fn persist_settings(&mut self) {
        if let Some(path) = &self.settings_path
            && let Err(e) = self.settings.save(path)
        {
            self.project_error = Some(t!("settings.save_failed", error = e).into_owned());
        }
    }

    /// `(total, playhead, (in, out), playing)` of the playback bar.
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

    /// Full-screen player over the rest of the interface, which stays
    /// drawn underneath (and keeps handling the shortcuts) but no longer
    /// receives the mouse. The playback bar appears only with the mouse at the bottom.
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

    fn show_viewer_zoom_bar(&mut self, ui: &mut egui::Ui) {
        let ppp = ui.ctx().pixels_per_point();
        let geometry = self
            .viewer_geometry
            .filter(|_| self.last_viewer_frame_kind == Some(ViewerFrameKind::Video));
        let label = match geometry {
            Some((area, frame_px)) => {
                viewer_zoom::percent_label(self.viewer_zoom.scale(area, frame_px, ppp))
            }
            None => "–".to_owned(),
        };
        let keymap = &self.settings.keymap;
        let shortcut_text =
            |action| keymap.shortcuts(action).first().map(ToString::to_string).unwrap_or_default();
        ui.add_enabled_ui(geometry.is_some(), |ui| {
            ui.menu_button(format!("{label} ⏷"), |ui| {
                let Some((area, frame_px)) = geometry else {
                    return;
                };
                let fit = egui::Button::selectable(self.viewer_zoom.is_fit(), t!("viewer.zoom_fit"))
                    .shortcut_text(shortcut_text(Action::ViewerZoomFit));
                if ui.add(fit).clicked() {
                    self.viewer_zoom.fit();
                    ui.close();
                }
                ui.separator();
                let current = self.viewer_zoom.scale(area, frame_px, ppp);
                for preset in viewer_zoom::PRESETS {
                    let percent = viewer_zoom::percent_label(preset);
                    let button = if preset == 1.0 {
                        egui::Button::selectable(false, t!("viewer.zoom_actual", percent = percent))
                            .shortcut_text(shortcut_text(Action::ViewerZoomActual))
                    } else {
                        egui::Button::selectable(false, percent)
                    };
                    let selected = !self.viewer_zoom.is_fit() && (current - preset).abs() < 1e-4;
                    if ui.add(button.selected(selected)).clicked() {
                        self.viewer_zoom.set_scale(preset, area, frame_px, ppp);
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text(t!("viewer.zoom_hint"));
        });
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

    /// D: disables the selected clips, or re-enables them if they are all already disabled.
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

    /// Ctrl+A: selects all the clips of the timeline.
    fn select_all_clips(&mut self) {
        self.select_clips(|_| true);
    }

    /// Ctrl+A with the media pool focused: selects all the media of the pool.
    fn select_all_media(&mut self) {
        let ids: Vec<MediaId> = self.project.media_pool.keys().collect();
        self.media_pool_state.select_only(ids);
    }

    /// Alt+Y: selects from the playhead forward — the clip under the
    /// playhead is included, those ending earlier stay out.
    fn select_clips_from_playhead(&mut self) {
        let playhead = self.timeline_state.playhead;
        self.select_clips(|clip| clip.timeline_end() > playhead);
    }

    /// Selects the clips of the unlocked tracks for which `keep` is true.
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

    /// Del in the keyframe editor: it touches the keyframes, not the clip.
    fn delete_selected_keyframes(&mut self) {
        let commands = self.keyframe_editor.remove_selected(self.zoom_link);
        self.apply_effect_changes(commands, false);
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

    /// Del/Backspace on the media pool: removes the selected media. The clips
    /// using them stay on the timeline and become offline (see
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

    /// Deleting a compound clip deletes its nested timeline
    /// (`vv_core::RemoveMedia`): if it was being edited, one goes back up to the
    /// upper level that still exists.
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

    /// The active clip points at a media no longer in the media pool.
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

    /// Copy/cut/paste from the keyboard. After a copy it writes a placeholder
    /// into the system clipboard: egui-winit generates `Event::Paste` only if
    /// that is not empty.
    fn handle_clipboard_events(&mut self, ui: &egui::Ui, events: &[egui::Event]) {
        for event in events {
            match event {
                egui::Event::Copy => {
                    self.copy_selected_clips();
                    if !self.timeline_state.clipboard.is_empty() {
                        ui.ctx().copy_text("venturi:clip".to_owned());
                    }
                }
                // Ctrl+X in a text field cuts the text, not the clips.
                egui::Event::Cut if !ui.ctx().egui_wants_keyboard_input() => {
                    if self.timeline_state.selected.is_empty() {
                        continue;
                    }
                    self.copy_selected_clips();
                    ui.ctx().copy_text("venturi:clip".to_owned());
                    self.delete_selected();
                }
                egui::Event::Paste(_) => self.paste_clipboard_at_playhead(),
                _ => {}
            }
        }
    }

    /// Copies the selected clips (the linked groups are all already inside the
    /// selection).
    fn copy_selected_clips(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            return;
        }

        let tl = &self.project.timelines[timeline_id];
        // The groups are remapped onto local tags: the paste needs new groups.
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

    /// Pastes the clipboard at the playhead preserving the distances and the linked
    /// groups; the pasted clips overwrite what was underneath.
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
            // Pasting a copied compound clip inside its own
            // nested timeline (directly or through another
            // compound clip) would close a cycle — see
            // `Project::would_create_a_cycle`, the same check as the drop
            // from the media pool.
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

    /// Tracks missing to paste `entries`: a compound clip with video
    /// tracks only, pasted into a timeline having a single one, must
    /// create its own V2, not end up on the audio.
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

    /// Ripple delete: removes the selected clips (and their linked
    /// groups) and closes the holes on all the tracks, preserving the A/V sync.
    fn ripple_delete_selected(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.selected.is_empty() {
            // No clip selected: the selected gap is closed, if there is one.
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

        // Holes merged when they overlap: closing them once per clip
        // would make the rest go back twice as far.
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
        // From right to left: closing a hole moves what comes
        // after it, not what comes before it, so the holes still to
        // close stay where we measured them.
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

    /// After a bulk move, cuts the overlaps that may have
    /// remained (see `vv_core::cut_overlaps`): the clip starting later
    /// always wins, the one that just arrived there.
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

    /// Brings the playhead where the clip that slid in to close the hole now starts;
    /// if none arrived it stays where it is.
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

    /// Cuts at the playhead the selected clips covering it or, without a
    /// selection, all of them. The right halves of a linked group get
    /// relinked to each other.
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

        // Id of the right half pre-allocated for every target, so it can
        // be used right away for the relinking commands.
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

        // Selects the *left* half (the one under the playhead would be the
        // right one): after a cut one usually works on what comes before.
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

/// Cache intervals in source frames → timeline frames of `clip`,
/// limited to its trim.
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

/// How many levels of nested compound clips are walked (waveform,
/// "buffered" strip): past that, it gives up — like
/// `vv_core::MAX_COMPOUND_DEPTH`.
const MAX_COMPOUND_WALK_DEPTH: usize = 8;

/// Peaks of a compound clip, composed from those of the audio clips of
/// its nested timeline: there is no file to decode, so the
/// worker cannot generate them. The `bool` is `false` if some source was
/// not cached — the waveform is partial and must be redone later.
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

/// Duration of a media for the panel column: MM:SS, with the hours only
/// when there are any.
pub(crate) fn format_duration(duration_frames: vv_core::FrameIdx, fps: f64) -> String {
    let secs = (duration_frames.max(0) as f64 / fps.max(1e-9)).round() as u64;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Extensions treated as images: probing a container is not enough to
/// tell them apart.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "bmp", "webp", "tif", "tiff"];

fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| IMAGE_EXTENSIONS.iter().any(|img| img.eq_ignore_ascii_case(ext)))
}

/// Hand-drawn magnet: on some platforms (Asahi) egui's fonts
/// do not have the 🧲 glyph.
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
        // Lower arc: t=0 -> left leg, t=π -> right leg,
        // passing through the lowest point at t=π/2 (it closes the "U").
        let arc_points: Vec<egui::Pos2> = (0..=16)
            .map(|i| {
                let t = std::f32::consts::PI * (i as f32 / 16.0);
                egui::pos2(c.x - r * t.cos(), arc_center_y + r * t.sin())
            })
            .collect();
        painter.add(egui::Shape::line(arc_points, stroke));

        // Poles at the two tips, colored like a real horseshoe
        // magnet (schoolbook convention: red and grey).
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

/// Box with four corner handles and the pivot at the center.
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

/// Film strip and waveform icons at the bottom of the viewer: dragging them brings
/// only the video or only the audio onto the timeline.
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

/// On Wayland winit sends `Started` for high-resolution scrolling but
/// almost never `Ended`: egui keeps Alt held and the zoom stays stuck.
/// Treated as `Move`, every event uses the current modifiers.
fn unstick_wheel_modifiers(raw_input: &mut egui::RawInput) {
    for event in &mut raw_input.events {
        if let egui::Event::MouseWheel { phase, .. } = event
            && *phase == egui::TouchPhase::Start
        {
            *phase = egui::TouchPhase::Move;
        }
    }
}

/// winit on macOS maps the ISO "<" key to `Backquote` and never emits
/// `IntlBackslash`; egui has no key for "<", so it falls back to that physical
/// code. The typed text tells the two apart; the release carries no text, so
/// `iso_key_down` rewrites it too, otherwise egui would see the key held.
#[cfg(target_os = "macos")]
fn fix_iso_key(raw_input: &mut egui::RawInput, iso_key_down: &mut bool) {
    for i in 0..raw_input.events.len() {
        let typed_angle = matches!(
            raw_input.events.get(i + 1),
            Some(egui::Event::Text(t)) if t.starts_with(['<', '>'])
        );
        if let egui::Event::Key { key, pressed, .. } = &mut raw_input.events[i]
            && *key == egui::Key::Backtick
        {
            if *pressed && typed_angle {
                *iso_key_down = true;
            }
            if *iso_key_down {
                *key = egui::Key::IntlBackslash;
                if !*pressed {
                    *iso_key_down = false;
                }
            }
        }
    }
}

impl eframe::App for VenturiApp {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        unstick_wheel_modifiers(raw_input);
        #[cfg(target_os = "macos")]
        fix_iso_key(raw_input, &mut self.iso_key_down);
        #[cfg(target_os = "linux")]
        if let Some(dnd) = &self.wayland_dnd {
            dnd.feed(raw_input);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_close_request(&ui.ctx().clone());
        self.poll_pending_dialog(&ui.ctx().clone());
        self.poll_dropped_files(&ui.ctx().clone());
        self.poll_pending_import(&ui.ctx().clone());
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
        self.sync_window_title(ui.ctx());

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
                if let Some((done, total)) = self.import_progress() {
                    ui.separator();
                    ui.add(
                        egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                            .desired_width(180.0)
                            .text(t!("project.import_progress", done = done, total = total)),
                    );
                }
                // Above the panel it opens, like the toggles on the left.
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
        self.show_new_timeline_dialog(ui.ctx());
        if std::mem::take(&mut self.timeline_state.paste_attributes_requested) {
            self.open_paste_attributes_dialog();
        }
        self.show_paste_attributes_dialog(ui.ctx());

        // Targets of the panel: the selected clips by track kind, in order
        // (track, start). The frame of each is the playhead in its own source
        // space; the first gives the shown values, the changes go to all of them.
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
                        // Whoever got the last click decides who gets Del/Backspace.
                        if let Some(pos) = ui.ctx().input(|i| {
                            i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten()
                        }) {
                            self.media_pool_state.focused = pool.rect.contains(pos);
                        }
                        // Highlights the pool while a file is dragged from the file manager.
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
        // Without a timeline there is no scale to position the drop: the
        // timeline is created and it is appended at 0.
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
                    // Narrow band on the right, carved out *before*
                    // showing the timeline: it steals only this fixed
                    // width from it, it does not compress it proportionally.
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
                    // The buffer advances on another thread: without a repaint the
                    // "buffered" strip would not update.
                    if self.render_ahead.as_ref().is_some_and(|r| !r.is_caught_up()) {
                        ui.ctx().request_repaint();
                    }
                    // Audio peaks already in memory (loaded from the cache file
                    // the first time they are needed, see `ensure_waveforms_loaded`):
                    // the timeline draws them as waveforms on the audio clips.
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

        // A user scrub must be followed during playback too, unlike
        // the playhead moved by `drive_playback`.
        let user_scrubbed_playhead = self.timeline_state.playhead != playhead_before_timeline_ui;
        if user_scrubbed_playhead {
            self.sync_selection_to_playhead();
        }

        // Interacting with the timeline (selecting a clip or moving the
        // playhead) takes control of the viewer back from the "raw"
        // media pool preview, if active.
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
            // The selection follows the playhead moved by playback too, but only
            // if it really moved: it does not touch a selection made in this frame.
            let playhead_before_playback = self.timeline_state.playhead;
            self.drive_playback();
            if self.timeline_state.playhead != playhead_before_playback {
                self.sync_selection_to_playhead();
            }
        }
        self.drive_browse_playback();
        // The timeline buffer stays warm during the pool preview too.
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
                self.zoom_link,
            );
            pending_effects.extend(editor.commands);
            pending_playhead = editor.playhead.or(pending_playhead);
        }

        if let Some(id) = preview_action {
            self.preview_media(id);
            // Pool preview: no active clip, and the playhead must not
            // take it away on the next frame.
            self.active_clip = None;
            self.browsing_media = Some(id);
        }
        if let Some(frame) = pending_playhead {
            self.timeline_state.playhead = frame.max(0);
        }

        // A change from the panel can touch several clips (multiple
        // selection): a single composite command, so the undo brings them
        // all back together.
        // An open dropdown is applying the preview of its entries:
        // its changes go into a single undo step, like a drag.
        let holding = ui.input(|i| i.pointer.any_down()) || preview_combo_open(ui.ctx());
        self.apply_effect_changes(pending_effects, holding);

        let mut transport_action = transport::TransportResponse::default();
        let mut viewer_rect = None;
        // The whole player box, bars included: the handles near the
        // edge of the frame must stay grabbable outside too.
        let mut viewer_area = None;
        let mut overlay_effects: Vec<BoxedCommand> = Vec::new();
        egui::CentralPanel::default().show(ui, |ui| {
            // Inside the CentralPanel: it occupies only the viewer column.
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
            egui::Panel::top("viewer_zoom_bar")
                .resizable(false)
                .show(ui, |ui| self.show_viewer_zoom_bar(ui));
            let media_offline = self.active_clip_media_offline();
            // Browsing a "raw" media from the media pool there is no
            // timeline to compose: a single layer, its frame as it is.
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
                    let output = vv_render::OutputFrame::exact(frame.width, frame.height);
                    let layer = frame_provider::OwnedLayer::Video {
                        source_size: (frame.width, frame.height),
                        frame,
                        transform: vv_core::Transform::default(),
                        opacity: 1.0,
                        filters: Vec::new(),
                        blend: vv_core::BlendMode::Normal,
                    };
                    self.show_composited(vec![layer], output);
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
                // With SolidColor clips only it composes at the resolution
                // of the timeline.
                let composite_size = video_size.or(timeline_size.filter(|_| !layers.is_empty()));

                match composite_size {
                    Some(size) => {
                        // At the resolution of the decoded frame, widened to the aspect of the
                        // timeline: the bars show without upscaling.
                        let timeline_size = timeline_size.unwrap_or(size);
                        let (out_w, out_h) = vv_render::fit_output_size(size, timeline_size);
                        self.show_composited(
                            layers,
                            vv_render::OutputFrame::scaled(out_w, out_h, timeline_size),
                        );
                    }
                    // Empty: black, not the last frame left. A tiny
                    // texture with the aspect of the timeline is enough.
                    None => {
                        let (w, h) = timeline_size.unwrap_or((16, 9));
                        let step = (w.max(h) / 64).max(1);
                        self.show_composited(
                            Vec::new(),
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
                        let (area, _) =
                            ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
                        self.viewer_zoom.handle_input(ui, area, tex_size);
                        let ppp = ui.ctx().pixels_per_point();
                        let rect = self.viewer_zoom.frame_rect(area, tex_size, ppp);
                        ui.painter().with_clip_rect(area).image(
                            id,
                            rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                        self.viewer_geometry = Some((area, tex_size));
                        viewer_area = Some(area);
                        viewer_rect = Some(rect);
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
                        // It can be dragged onto the timeline even without an image.
                        if audio_only {
                            viewer_rect = Some(label.inner.rect);
                        }
                    }
                }
            }

            let Some(rect) = viewer_rect else {
                return;
            };
            let area = viewer_area.unwrap_or(rect);
            let ui = &mut self.viewer_zoom.tools_ui(ui, area);
            if viewer_area.is_some() {
                self.show_viewer_overlay(ui, rect, area, &video_targets, &mut overlay_effects);
            }

            if let Some(media_id) = self.browsing_media {
                let rect = rect.intersect(area);
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

        // Repaint during the drags, otherwise they are not smooth with the player stopped.
        if ui
            .ctx()
            .input(|i| i.pointer.any_down() || i.pointer.any_released())
        {
            ui.ctx().request_repaint();
        }
    }

    /// Called by eframe on close and periodically (see
    /// `auto_save_interval`): the panel layout updated on every frame
    /// in `ui()` thus ends up on disk without writing it on every resize.
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist_settings();
    }
}

fn app_icon() -> Option<egui::IconData> {
    const PNG: &[u8] = include_bytes!("../../../media/icons/png/vv-icon-256.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(PNG));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some(egui::IconData { rgba: buf, width: info.width, height: info.height })
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Optional argument: path of a video to import immediately at startup
    // (handy for debugging/smoke tests, as well as for command-line use).
    let startup_path = std::env::args().nth(1).map(PathBuf::from);
    std::thread::spawn(vv_render::text::warm_up);
    // The first NVENC check initializes CUDA: better not in the UI.
    std::thread::spawn(|| vv_media::VideoCodec::Nvenc.is_available());

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Venturi")
        // It must match the name of the .desktop, otherwise the Wayland
        // compositors do not associate the icon with the window.
        .with_app_id("venturi");
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "venturi",
        options,
        Box::new(move |cc| {
            // Timeline zoom with Alt+scroll instead of the default Ctrl.
            // Shift+scroll is left free for the vertical zoom of the tracks,
            // so the horizontal scroll moves to Ctrl+scroll.
            cc.egui_ctx.options_mut(|o| {
                o.input_options.zoom_modifier = egui::Modifiers::ALT;
                o.input_options.horizontal_scroll_modifier = egui::Modifiers::CTRL;
            });
            let mut app = VenturiApp::default();
            app.settings_path = settings::Settings::default_path();
            if let Some(path) = &app.settings_path {
                app.settings = settings::Settings::load(path);
            }
            app.settings.language.apply();
            // Opened immediately: opening the audio stream blocks for hundreds of ms.
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
#[path = "tests/main.rs"]
mod tests;
