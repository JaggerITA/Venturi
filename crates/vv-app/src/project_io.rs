//! Media import, file dialogs, saving/opening the project, OTIO,
//! export and relink.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectSwitch {
    Open,
    OpenRecent(PathBuf),
    ImportOtio,
    Quit,
}

pub(crate) enum UnsavedChoice {
    Save,
    Discard,
    Cancel,
}

/// What to do with the result of a background file dialog. `RelinkMedia`
/// carries the pool selection as it was when it opened.
pub(crate) enum DialogKind {
    ImportMedia,
    SaveProjectAs,
    ExportOtio(TimelineId),
    ImportOtio,
    OpenProject,
    RelinkMedia(Vec<MediaId>),
}

pub(crate) enum DialogOutcome {
    File(Option<PathBuf>),
    Files(Option<Vec<PathBuf>>),
}

/// Multiple import in progress: probing on the `ImportWorker` threads, outcome
/// (preview of the last one, errors) applied when it finishes.
pub(crate) struct PendingImport {
    pub(crate) worker: import_worker::ImportWorker,
    pub(crate) errors: Vec<String>,
    pub(crate) imported: Vec<MediaId>,
}

pub(crate) struct PendingDialog {
    pub(crate) kind: DialogKind,
    pub(crate) rx: mpsc::Receiver<DialogOutcome>,
}

/// UI state of an export in progress: progress/cancellation shared with the
/// thread actually exporting (`export::export_timeline`),
/// plus the handle to collect its outcome at the end.
pub(crate) struct ExportUiState {
    pub(crate) progress: std::sync::Arc<Mutex<export::ExportProgress>>,
    pub(crate) cancel: std::sync::Arc<AtomicBool>,
    pub(crate) handle: std::thread::JoinHandle<Result<(), String>>,
}

/// All the files under `base_dir` by name, breadth-first: on equal names the
/// least nested one wins. Unreadable directories are skipped.
/// Absolute, symlink-free form of `path`, or `path` itself if the file is
/// not reachable (removed media must still compare equal to itself).
fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn index_media_by_filename(base_dir: &Path) -> HashMap<std::ffi::OsString, PathBuf> {
    let mut index = HashMap::new();
    let mut dirs = std::collections::VecDeque::from([base_dir.to_path_buf()]);
    while let Some(dir) = dirs.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                dirs.push_back(path);
            } else if file_type.is_file() {
                index.entry(entry.file_name()).or_insert(path);
            }
        }
    }
    index
}

impl VenturiApp {
    pub(crate) fn import_media(&mut self, path: PathBuf) {
        if let Some(existing) = self.media_with_path(&path) {
            self.import_warnings.clear();
            self.preview_media(existing);
            self.media_pool_state.select_only([existing]);
            return;
        }
        match self.add_media_to_pool(path) {
            Ok(media_id) => {
                self.import_warnings.clear();
                self.preview_media(media_id);
                self.media_pool_state.select_only([media_id]);
            }
            Err(e) => self.import_warnings = vec![e],
        }
    }

    /// Multiple import: probing the files goes to `ImportWorker` (tens of
    /// ms each), the UI stays alive and shows the progress. Preview
    /// of the last imported media only, errors collected instead of
    /// overwriting one another.
    pub(crate) fn import_media_files(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.pending_import.is_some() {
            self.import_queue.extend(paths);
            return;
        }
        let paths = self.without_media_already_in_pool(paths);
        if paths.is_empty() {
            return;
        }
        self.import_warnings.clear();
        self.pending_import = Some(PendingImport {
            worker: import_worker::ImportWorker::spawn(paths),
            errors: Vec::new(),
            imported: Vec::new(),
        });
    }

    /// Adds to the pool the media already probed by the `ImportWorker`.
    pub(crate) fn poll_pending_import(&mut self, ctx: &egui::Context) {
        let Some(mut pending) = self.pending_import.take() else {
            let queued = std::mem::take(&mut self.import_queue);
            self.import_media_files(queued);
            return;
        };
        for (path, result) in pending.worker.drain_ready() {
            let label = file_label(&path);
            match result {
                Ok(meta) => {
                    let media_id = self.insert_media(path, meta);
                    pending.imported.push(media_id);
                }
                Err(e) => pending.errors.push(format!("{label}: {e}")),
            }
        }
        if !pending.worker.is_finished() {
            self.pending_import = Some(pending);
            // Without a new event egui would not redraw: the progress
            // bar would stay frozen.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        }
        self.import_warnings = pending.errors;
        if let Some(&last) = pending.imported.last() {
            self.preview_media(last);
        }
        if !pending.imported.is_empty() {
            self.media_pool_state.select_only(pending.imported);
        }
    }

    /// Blocks until the multiple import in progress is finished: the tests
    /// have no event loop calling `poll_pending_import`.
    #[cfg(test)]
    pub(crate) fn wait_for_import(&mut self) {
        let ctx = egui::Context::default();
        while self.pending_import.is_some() || !self.import_queue.is_empty() {
            self.poll_pending_import(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub(crate) fn import_progress(&self) -> Option<(usize, usize)> {
        self.pending_import.as_ref().map(|p| {
            let (done, total) = p.worker.progress();
            (done, total + self.import_queue.len())
        })
    }

    /// The media in the pool that points at `path`, if any. Paths are
    /// canonicalized so that symlinks and `..` do not import a duplicate.
    pub(crate) fn media_with_path(&self, path: &Path) -> Option<MediaId> {
        let target = canonical_path(path);
        self.project
            .media_pool
            .iter()
            .find(|(_, item)| item.compound.is_none() && canonical_path(&item.path) == target)
            .map(|(id, _)| id)
    }

    /// Drops the paths already in the pool and the duplicates inside the
    /// batch itself.
    fn without_media_already_in_pool(&self, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let mut seen: std::collections::HashSet<PathBuf> = self
            .project
            .media_pool
            .iter()
            .filter(|(_, item)| item.compound.is_none())
            .map(|(_, item)| canonical_path(&item.path))
            .collect();
        paths.into_iter().filter(|path| seen.insert(canonical_path(path))).collect()
    }

    pub(crate) fn add_media_to_pool(&mut self, path: PathBuf) -> Result<MediaId, String> {
        match vv_media::probe_media(&path) {
            Ok(meta) => Ok(self.insert_media(path, meta)),
            Err(e) => Err(e.to_string()),
        }
    }

    fn insert_media(&mut self, path: PathBuf, meta: vv_core::MediaMeta) -> MediaId {
        if meta.has_video {
            self.ensure_timeline_for(&meta);
        } else {
            self.ensure_timeline_audio_only();
        }
        // `0` only if the file vanished in the meantime: at worst a proxy is
        // regenerated.
        let content_hash = vv_media::content_fingerprint(&path).unwrap_or(0);
        let media_id = self.project.media_pool.insert(vv_core::MediaItem {
            path,
            meta,
            content_hash,
            compound: None,
        });
        self.unsaved_media = true;
        self.enqueue_media_background_jobs(media_id);
        media_id
    }

    /// Proxy, thumbnail and waveform of a media in the pool, whether just
    /// imported or from an opened project: what is already in the on-disk
    /// cache is skipped by the workers.
    pub(crate) fn enqueue_media_background_jobs(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else {
            return;
        };
        // A compound clip has no file on disk to proxy/thumbnail/
        // analyze: its content is `item.compound`, not `item.path`.
        if item.compound.is_some() {
            return;
        }
        // An image has nothing to gain from a proxy, and its duration is
        // the `IMAGE_DURATION_FRAMES` sentinel.
        if self.settings.proxy_enabled && item.meta.has_video && !item.meta.is_image() {
            let frames = item.meta.duration_frames.max(0) as u64;
            let quality = self.settings.proxy_quality;
            self.proxy_worker
                .get_or_insert_with(|| proxy_worker::ProxyWorker::spawn(quality))
                .enqueue(item.path.clone(), item.content_hash, frames);
        }
        if item.meta.has_video && !self.thumbnails.contains_key(&item.content_hash) {
            self.thumbnails.insert(item.content_hash, None);
            self.thumbnail_worker
                .get_or_insert_with(thumbnail_worker::ThumbnailWorker::spawn)
                .enqueue(
                    item.path.clone(),
                    item.content_hash,
                    item.meta.duration_frames as f64 / item.meta.fps.as_f64(),
                );
        }
        // Only media with audio: the timeline draws the waveform only
        // on audio clips.
        if item.meta.has_audio {
            let secs = item.meta.duration_frames as f64 / item.meta.fps.as_f64();
            let num_peaks = vv_media::recommended_num_peaks(secs);
            self.waveform_worker
                .get_or_insert_with(waveform_worker::WaveformWorker::spawn)
                .enqueue(
                    item.path.clone(),
                    item.content_hash,
                    item.meta.audio_stream_count(),
                    num_peaks,
                );
        }
    }

    pub(crate) fn poll_thumbnails(&mut self, ctx: &egui::Context) {
        let Some(worker) = &mut self.thumbnail_worker else {
            return;
        };
        for (content_hash, thumb) in worker.drain() {
            let texture = thumb.map(|t| {
                ctx.load_texture(
                    format!("thumbnail-{content_hash:016x}"),
                    egui::ColorImage::from_rgba_unmultiplied(
                        [t.width as usize, t.height as usize],
                        &t.rgba,
                    ),
                    egui::TextureOptions::LINEAR,
                )
            });
            self.thumbnails.insert(content_hash, texture);
        }
    }

    /// Opens the native file dialog on a separate thread: on the
    /// GNOME/Wayland event loop thread it marks the app as unresponsive. One at
    /// a time.
    pub(crate) fn spawn_dialog(
        &mut self,
        kind: DialogKind,
        run: impl FnOnce(rfd::FileDialog) -> DialogOutcome + Send + 'static,
    ) {
        if self.pending_dialog.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run(rfd::FileDialog::new()));
        });
        self.pending_dialog = Some(PendingDialog { kind, rx });
    }

    pub(crate) fn spawn_file_dialog(
        &mut self,
        kind: DialogKind,
        build: impl FnOnce(rfd::FileDialog) -> Option<PathBuf> + Send + 'static,
    ) {
        self.spawn_dialog(kind, |dlg| DialogOutcome::File(build(dlg)));
    }

    /// Applies the result of the background dialog, if it arrived.
    pub(crate) fn poll_pending_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_dialog else {
            return;
        };
        let Ok(outcome) = pending.rx.try_recv() else {
            // Still waiting: without a new event (mouse, keyboard)
            // egui would not redraw, so the result would arrive
            // only at the user's next input.
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        };
        let Some(PendingDialog { kind, .. }) = self.pending_dialog.take() else {
            return;
        };
        match (kind, outcome) {
            (DialogKind::ImportMedia, DialogOutcome::Files(Some(paths))) => {
                self.import_media_files(paths);
            }
            (DialogKind::SaveProjectAs, DialogOutcome::File(Some(path))) => {
                self.save_project_to(&path);
            }
            (DialogKind::ExportOtio(timeline_id), DialogOutcome::File(Some(path))) => {
                self.export_otio_to(timeline_id, &path);
            }
            (DialogKind::ImportOtio, DialogOutcome::File(Some(path))) => {
                self.import_otio_from(&path);
            }
            (DialogKind::OpenProject, DialogOutcome::File(Some(path))) => {
                self.load_project_from(path);
            }
            (DialogKind::RelinkMedia(targets), DialogOutcome::File(Some(base_dir))) => {
                self.relink_media(&base_dir, &targets);
            }
            _ => {} // dialog cancelled by the user
        }
    }

    /// Files dropped by the file manager onto the window: they are imported into
    /// the pool, wherever they land.
    pub(crate) fn poll_dropped_files(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect()
        });
        if !paths.is_empty() {
            self.import_media_files(paths);
        }
    }

    // Opens the file dialog and imports the chosen files (used by the toolbar
    // button and by the Ctrl+I shortcut).
    pub(crate) fn import_media_dialog(&mut self) {
        self.spawn_dialog(DialogKind::ImportMedia, |dlg| {
            DialogOutcome::Files(
            dlg.add_filter(
                "media",
                &[
                    "mp4", "mov", "mkv", "avi", "wav", "mp3", "flac", "m4a", "aac", "ogg", "opus",
                    "jpg", "jpeg", "png", "bmp", "webp", "tif", "tiff",
                ],
            )
            .add_filter("video", &["mp4", "mov", "mkv", "avi"])
            .add_filter("audio", &["wav", "mp3", "flac", "m4a", "aac", "ogg", "opus"])
            .add_filter(t!("file_filter.images"), vv_media::IMAGE_EXTENSIONS)
            .pick_files(),
            )
        });
    }

    /// Saves to the current file (`current_project_path`), or as a "save as"
    /// if the project has not been saved/opened yet.
    pub(crate) fn save_project(&mut self) {
        match self.current_project_path.clone() {
            Some(path) => self.save_project_to(&path),
            None => self.save_project_as(),
        }
    }

    /// Always opens the save file dialog, even if the project already has
    /// a current file (used by the "Save as..." button and by
    /// Ctrl+Shift+S).
    pub(crate) fn save_project_as(&mut self) {
        self.spawn_file_dialog(DialogKind::SaveProjectAs, |dlg| {
            dlg.set_file_name(format!("{}.vvproj", t!("project.default_file_name")))
                .add_filter(t!("file_filter.project"), &["vvproj"])
                .save_file()
        });
    }

    pub(crate) fn save_project_to(&mut self, path: &Path) {
        match vv_core::save_project(&self.project, path) {
            Ok(()) => {
                self.current_project_path = Some(path.to_path_buf());
                self.project_error = None;
                self.mark_saved();
                self.remember_recent_project(path.to_path_buf());
            }
            Err(e) => self.project_error = Some(t!("project.save_failed", error = e).into_owned()),
        }
    }

    pub(crate) fn export_otio_dialog(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let file_name = format!("{}.otio", self.project.timelines[timeline_id].name);
        self.spawn_file_dialog(DialogKind::ExportOtio(timeline_id), move |dlg| {
            dlg.set_file_name(file_name)
                .add_filter("OpenTimelineIO", &["otio"])
                .save_file()
        });
    }

    pub(crate) fn export_otio_to(&mut self, timeline_id: TimelineId, path: &Path) {
        let measure = |title: &vv_core::TitleParams| vv_render::text::title_metrics(title);
        self.project_error =
            vv_core::export_otio(&self.project, timeline_id, path, Some(&measure))
            .err()
            .map(|e| t!("project.otio_export_failed", error = e).into_owned());
    }

    pub(crate) fn mark_saved(&mut self) {
        self.saved_generation = self.history.generation();
        self.unsaved_media = false;
    }

    pub(crate) fn has_unsaved_changes(&self) -> bool {
        self.unsaved_media || self.history.generation() != self.saved_generation
    }

    /// Name of the current project, without extension.
    pub(crate) fn project_label(&self) -> String {
        self.current_project_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| t!("project.untitled").into_owned())
    }

    pub(crate) fn sync_window_title(&mut self, ctx: &egui::Context) {
        let title = format!(
            "{}{} — Venturi",
            self.project_label(),
            if self.has_unsaved_changes() { "*" } else { "" },
        );
        if title != self.window_title {
            self.window_title = title.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
    }

    /// Opens or imports a project, asking first whether to save the changes.
    pub(crate) fn request_project_switch(&mut self, switch: ProjectSwitch) {
        if self.has_unsaved_changes() {
            self.pending_project_switch = Some(switch);
        } else {
            self.run_project_switch(switch);
        }
    }

    pub(crate) fn run_project_switch(&mut self, switch: ProjectSwitch) {
        match switch {
            ProjectSwitch::Open => self.open_project_dialog(),
            ProjectSwitch::OpenRecent(path) => self.load_project_from(path),
            ProjectSwitch::ImportOtio => self.import_otio_dialog(),
            ProjectSwitch::Quit => self.quit_confirmed = true,
        }
    }

    /// Closing the window is suspended until the user answers
    /// "save the changes?"; then it is requested again.
    pub(crate) fn handle_close_request(&mut self, ctx: &egui::Context) {
        if self.quit_confirmed {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.has_unsaved_changes() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending_project_switch = Some(ProjectSwitch::Quit);
            ctx.request_repaint();
        }
    }

    pub(crate) fn show_unsaved_changes_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(switch) = self.pending_project_switch.clone() else {
            return;
        };
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("unsaved_changes")).show(ui.ctx(), |ui| {
            ui.heading(if switch == ProjectSwitch::Quit {
                t!("project.save_before_quit")
            } else {
                t!("project.save_changes")
            });
            ui.label(t!("project.unsaved_changes"));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(t!("common.save")).clicked() {
                    choice = Some(UnsavedChoice::Save);
                }
                if ui.button(t!("project.dont_save")).clicked() {
                    choice = Some(UnsavedChoice::Discard);
                }
                if ui.button(t!("common.cancel")).clicked() {
                    choice = Some(UnsavedChoice::Cancel);
                }
            });
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(UnsavedChoice::Cancel);
        }
        if let Some(choice) = choice {
            self.resolve_unsaved_changes(choice);
            ui.ctx().request_repaint();
        }
    }

    pub(crate) fn resolve_unsaved_changes(&mut self, choice: UnsavedChoice) {
        let Some(switch) = self.pending_project_switch.take() else {
            return;
        };
        match choice {
            UnsavedChoice::Save => {
                self.save_project();
                // Save cancelled or failed: better not to lose anything.
                if !self.has_unsaved_changes() {
                    self.run_project_switch(switch);
                }
            }
            UnsavedChoice::Discard => self.run_project_switch(switch),
            UnsavedChoice::Cancel => {}
        }
    }

    pub(crate) fn import_otio_dialog(&mut self) {
        self.spawn_file_dialog(DialogKind::ImportOtio, |dlg| {
            dlg.add_filter("OpenTimelineIO", &["otio"]).pick_file()
        });
    }

    /// Like "Open project", but from an `.otio`: Ctrl+S will ask where to
    /// save instead of overwriting the imported file. What was not
    /// imported ends up in `import_warnings`.
    pub(crate) fn import_otio_from(&mut self, path: &Path) {
        let measure = |title: &vv_core::TitleParams| vv_render::text::title_metrics(title);
        let imported = vv_core::import_otio(
            path,
            |media_path| {
                let meta = vv_media::probe_media(media_path).map_err(|e| e.to_string())?;
                Ok((meta, vv_media::content_fingerprint(media_path).unwrap_or(0)))
            },
            Some(&measure),
        );
        match imported {
            Ok(imported) => {
                self.replace_project(imported.project, None);
                self.import_warnings = imported.warnings.iter().map(otio_warning_text).collect();
            }
            Err(e) => self.project_error = Some(t!("project.otio_import_failed", error = e).into_owned()),
        }
    }

    pub(crate) fn open_project_dialog(&mut self) {
        self.spawn_file_dialog(DialogKind::OpenProject, |dlg| {
            dlg.add_filter(t!("file_filter.project"), &["vvproj"]).pick_file()
        });
    }

    /// Replaces the project and resets the UI state tied to the old one.
    pub(crate) fn load_project_from(&mut self, path: PathBuf) {
        match vv_core::load_project(&path) {
            Ok(project) => {
                self.remember_recent_project(path.clone());
                self.replace_project(project, Some(path));
            }
            Err(e) => self.project_error = Some(t!("project.open_failed", error = e).into_owned()),
        }
    }

    /// Updates the "Recent projects" list and persists it immediately, so it
    /// survives even a crash before a clean shutdown.
    pub(crate) fn remember_recent_project(&mut self, path: PathBuf) {
        self.settings.add_recent_project(path);
        if let Some(settings_path) = &self.settings_path {
            let _ = self.settings.save(settings_path);
        }
    }

    /// `path` is the file Ctrl+S will save to: `None` for a project not
    /// coming from a `.vvproj` (OTIO import).
    pub(crate) fn replace_project(&mut self, project: vv_core::Project, path: Option<PathBuf>) {
        self.timeline_id = project.timelines.keys().next();
        self.project = project;
        self.history = vv_core::History::default();
        self.timeline_state = timeline_ui::TimelineState::default();
        self.import_warnings.clear();
        self.preview_meta = None;
        self.preview_error = None;
        self.last_viewer_frame_kind = None;
        self.browsing_render_ahead = None;
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
        self.current_project_path = path;
        self.project_error = None;
        self.mark_saved();
        // Project replaced outside the history: `sync_render_ahead` would not
        // notice.
        if let Some(timeline_id) = self.timeline_id {
            self.spawn_render_ahead_if_needed(timeline_id);
            if let Some(render_ahead) = &self.render_ahead {
                render_ahead.update_project(&self.project, timeline_id);
            }
        }
        self.render_ahead_generation = self.history.generation();
        for item in self.project.media_pool.values_mut() {
            // Projects saved before `MediaMeta::audio_streams`.
            if item.compound.is_none() && item.meta.has_audio && item.meta.audio_streams == 0 {
                item.meta.audio_streams =
                    vv_media::audio_streams(&item.path).map_or(1, |s| s.len() as u16);
            }
        }
        let media_ids: Vec<MediaId> = self.project.media_pool.keys().collect();
        for media_id in media_ids {
            self.enqueue_media_background_jobs(media_id);
        }
    }

    /// Opens the settings window: the export starts only from there
    /// (`run_export`).
    pub(crate) fn start_export(&mut self) {
        // The button is disabled during an export, but Ctrl+Shift+E is not.
        if self.timeline_id.is_none() || self.export.is_some() || self.export_dialog.is_some() {
            return;
        }
        let settings = self.last_export_settings.clone().unwrap_or_else(|| {
            export::ExportSettings::preferred(export_dialog::default_output_path(
                self.current_project_path.as_deref(),
            ))
        });
        self.export_dialog = Some(export_dialog::ExportDialog::new(settings));
    }

    pub(crate) fn show_export_dialog(&mut self, ui: &mut egui::Ui) {
        let (Some(dialog), Some(timeline_id)) = (&mut self.export_dialog, self.timeline_id) else {
            return;
        };
        let timeline = &self.project.timelines[timeline_id];
        let total_frames = timeline.total_frames();
        let marks = &self.timeline_state.export_marks;
        let info = export_dialog::TimelineInfo {
            resolution: timeline.resolution,
            fps: timeline.fps,
            total_frames,
            marks: (!marks.is_full(total_frames)).then(|| marks.resolve(total_frames)),
            has_audio: timeline
                .tracks_of_kind(TrackKind::Audio)
                .any(|(_, t)| !t.clips.is_empty()),
        };
        match dialog.show(ui.ctx(), &info) {
            export_dialog::ExportDialogAction::None => {}
            export_dialog::ExportDialogAction::Cancel => self.export_dialog = None,
            export_dialog::ExportDialogAction::Export { settings, range } => {
                self.export_dialog = None;
                self.last_export_settings = Some(settings.clone());
                self.run_export(timeline_id, settings, range);
            }
        }
    }

    /// Exports on a thread using a copy of the project: editing can
    /// continue.
    pub(crate) fn run_export(
        &mut self,
        timeline_id: TimelineId,
        settings: export::ExportSettings,
        range: std::ops::Range<FrameIdx>,
    ) {
        self.pause_proxies_for_export();

        let project = self.project.clone();
        let progress = std::sync::Arc::new(Mutex::new(export::ExportProgress::default()));
        let cancel = std::sync::Arc::new(AtomicBool::new(false));

        let thread_progress = progress.clone();
        let thread_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            let result = export::export_timeline(
                &project,
                timeline_id,
                &settings,
                range,
                &thread_progress,
                &thread_cancel,
            );
            // Errors and cancellation close `progress` too: the UI reads only that.
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

    pub(crate) fn show_import_warnings(&mut self, ui: &mut egui::Ui) {
        if self.import_warnings.is_empty() {
            return;
        }
        let mut close = false;
        egui::Window::new(t!("project.import_warnings", count = self.import_warnings.len()))
            .id(egui::Id::new("import_warnings"))
            .collapsible(true)
            .default_width(480.0)
            .show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for warning in &self.import_warnings {
                        ui.label(warning);
                    }
                });
                ui.separator();
                close = ui.button(t!("common.close")).clicked();
            });
        if close {
            self.import_warnings.clear();
        }
    }

    pub(crate) fn show_relink_message(&mut self, ui: &mut egui::Ui) {
        let Some(message) = &self.relink_message else {
            return;
        };
        let mut close = false;
        egui::Window::new("Relink media")
            .collapsible(false)
            .default_width(360.0)
            .show(ui.ctx(), |ui| {
                ui.label(message.as_str());
                ui.separator();
                close = ui.button(t!("common.close")).clicked();
            });
        if close {
            self.relink_message = None;
        }
    }

    pub(crate) fn show_export_progress(&mut self, ui: &mut egui::Ui) {
        let Some(state) = &self.export else {
            return;
        };

        let (current, total, done, error, elapsed) = {
            let p = state.progress.lock().unwrap();
            (p.current_frame, p.total_frames, p.done, p.error.clone(), p.elapsed)
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
                        .text(t!("project.export_progress", current = current, total = total))
                        .animate(!done),
                );
                if let Some(err) = &error {
                    ui.colored_label(egui::Color32::RED, err);
                } else if done {
                    ui.label(t!("project.export_done", elapsed = format_elapsed(elapsed)));
                }
                ui.horizontal(|ui| {
                    if !done && ui.button(t!("common.cancel")).clicked() {
                        state.cancel.store(true, Ordering::Relaxed);
                    }
                    if done && ui.button(t!("common.close")).clicked() {
                        should_close = true;
                    }
                });
            });

        // Without input egui does not redraw: the bar would stay frozen.
        if !done {
            ui.ctx().request_repaint();
        }

        if done {
            self.resume_proxies_after_export();
        }

        if should_close && let Some(state) = self.export.take() {
            let _ = state.handle.join();
        }
    }

    /// Applies the "use proxy" toggle and the quality: off (or a different
    /// quality) also stops the generation in progress (the worker is thrown
    /// away, the partial encode discarded), on restarts it for the media that
    /// do not have a proxy yet.
    pub(crate) fn apply_proxy_settings(&mut self) {
        let proxy = self.settings.proxy();
        for render_ahead in self.render_aheads() {
            render_ahead.set_proxy(proxy);
        }
        if self.proxy_worker.as_ref().map(|w| w.quality()) != proxy {
            self.proxy_worker = None;
            self.proxy_paused_for_export = false;
        }
        let Some(quality) = proxy else {
            return;
        };
        let media: Vec<(PathBuf, u64, u64)> = self
            .project
            .media_pool
            .iter()
            .filter(|(_, item)| {
                item.compound.is_none() && item.meta.has_video && !item.meta.is_image()
            })
            .map(|(_, item)| {
                (item.path.clone(), item.content_hash, item.meta.duration_frames.max(0) as u64)
            })
            .collect();
        if media.is_empty() {
            return;
        }
        let worker = self.proxy_worker.get_or_insert_with(|| proxy_worker::ProxyWorker::spawn(quality));
        for (path, content_hash, frames) in media {
            worker.enqueue(path, content_hash, frames);
        }
    }

    /// Pauses the proxies during the export: encoding a proxy slows it down a lot.
    /// A pause already chosen by the user must not be cancelled at the end of the export.
    pub(crate) fn pause_proxies_for_export(&mut self) {
        let Some(worker) = &self.proxy_worker else {
            return;
        };
        if worker.is_paused() {
            return;
        }
        worker.set_paused(true);
        self.proxy_paused_for_export = true;
    }

    /// Resumes the proxies if the export paused them. Idempotent.
    pub(crate) fn resume_proxies_after_export(&mut self) {
        if !self.proxy_paused_for_export {
            return;
        }
        self.proxy_paused_for_export = false;
        if let Some(worker) = &self.proxy_worker {
            worker.set_paused(false);
        }
    }

    /// Asks for a base directory and relinks the selected media. The selection
    /// is fixed now: the dialog comes back whenever it wants.
    pub(crate) fn relink_media_dialog(&mut self) {
        if self.media_pool_state.selected.is_empty() {
            return;
        }
        let targets: Vec<MediaId> = self.media_pool_state.selected.iter().copied().collect();
        self.spawn_file_dialog(DialogKind::RelinkMedia(targets), |dlg| dlg.pick_folder());
    }

    /// Relinks the media of `targets` that no longer exist at their path to
    /// a file with the same name under `base_dir`.
    pub(crate) fn relink_media(&mut self, base_dir: &Path, targets: &[MediaId]) {
        let mut index: Option<HashMap<std::ffi::OsString, PathBuf>> = None;
        let mut commands: Vec<Box<dyn vv_core::Command>> = Vec::new();
        let mut relinked_ids: Vec<MediaId> = Vec::new();
        let mut missing = 0usize;
        for &media_id in targets {
            let Some(item) = self.project.media_pool.get(media_id) else {
                continue;
            };
            if item.compound.is_some() || item.path.exists() {
                continue;
            }
            let Some(file_name) = item.path.file_name() else {
                continue;
            };
            let found = index
                .get_or_insert_with(|| index_media_by_filename(base_dir))
                .get(file_name)
                .cloned();
            let Some(found) = found else {
                missing += 1;
                continue;
            };
            let content_hash = vv_media::content_fingerprint(&found).unwrap_or(0);
            commands.push(Box::new(vv_core::SetMediaPath::new(media_id, found, content_hash))
                as Box<dyn vv_core::Command>);
            relinked_ids.push(media_id);
        }
        self.relink_message = Some(if commands.is_empty() {
            t!("project.relink_none").into_owned()
        } else if missing == 0 {
            t!("project.relink_done", count = commands.len()).into_owned()
        } else {
            t!("project.relink_partial", count = commands.len(), missing = missing).into_owned()
        });
        if commands.is_empty() {
            return;
        }
        self.history.do_command(
            &mut self.project,
            Box::new(vv_core::CompositeCommand::new(vv_core::CommandLabel::RelinkMedia, commands)),
        );
        self.unsaved_media = true;
        for media_id in relinked_ids {
            self.enqueue_media_background_jobs(media_id);
        }
    }
}

fn otio_warning_text(warning: &vv_core::OtioWarning) -> String {
    use vv_core::OtioWarning as W;
    match warning {
        W::EffectIgnored { effect, clips } => t!("otio.effect_ignored", effect = effect, clips = clips),
        W::SpeedNotApplied { clip, percent } => {
            t!("otio.speed_not_applied", clip = clip, percent = percent)
        }
        W::EffectPartlyIgnored { effect, clips } => {
            t!("otio.effect_partly_ignored", effect = effect, clips = clips)
        }
        W::UnsupportedInStack { schema } => t!("otio.unsupported_in_stack", schema = schema),
        W::TrackKindIgnored { kind } => {
            t!("otio.track_kind_ignored", kind = kind.as_deref().unwrap_or("?"))
        }
        W::TransitionIgnored => t!("otio.transition_ignored"),
        W::UnsupportedItem { schema } => t!("otio.unsupported_item", schema = schema),
        W::ClipWithoutDuration { clip } => t!("otio.clip_without_duration", clip = clip),
        W::ClipDisabled { clip } => t!("otio.clip_disabled", clip = clip),
        W::ClipShorterThanAFrame { clip } => t!("otio.clip_too_short", clip = clip),
        W::AudioOnlyOnVideoTrack { clip } => t!("otio.audio_only_on_video_track", clip = clip),
        W::UnsupportedReference { clip, schema } => {
            t!("otio.unsupported_reference", clip = clip, schema = schema)
        }
        W::UnsupportedUrl { url } => t!("otio.unsupported_url", url = url),
        W::MediaUnreadable { path, error } => {
            t!("otio.media_unreadable", path = path.display(), error = error)
        }
    }
    .into_owned()
}

fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..60 => format!("{:.1} s", d.as_secs_f32()),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m {:02}s", secs / 3600, secs / 60 % 60, secs % 60),
    }
}
