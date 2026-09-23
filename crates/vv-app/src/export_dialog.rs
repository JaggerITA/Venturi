//! Export settings window, shown before starting the export.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use vv_core::{FrameIdx, Rational};
use vv_media::{AudioCodec, VideoCodec};
use crate::properties_panel::preview_combo;

use crate::export::ExportSettings;
use crate::format_duration;

/// Timeline data the window shows but does not modify.
pub struct TimelineInfo {
    pub resolution: (u32, u32),
    pub fps: Rational,
    pub total_frames: FrameIdx,
    /// In/out range, `None` if it covers the whole timeline.
    pub marks: Option<(FrameIdx, FrameIdx)>,
    pub has_audio: bool,
}

pub enum ExportDialogAction {
    None,
    Cancel,
    Export {
        settings: ExportSettings,
        range: std::ops::Range<FrameIdx>,
    },
}

pub struct ExportDialog {
    settings: ExportSettings,
    path_text: String,
    whole_timeline: bool,
    /// Kept aside while "Include audio" is off, so they can be restored.
    audio_settings: vv_media::AudioSettings,
    /// File dialog opened on a separate thread (on the GNOME/Wayland event
    /// loop thread it marks the app as unresponsive).
    browsing: Option<std::sync::mpsc::Receiver<Option<PathBuf>>>,
}

impl ExportDialog {
    pub fn new(settings: ExportSettings) -> Self {
        Self {
            path_text: settings.output_path.display().to_string(),
            audio_settings: settings.audio.clone().unwrap_or_default(),
            settings,
            whole_timeline: false,
            browsing: None,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, info: &TimelineInfo) -> ExportDialogAction {
        let mut action = ExportDialogAction::None;
        let mut open = true;
        egui::Window::new(t!("export.title"))
            .id(egui::Id::new("export_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                self.destination_section(ui);
                ui.separator();
                self.range_section(ui, info);
                ui.separator();
                self.video_section(ui, info);
                ui.separator();
                self.audio_section(ui, info);
                ui.separator();
                action = self.buttons(ui, info);
            });
        if !open {
            action = ExportDialogAction::Cancel;
        }
        action
    }

    fn destination_section(&mut self, ui: &mut egui::Ui) {
        ui.strong(t!("export.destination"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.path_text).desired_width(340.0));
            if ui
                .add_enabled(self.browsing.is_none(), egui::Button::new(t!("export.browse")))
                .clicked()
            {
                let current = PathBuf::from(&self.path_text);
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let mut dialog = rfd::FileDialog::new().add_filter("mp4", &["mp4"]);
                    if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(name) = current.file_name() {
                        dialog = dialog.set_file_name(name.to_string_lossy());
                    }
                    let _ = tx.send(dialog.save_file());
                });
                self.browsing = Some(rx);
            }
            if let Some(rx) = &self.browsing {
                match rx.try_recv() {
                    Ok(chosen) => {
                        if let Some(path) = chosen {
                            self.path_text = path.display().to_string();
                        }
                        self.browsing = None;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => self.browsing = None,
                }
            }
        });
        match validate_path(&self.path_text) {
            Ok(path) if path.exists() => {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    t!("export.overwrite_warning"),
                );
            }
            Ok(_) => {}
            Err(e) => {
                ui.colored_label(egui::Color32::RED, e);
            }
        }
    }

    fn range_section(&mut self, ui: &mut egui::Ui, info: &TimelineInfo) {
        ui.strong(t!("export.range"));
        let fps = info.fps.as_f64();
        match info.marks {
            Some((mark_in, mark_out)) => {
                ui.radio_value(
                    &mut self.whole_timeline,
                    false,
                    t!(
                        "export.range_in_out",
                        start = format_duration(mark_in, fps),
                        end = format_duration(mark_out, fps),
                        duration = format_duration(mark_out - mark_in, fps)
                    ),
                );
                ui.radio_value(
                    &mut self.whole_timeline,
                    true,
                    t!("export.range_whole", duration = format_duration(info.total_frames, fps)),
                );
            }
            None => {
                ui.label(t!(
                    "export.range_whole",
                    duration = format_duration(info.total_frames, fps)
                ));
            }
        }
    }

    fn video_section(&mut self, ui: &mut egui::Ui, info: &TimelineInfo) {
        ui.strong("Video");
        let video = &mut self.settings.video;
        egui::Grid::new("export_video_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(t!("export.encoder"));
                let before = video.codec;
                let items: Vec<_> = VideoCodec::ALL
                    .iter()
                    .map(|codec| (*codec, video_codec_label(*codec).to_string(), codec.is_available()))
                    .collect();
                preview_combo(
                    ui,
                    "export_video_codec",
                    &mut video.codec,
                    &items,
                    None,
                    None,
                    Some(&t!("export.unavailable_on_system")),
                );
                if video.codec != before {
                    video.preset = video.codec.default_preset().into();
                }
                ui.end_row();

                ui.label(t!("export.preset")).on_hover_text(t!("export.preset_hint"));
                let items: Vec<_> = video
                    .codec
                    .presets()
                    .iter()
                    .map(|preset| (preset.to_string(), preset.to_string(), true))
                    .collect();
                preview_combo(ui, "export_video_preset", &mut video.preset, &items, None, None, None);
                ui.end_row();

                ui.label(match video.codec {
                    VideoCodec::X264 => t!("export.quality_crf"),
                    VideoCodec::Nvenc => t!("export.quality_cq"),
                });
                ui.add(egui::Slider::new(&mut video.quality, 0..=51))
                    .on_hover_text(t!("export.quality_hint"));
                ui.end_row();

                ui.label(t!("export.resolution"));
                let scale = &mut self.settings.scale_percent;
                let size_label = |percent: u32| {
                    let mut probe = ExportSettings::new(PathBuf::new());
                    probe.scale_percent = percent;
                    let (w, h) = probe.output_size(info.resolution);
                    format!("{w}×{h} ({percent}%)")
                };
                let items: Vec<_> = ExportSettings::SCALE_CHOICES
                    .iter()
                    .map(|percent| (*percent, size_label(*percent), true))
                    .collect();
                preview_combo(ui, "export_scale", scale, &items, None, None, None);
                ui.end_row();

                ui.label(t!("export.frame_rate"));
                ui.label(t!("export.fps_of_timeline", fps = format!("{:.3}", info.fps.as_f64())));
                ui.end_row();
            });
    }

    fn audio_section(&mut self, ui: &mut egui::Ui, info: &TimelineInfo) {
        ui.strong("Audio");
        if !info.has_audio {
            ui.label(t!("export.no_audio"));
            return;
        }
        let mut include = self.settings.audio.is_some();
        ui.checkbox(&mut include, t!("export.include_audio"));
        if !include {
            if let Some(audio) = self.settings.audio.take() {
                self.audio_settings = audio;
            }
            return;
        }
        let audio = self
            .settings
            .audio
            .get_or_insert_with(|| self.audio_settings.clone());
        egui::Grid::new("export_audio_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label(t!("export.encoder"));
                let items: Vec<_> = AudioCodec::ALL
                    .iter()
                    .map(|codec| (*codec, audio_codec_label(*codec).to_string(), codec.is_available()))
                    .collect();
                preview_combo(
                    ui,
                    "export_audio_codec",
                    &mut audio.codec,
                    &items,
                    None,
                    None,
                    Some(&t!("export.unavailable_in_ffmpeg")),
                );
                ui.end_row();

                if audio.codec == AudioCodec::Aac {
                    ui.label(t!("export.preset"));
                    let items: Vec<_> = [false, true]
                        .into_iter()
                        .map(|fast| (fast, aac_coder_label(fast).to_string(), true))
                        .collect();
                    preview_combo(ui, "export_audio_coder", &mut audio.fast_coder, &items, None, None, None);
                    ui.end_row();
                }

                ui.label("Bitrate");
                let items: Vec<_> = [96, 128, 160, 192, 256, 320]
                    .into_iter()
                    .map(|kbps| (kbps, format!("{kbps} kbps"), true))
                    .collect();
                preview_combo(ui, "export_audio_bitrate", &mut audio.bitrate_kbps, &items, None, None, None);
                ui.end_row();
            });
    }

    fn export_range(&self, info: &TimelineInfo) -> std::ops::Range<FrameIdx> {
        match info.marks {
            Some((mark_in, mark_out)) if !self.whole_timeline => mark_in..mark_out,
            _ => 0..info.total_frames,
        }
    }

    fn buttons(&mut self, ui: &mut egui::Ui, info: &TimelineInfo) -> ExportDialogAction {
        let path = validate_path(&self.path_text);
        let mut action = ExportDialogAction::None;
        ui.horizontal(|ui| {
            if ui.add_enabled(path.is_ok(), egui::Button::new(t!("export.start"))).clicked()
                && let Ok(path) = path
            {
                let mut settings = self.settings.clone();
                settings.output_path = path;
                action = ExportDialogAction::Export {
                    settings,
                    range: self.export_range(info),
                };
            }
            if ui.button(t!("common.cancel")).clicked() {
                action = ExportDialogAction::Cancel;
            }
        });
        action
    }
}

/// Valid output path, with the `.mp4` extension added if missing.
fn validate_path(text: &str) -> Result<PathBuf, Cow<'static, str>> {
    let text = text.trim();
    if text.is_empty() {
        return Err(t!("export.error_no_path"));
    }
    let mut path = PathBuf::from(text);
    if path.file_name().is_none() {
        return Err(t!("export.error_no_file_name"));
    }
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
    {
        path.as_mut_os_string().push(".mp4");
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if parent.is_some_and(|p| !p.is_dir()) {
        return Err(t!("export.error_missing_folder"));
    }
    Ok(path)
}

/// Path proposed on the first export: next to the project, with its name.
pub fn default_output_path(project_path: Option<&Path>) -> PathBuf {
    match project_path {
        Some(project) => project.with_extension("mp4"),
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("export.mp4"),
    }
}

fn video_codec_label(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::X264 => "H.264 – x264 (CPU)",
        VideoCodec::Nvenc => "H.264 – NVENC (GPU NVIDIA)",
    }
}

fn audio_codec_label(codec: AudioCodec) -> Cow<'static, str> {
    match codec {
        AudioCodec::Aac => "AAC – ffmpeg".into(),
        AudioCodec::FdkAac => t!("export.codec_fdk"),
    }
}

fn aac_coder_label(fast: bool) -> Cow<'static, str> {
    if fast { t!("export.coder_fast") } else { t!("export.coder_quality") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_path_appends_mp4_and_rejects_missing_dirs() {
        let dir = std::env::temp_dir();
        let expected = dir.join("video.mp4");
        assert_eq!(validate_path(dir.join("video").to_str().unwrap()), Ok(expected.clone()));
        assert_eq!(validate_path(expected.to_str().unwrap()), Ok(expected));
        assert!(validate_path("").is_err());
        assert!(validate_path("/does/not/really/exist/video.mp4").is_err());
    }

    #[test]
    fn export_uses_in_out_marks_unless_whole_timeline_is_chosen() {
        let info = TimelineInfo {
            resolution: (1920, 1080),
            fps: Rational::new(25, 1),
            total_frames: 100,
            marks: Some((20, 60)),
            has_audio: true,
        };
        let path = std::env::temp_dir().join("out.mp4");
        let mut dialog = ExportDialog::new(ExportSettings::new(path));
        assert_eq!(dialog.export_range(&info), 20..60);
        dialog.whole_timeline = true;
        assert_eq!(dialog.export_range(&info), 0..100);
    }

    #[test]
    fn output_size_is_even_and_exact_at_full_scale() {
        let mut settings = ExportSettings::new(PathBuf::new());
        assert_eq!(settings.output_size((1920, 1080)), (1920, 1080));
        settings.scale_percent = 75;
        assert_eq!(settings.output_size((1920, 1080)), (1440, 810));
        settings.scale_percent = 25;
        assert_eq!(settings.output_size((1366, 768)), (340, 192));
    }

    #[test]
    fn dialog_renders_without_starting_an_export_on_its_own() {
        let info = TimelineInfo {
            resolution: (1920, 1080),
            fps: Rational::new(60, 1),
            total_frames: 28_800,
            marks: Some((600, 1200)),
            has_audio: true,
        };
        let mut dialog = ExportDialog::new(ExportSettings::new(std::env::temp_dir().join("x.mp4")));
        let ctx = egui::Context::default();
        for _ in 0..3 {
            let mut action = None;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                action = Some(dialog.show(ui.ctx(), &info));
            });
            output.textures_delta.clear();
            assert!(matches!(action, Some(ExportDialogAction::None)));
        }
    }
}
