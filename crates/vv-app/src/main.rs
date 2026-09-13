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

mod player;
mod timeline_ui;

use player::Player;
use std::collections::HashMap;
use std::path::PathBuf;
use vv_core::{ClipId, FrameIdx, MediaId, TimelineId, Track, TrackKind};

enum PoolAction {
    Preview(MediaId),
    AddToTimeline(MediaId),
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

    /// Clip la cui anteprima è attualmente mostrata: guida sia il player
    /// (quale media riprodurre) sia il transform/gain applicati (milestone
    /// 5). `None` se non c'è ancora una clip selezionata.
    active_clip: Option<(usize, ClipId)>,
    compositor: vv_render::Compositor,
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
            active_clip: None,
            compositor: vv_render::Compositor::new_headless(),
        }
    }
}

impl VibeVideoApp {
    fn import_media(&mut self, path: PathBuf) {
        match vv_media::probe(&path) {
            Ok(meta) => {
                self.import_error = None;
                if self.timeline_id.is_none() {
                    let id = self.project.timelines.insert(vv_core::Timeline {
                        name: "Timeline 1".into(),
                        fps: meta.fps,
                        resolution: (meta.width, meta.height),
                        tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
                    });
                    self.timeline_id = Some(id);
                }
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

    fn preview_media(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let path = item.path.clone();
        let meta = item.meta.clone();
        let duration_secs = meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9);

        self.frame_texture = None;
        self.preview_player = None;
        self.preview_error = None;

        match Player::open(&path, duration_secs) {
            Ok(player) => self.preview_player = Some(player),
            Err(e) => self.preview_error = Some(e),
        }
        self.preview_path = Some(path);
        self.preview_meta = Some(meta);
    }

    /// Mostra l'anteprima della clip selezionata in timeline: apre il
    /// player sul suo media e applica subito gain/transform correnti.
    fn preview_clip(&mut self, track_index: usize, clip_id: ClipId) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(clip) = self.project.timelines[timeline_id]
            .tracks
            .get(track_index)
            .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
        else {
            return;
        };

        self.active_clip = Some((track_index, clip_id));

        match clip.source {
            vv_core::ClipSource::Media(media_id) => {
                self.preview_media(media_id);
                self.apply_active_clip_gain();
            }
            vv_core::ClipSource::SolidColor => {
                // Nessun media da aprire: il viewer genera il frame colore
                // al volo (milestone 6), non serve un Player. Ripulisco lo
                // stato del player precedente per non mostrare un frame
                // stantio dell'ultima clip media selezionata.
                self.preview_player = None;
                self.frame_texture = None;
                self.preview_error = None;
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

    fn add_media_to_timeline(&mut self, media_id: MediaId) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        let meta = item.meta.clone();

        let video_start = track_end(&self.project, timeline_id, 0);
        let video_clip = vv_core::Clip {
            id: self.project.alloc_clip_id(),
            source: vv_core::ClipSource::Media(media_id),
            source_in: 0,
            source_out: meta.duration_frames,
            timeline_start: video_start,
            effects: vv_core::EffectStack::default(),
        };
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::InsertClip {
                timeline: timeline_id,
                track_index: 0,
                clip: video_clip,
            }),
        );

        if meta.has_audio {
            let audio_start = track_end(&self.project, timeline_id, 1);
            let audio_clip = vv_core::Clip {
                id: self.project.alloc_clip_id(),
                source: vv_core::ClipSource::Media(media_id),
                source_in: 0,
                source_out: meta.duration_frames,
                timeline_start: audio_start,
                effects: vv_core::EffectStack::default(),
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

    /// Normal delete: rimuove la clip selezionata, lascia un vuoto al suo
    /// posto sulla track. Le altre track non si muovono.
    fn delete_selected(&mut self) {
        if let (Some(timeline_id), Some((track_index, clip_id))) =
            (self.timeline_id, self.timeline_state.selected)
        {
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::LiftDelete::new(timeline_id, track_index, clip_id)),
            );
            self.timeline_state.selected = None;
        }
    }

    /// Ripple delete: rimuove la clip selezionata e chiude il gap su
    /// *tutte* le track, mantenendo il sync audio/video (vedi
    /// ARCHITECTURE.md § Ripple delete — comportamento scelto: sempre
    /// globale, nessun toggle).
    fn ripple_delete_selected(&mut self) {
        if let (Some(timeline_id), Some((track_index, clip_id))) =
            (self.timeline_id, self.timeline_state.selected)
        {
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::RippleDeleteAllTracks::new(
                    timeline_id,
                    track_index,
                    clip_id,
                )),
            );
            self.timeline_state.selected = None;
        }
    }

    fn split_selected_at_playhead(&mut self) {
        if let (Some(timeline_id), Some((track_index, clip_id))) =
            (self.timeline_id, self.timeline_state.selected)
        {
            let playhead = self.timeline_state.playhead;
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::SplitClip::new(
                    timeline_id,
                    track_index,
                    clip_id,
                    playhead,
                )),
            );
        }
    }
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
fn keyframe_button(
    ui: &mut egui::Ui,
    is_constant: bool,
    has_keyframe_here: bool,
) -> egui::Response {
    let (symbol, tooltip) = if is_constant {
        ("◇", "Anima: crea il primo keyframe qui")
    } else if has_keyframe_here {
        ("◆", "Rimuovi il keyframe qui")
    } else {
        ("◇", "Aggiungi un keyframe qui")
    };
    ui.button(symbol).on_hover_text(tooltip)
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

        if self.timeline_state.selected != self.active_clip
            && let Some((track_index, clip_id)) = self.timeline_state.selected
        {
            self.preview_clip(track_index, clip_id);
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

        ui.input(|i| {
            let delete_pressed =
                i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace);
            if delete_pressed && i.modifiers.shift {
                self.ripple_delete_selected();
            } else if delete_pressed {
                self.delete_selected();
            }
            if i.key_pressed(egui::Key::S) && !i.modifiers.command {
                self.split_selected_at_playhead();
            }
            if i.modifiers.command && i.key_pressed(egui::Key::Z) {
                if i.modifiers.shift {
                    self.history.redo(&mut self.project);
                } else {
                    self.history.undo(&mut self.project);
                }
            }
        });

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Importa media...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("video", &["mp4", "mov", "mkv", "avi"])
                        .pick_file()
                {
                    self.import_media(path);
                }
                if ui.button("Nuovo Solid Color").clicked() {
                    self.add_solid_color_clip();
                }

                ui.separator();
                if ui.button("Elimina (Del)").clicked() {
                    self.delete_selected();
                }
                if ui
                    .button("Ripple delete (Shift+Del)")
                    .on_hover_text(
                        "Rimuove la clip e chiude il gap su tutte le track, mantenendo il sync A/V",
                    )
                    .clicked()
                {
                    self.ripple_delete_selected();
                }
                if ui.button("Dividi (S)").clicked() {
                    self.split_selected_at_playhead();
                }
                ui.separator();
                if ui.button("Undo (Ctrl+Z)").clicked() {
                    self.history.undo(&mut self.project);
                }
                if ui.button("Redo (Ctrl+Shift+Z)").clicked() {
                    self.history.redo(&mut self.project);
                }

                if let Some(player) = &mut self.preview_player {
                    ui.separator();
                    let label = if player.is_playing() { "⏸" } else { "▶" };
                    if ui.button(label).clicked() {
                        player.toggle_play_pause();
                    }

                    let duration = player.duration_secs();
                    let mut pos = player.position_secs();
                    ui.label(format!("{pos:.2}s / {duration:.2}s"));
                    let resp = ui.add(
                        egui::Slider::new(&mut pos, 0.0..=duration.max(0.001))
                            .show_value(false)
                            .trailing_fill(true),
                    );
                    if resp.changed() {
                        player.seek_secs(pos);
                    }
                }
            });
        });

        // Frame a cui vengono lette/scritte le proprietà nel pannello:
        // per una clip Media è la posizione del player; per una clip
        // SolidColor (che non ha un player) è la posizione sul playhead
        // della timeline, tradotta in frame locale alla clip — lo stesso
        // frame usato dal viewer per generare l'anteprima del colore.
        let selected_clip_start_if_solid_color =
            self.timeline_state
                .selected
                .and_then(|(track_index, clip_id)| {
                    let timeline_id = self.timeline_id?;
                    let clip = self.project.timelines[timeline_id]
                        .tracks
                        .get(track_index)?
                        .clips
                        .iter()
                        .find(|c| c.id == clip_id)?;
                    matches!(clip.source, vv_core::ClipSource::SolidColor)
                        .then_some(clip.timeline_start)
                });
        let source_frame = match selected_clip_start_if_solid_color {
            Some(clip_start) => (self.timeline_state.playhead - clip_start).max(0),
            None => self
                .preview_player
                .as_ref()
                .map(|p| p.current_source_frame())
                .unwrap_or(0),
        };

        let mut pool_action = None;
        let mut pending_effect = None;
        egui::Panel::left("media_pool")
            .default_size(260.0)
            .show(ui, |ui| {
                ui.heading("Media Pool");
                if let Some(err) = &self.import_error {
                    ui.colored_label(egui::Color32::RED, err);
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let items: Vec<(MediaId, String, vv_core::MediaMeta)> = self
                        .project
                        .media_pool
                        .iter()
                        .map(|(id, item)| (id, file_label(&item.path), item.meta.clone()))
                        .collect();
                    for (id, label, meta) in items {
                        ui.group(|ui| {
                            ui.label(&label);
                            ui.small(format!(
                                "{}x{} · {:.2}fps · {}",
                                meta.width,
                                meta.height,
                                meta.fps.as_f64(),
                                if meta.has_audio { "audio" } else { "muto" }
                            ));
                            ui.horizontal(|ui| {
                                if ui.button("Anteprima").clicked() {
                                    pool_action = Some(PoolAction::Preview(id));
                                }
                                if ui.button("Aggiungi").clicked() {
                                    pool_action = Some(PoolAction::AddToTimeline(id));
                                }
                            });
                        });
                    }
                });

                if let Some((track_index, clip_id)) = self.timeline_state.selected {
                    ui.separator();
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
                                is_solid_color: matches!(c.source, vv_core::ClipSource::SolidColor),
                                transform_constant: c.effects.transform.is_constant(),
                                transform_kf_here: c
                                    .effects
                                    .transform
                                    .keyframe_at(source_frame)
                                    .is_some(),
                                transform: c.effects.transform.value_at(source_frame),
                                gain_constant: c.effects.gain_db.is_constant(),
                                gain_kf_here: c.effects.gain_db.keyframe_at(source_frame).is_some(),
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
                        ui.label(format!(
                            "Frame corrente ({}): {source_frame}",
                            if is_solid_color {
                                "locale alla clip"
                            } else {
                                "source"
                            }
                        ));

                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Crop / zoom / posizione");
                            if keyframe_button(ui, transform_constant, transform_kf_here).clicked()
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
                                    .and_then(|tid| self.project.timelines[tid].tracks[track_index]
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
                            .add(egui::Slider::new(&mut transform.crop[1], 0.0..=0.99).text("alto"))
                            .changed();
                        transform_changed |= ui
                            .add(
                                egui::Slider::new(&mut transform.crop[2], 0.01..=1.0)
                                    .text("destra"),
                            )
                            .changed();
                        transform_changed |= ui
                            .add(
                                egui::Slider::new(&mut transform.crop[3], 0.01..=1.0).text("basso"),
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
                                if keyframe_button(ui, color_constant, color_kf_here).clicked() {
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
                }
            });

        if let Some(action) = pool_action {
            match action {
                PoolAction::Preview(id) => self.preview_media(id),
                PoolAction::AddToTimeline(id) => self.add_media_to_timeline(id),
            }
        }
        if let (Some(timeline_id), Some(change)) = (self.timeline_id, pending_effect) {
            let cmd = build_effect_command(timeline_id, change);
            self.history.do_command(&mut self.project, cmd);
            self.apply_active_clip_gain();
        }

        egui::Panel::bottom("timeline")
            .default_size(240.0)
            .resizable(true)
            .show(ui, |ui| {
                ui.heading("Timeline");
                if let Some(timeline_id) = self.timeline_id {
                    let labels: HashMap<MediaId, String> = self
                        .project
                        .media_pool
                        .iter()
                        .map(|(id, item)| (id, file_label(&item.path)))
                        .collect();
                    timeline_ui::show_timeline(
                        ui,
                        &mut self.project,
                        &mut self.history,
                        timeline_id,
                        &|id| labels.get(&id).cloned().unwrap_or_default(),
                        &mut self.timeline_state,
                    );
                } else {
                    ui.label("Importa un media per creare la timeline.");
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            // Le clip SolidColor non hanno un media da decodificare: il
            // colore (eventualmente keyframeato) va valutato al frame
            // *locale alla clip* sul playhead della timeline, l'unico
            // orologio che ha senso per un generatore (un Player non ha
            // motivo di esistere per un riempimento uniforme).
            let solid_color_frame_info = self.active_clip.and_then(|(track_index, clip_id)| {
                let timeline_id = self.timeline_id?;
                let tl = &self.project.timelines[timeline_id];
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
                        "Importa un media e premi Anteprima."
                    });
                });
            }
        });

        if self.preview_player.as_ref().is_some_and(Player::is_playing) {
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
    fn ripple_delete_selected_shifts_other_tracks_and_clears_selection() {
        let mut app = VibeVideoApp::default();
        let video_a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_b = make_timeline_with_clip(&mut app, 0, 10, 10);
        let audio_a = make_timeline_with_clip(&mut app, 1, 0, 10);
        let _audio_b = make_timeline_with_clip(&mut app, 1, 10, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = Some((0, video_b));
        app.ripple_delete_selected();

        assert_eq!(app.timeline_state.selected, None);
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
    fn delete_selected_does_not_shift_other_tracks() {
        let mut app = VibeVideoApp::default();
        let video_a = make_timeline_with_clip(&mut app, 0, 0, 10);
        let video_b = make_timeline_with_clip(&mut app, 0, 10, 10);
        make_timeline_with_clip(&mut app, 1, 0, 10);
        make_timeline_with_clip(&mut app, 1, 10, 10);
        let timeline_id = app.timeline_id.unwrap();

        app.timeline_state.selected = Some((0, video_b));
        app.delete_selected();

        let tl = &app.project.timelines[timeline_id];
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[0].clips[0].id, video_a);
        assert_eq!(tl.tracks[1].clips.len(), 2);
        assert_eq!(tl.tracks[1].clips[1].timeline_start, 10);
    }

    /// Esercita il collegamento completo introdotto in milestone 5:
    /// selezionare una clip in timeline apre il player sul suo media
    /// (`preview_clip`) e il gain impostato via comando arriva davvero
    /// all'`AudioPlayer` sottostante, senza panic.
    #[test]
    fn selecting_a_media_clip_opens_preview_and_applies_gain() {
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

        app.timeline_state.selected = Some((0, clip_id));
        app.preview_clip(0, clip_id);

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
    fn selecting_solid_color_clip_clears_preview_player_and_sets_active_clip() {
        let mut app = VibeVideoApp::default();
        app.add_solid_color_clip();
        let timeline_id = app.timeline_id.unwrap();
        let clip_id = app.project.timelines[timeline_id].tracks[0].clips[0].id;

        app.preview_clip(0, clip_id);

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
}
