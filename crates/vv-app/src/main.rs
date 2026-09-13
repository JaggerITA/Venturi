//! Milestone 1: finestra egui vuota + apertura di un file video + lettura
//! metadata via vv-media::probe. Il decode del primo frame e il disegno su
//! texture wgpu nel viewer sono il prossimo passo (vedi ARCHITECTURE.md).

use std::path::PathBuf;

struct VibeVideoApp {
    opened_path: Option<PathBuf>,
    probe_result: Option<Result<vv_core::MediaMeta, String>>,
}

impl Default for VibeVideoApp {
    fn default() -> Self {
        Self {
            opened_path: None,
            probe_result: None,
        }
    }
}

impl eframe::App for VibeVideoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Apri video...").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("video", &["mp4", "mov", "mkv", "avi"])
                        .pick_file()
                    {
                        self.probe_result =
                            Some(vv_media::probe(&path).map_err(|e| e.to_string()));
                        self.opened_path = Some(path);
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| match (&self.opened_path, &self.probe_result)
        {
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
        });
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "vibevideo",
        options,
        Box::new(|_cc| Ok(Box::new(VibeVideoApp::default()))),
    )
}
