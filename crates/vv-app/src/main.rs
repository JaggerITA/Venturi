//! Milestone 1: finestra egui + apertura di un file video + metadata +
//! decode del primo frame mostrato nel viewer. Per questo primo frame la
//! conversione YUV->RGB è su CPU e la texture passa dal path standard
//! `egui::Context::load_texture` (che internamente usa il device wgpu
//! condiviso con eframe). Il path continuo ad alte prestazioni per il
//! playback (milestone 2+) userà invece il compositor in `vv-render`, che
//! terrà i frame in YUV e farà la conversione in shader — vedi
//! ARCHITECTURE.md § Compositing GPU.

use std::path::PathBuf;

#[derive(Default)]
struct VibeVideoApp {
    opened_path: Option<PathBuf>,
    probe_result: Option<Result<vv_core::MediaMeta, String>>,
    frame_texture: Option<egui::TextureHandle>,
    frame_error: Option<String>,
}

impl VibeVideoApp {
    fn open_file(&mut self, ctx: &egui::Context, path: PathBuf) {
        self.probe_result = Some(vv_media::probe(&path).map_err(|e| e.to_string()));
        self.frame_texture = None;
        self.frame_error = None;

        match vv_media::decode_first_frame(&path) {
            Ok(frame) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &frame.data,
                );
                self.frame_texture =
                    Some(ctx.load_texture("current-frame", image, egui::TextureOptions::LINEAR));
            }
            Err(e) => self.frame_error = Some(e.to_string()),
        }

        self.opened_path = Some(path);
    }
}

impl eframe::App for VibeVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Apri video...").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("video", &["mp4", "mov", "mkv", "avi"])
                        .pick_file()
                {
                    self.open_file(ui.ctx(), path);
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
            if let Some(texture) = &self.frame_texture {
                let available = ui.available_size();
                let tex_size = texture.size_vec2();
                let scale = (available.x / tex_size.x).min(available.y / tex_size.y);
                let display_size = tex_size * scale.max(0.0);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::from_texture(texture).fit_to_exact_size(display_size));
                });
            } else if let Some(err) = &self.frame_error {
                ui.colored_label(egui::Color32::RED, format!("Errore decode: {err}"));
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Apri un file video per vedere il primo frame.");
                });
            }
        });
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
        Box::new(move |cc| {
            let mut app = VibeVideoApp::default();
            if let Some(path) = startup_path {
                app.open_file(&cc.egui_ctx, path);
            }
            Ok(Box::new(app))
        }),
    )
}
