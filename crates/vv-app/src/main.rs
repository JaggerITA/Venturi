//! Finestra egui: media pool (sinistra), viewer (centro), timeline
//! multi-traccia (basso), toolbar con play/pause/seek per l'anteprima.
//!
//! Il viewer di anteprima riusa lo stesso `Player` della milestone 2 e
//! resta scollegato dal playhead della timeline: mostrare il frame
//! composito dell'intera timeline (con più track, trim, trasformazioni)
//! è compito del compositor GPU di vv-render (milestone 5), il punto
//! giusto per risolvere "quale clip è attiva su quale track a che tempo"
//! una volta sola — vedi ARCHITECTURE.md § Compositing GPU. Per ora la
//! timeline è editing puro (drag, lift-delete, split) con undo/redo, e
//! l'anteprima è per-media tramite i pulsanti nel media pool.
//!
//! La texture del frame corrente viene riusata in-place a ogni frame
//! (`TextureHandle::set`) invece di riallocarla 25-60 volte al secondo.

mod player;
mod timeline_ui;

use player::Player;
use std::collections::HashMap;
use std::path::PathBuf;
use vv_core::{FrameIdx, MediaId, TimelineId, Track, TrackKind};

enum PoolAction {
    Preview(MediaId),
    AddToTimeline(MediaId),
}

#[derive(Default)]
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

impl eframe::App for VibeVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(player) = &mut self.preview_player {
            player.tick();
        }

        ui.input(|i| {
            if i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace) {
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

                ui.separator();
                if ui.button("Elimina (Del)").clicked() {
                    self.delete_selected();
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

        let mut pool_action = None;
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
                    if let Some(timeline_id) = self.timeline_id {
                        let tl = &self.project.timelines[timeline_id];
                        if let Some(clip) = tl.tracks[track_index]
                            .clips
                            .iter()
                            .find(|c| c.id == clip_id)
                        {
                            ui.label(format!("Track: {track_index}"));
                            ui.label(format!("Start: {} frame", clip.timeline_start));
                            ui.label(format!("Durata: {} frame", clip.timeline_len()));
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
            let current = self.preview_player.as_ref().and_then(|p| p.current_frame());
            if let Some(frame) = current {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.data,
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
