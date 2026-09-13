//! Finestra egui + apertura di un file video + player lineare (play/pause/
//! seek) con audio sincronizzato. Il frame corrente viene caricato come
//! texture egui e riusato in-place a ogni frame (`TextureHandle::set`) per
//! evitare una riallocazione GPU 25-60 volte al secondo. La conversione
//! YUV->RGB resta su CPU (vedi vv-media::decode) finché il compositor GPU
//! di vv-render non è pronto (milestone 5) — vedi ARCHITECTURE.md §
//! Compositing GPU.

mod player;

use player::Player;
use std::path::PathBuf;

#[derive(Default)]
struct VibeVideoApp {
    opened_path: Option<PathBuf>,
    probe_result: Option<Result<vv_core::MediaMeta, String>>,
    player: Option<Player>,
    player_error: Option<String>,
    frame_texture: Option<egui::TextureHandle>,
}

impl VibeVideoApp {
    fn open_file(&mut self, path: PathBuf) {
        self.frame_texture = None;
        self.player = None;
        self.player_error = None;

        let probe_result = vv_media::probe(&path).map_err(|e| e.to_string());

        if let Ok(meta) = &probe_result {
            let duration_secs = meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9);
            match Player::open(&path, duration_secs) {
                Ok(player) => self.player = Some(player),
                Err(e) => self.player_error = Some(e),
            }
        }

        self.probe_result = Some(probe_result);
        self.opened_path = Some(path);
    }
}

impl eframe::App for VibeVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(player) = &mut self.player {
            player.tick();
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Apri video...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("video", &["mp4", "mov", "mkv", "avi"])
                        .pick_file()
                {
                    self.open_file(path);
                }

                if let Some(player) = &mut self.player {
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

        egui::Panel::right("metadata").show(ui, |ui| {
            match (&self.opened_path, &self.probe_result) {
                (Some(path), Some(Ok(meta))) => {
                    ui.label(format!("File: {}", path.display()));
                    ui.label(format!("Risoluzione: {}x{}", meta.width, meta.height));
                    ui.label(format!(
                        "FPS: {}/{} ({:.3})",
                        meta.fps.num,
                        meta.fps.den,
                        meta.fps.as_f64()
                    ));
                    ui.label(format!("Durata: {} frame", meta.duration_frames));
                    ui.label(format!(
                        "Audio: {}",
                        if meta.has_audio {
                            format!("{} Hz, {} canali", meta.sample_rate, meta.channels)
                        } else {
                            "nessuno".into()
                        }
                    ));
                    if let Some(err) = &self.player_error {
                        ui.colored_label(egui::Color32::RED, format!("Player: {err}"));
                    }
                }
                (Some(path), Some(Err(err))) => {
                    ui.colored_label(
                        egui::Color32::RED,
                        format!("Errore aprendo {}: {err}", path.display()),
                    );
                }
                _ => {
                    ui.label("Apri un file video per vedere i metadata.");
                }
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let current = self.player.as_ref().and_then(|p| p.current_frame());
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
            } else if let Some(err) = &self.player_error {
                ui.colored_label(egui::Color32::RED, format!("Errore player: {err}"));
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label(if self.player.is_some() {
                        "Decodifica in corso..."
                    } else {
                        "Apri un file video per iniziare."
                    });
                });
            }
        });

        // Il player avanza in modo continuo (playback e decode-ahead in
        // background): serve un repaint continuo per riflettere il nuovo
        // frame/posizione, altrimenti egui ridisegna solo sugli eventi.
        if self.player.as_ref().is_some_and(Player::is_playing) {
            ui.ctx().request_repaint();
        }
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Argomento opzionale: path di un video da aprire subito all'avvio
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
                app.open_file(path);
            }
            Ok(Box::new(app))
        }),
    )
}
