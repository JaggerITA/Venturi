//! Window of the forced relink: criteria, search progress, then the files
//! found for the user to accept all, some or none of.

use super::*;
use forced_relink::{Criteria, Criterion, Match, Reference, RunningSearch};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub(crate) struct ForcedRelinkDialog {
    base_dir: PathBuf,
    references: Arc<Vec<forced_relink::Reference>>,
    criteria: Criteria,
    phase: Phase,
}

enum Phase {
    Setup,
    Searching(RunningSearch),
    /// One row per reference, same order.
    Results(Vec<ResultRow>),
}

pub(crate) struct ResultRow {
    matches: Vec<Match>,
    chosen: usize,
    selected: bool,
}

enum Action {
    None,
    Search,
    Back,
    Apply,
    Close,
}

impl VenturiApp {
    pub(crate) fn open_forced_relink(&mut self, base_dir: PathBuf, media: &[MediaId]) {
        let references: Vec<Reference> = media
            .iter()
            .filter_map(|&media_id| {
                let item = self.project.media_pool.get(media_id)?;
                let waveform = item
                    .meta
                    .has_audio
                    .then(|| vv_media::load_waveform(item.content_hash, 0))
                    .flatten()
                    .map(|w| w.peaks);
                Some(Reference {
                    media_id,
                    path: item.path.clone(),
                    meta: item.meta.clone(),
                    waveform,
                })
            })
            .collect();
        if references.is_empty() {
            return;
        }
        self.forced_relink = Some(ForcedRelinkDialog {
            base_dir,
            references: Arc::new(references),
            criteria: self.forced_relink_criteria.clone(),
            phase: Phase::Setup,
        });
    }

    pub(crate) fn show_forced_relink_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.forced_relink else {
            return;
        };
        if let Phase::Searching(search) = &dialog.phase
            && search.handle.is_finished()
        {
            let Phase::Searching(search) = std::mem::replace(&mut dialog.phase, Phase::Setup)
            else {
                unreachable!()
            };
            if let Ok(Some(found)) = search.handle.join() {
                dialog.phase = Phase::Results(
                    found
                        .into_iter()
                        .map(|matches| ResultRow {
                            selected: !matches.is_empty(),
                            matches,
                            chosen: 0,
                        })
                        .collect(),
                );
            }
        }

        let mut open = true;
        let mut action = Action::None;
        egui::Window::new(t!("relink.force_title"))
            .id(egui::Id::new("forced_relink_dialog"))
            .open(&mut open)
            .collapsible(false)
            .default_width(480.0)
            .show(ctx, |ui| {
                action = match &mut dialog.phase {
                    Phase::Setup => show_setup(
                        ui,
                        &dialog.base_dir,
                        &dialog.references,
                        &mut dialog.criteria,
                    ),
                    Phase::Searching(search) => show_searching(ui, search),
                    Phase::Results(rows) => {
                        show_results(ui, &dialog.base_dir, &dialog.references, rows)
                    }
                };
            });
        if !open {
            action = Action::Close;
        }
        if matches!(dialog.phase, Phase::Searching(_)) {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        match action {
            Action::None => {}
            Action::Search => {
                self.forced_relink_criteria = dialog.criteria.clone();
                dialog.phase = Phase::Searching(forced_relink::spawn_search(
                    dialog.references.clone(),
                    dialog.base_dir.clone(),
                    dialog.criteria.clone(),
                ));
            }
            Action::Back => {
                if let Phase::Searching(search) = &dialog.phase {
                    search.progress.cancel.store(true, Ordering::Relaxed);
                }
                dialog.phase = Phase::Setup;
            }
            Action::Close => {
                if let Phase::Searching(search) = &dialog.phase {
                    search.progress.cancel.store(true, Ordering::Relaxed);
                }
                self.forced_relink = None;
            }
            Action::Apply => {
                let dialog = self.forced_relink.take().unwrap();
                let Phase::Results(rows) = dialog.phase else {
                    return;
                };
                let relinks = chosen_relinks(&dialog.references, rows);
                self.apply_relinks(relinks);
            }
        }
    }
}

pub(crate) fn chosen_relinks(
    references: &[Reference],
    rows: Vec<ResultRow>,
) -> Vec<(MediaId, PathBuf, Option<vv_core::MediaMeta>)> {
    references
        .iter()
        .zip(rows)
        .filter(|(_, row)| row.selected)
        .filter_map(|(reference, mut row)| {
            (row.chosen < row.matches.len()).then(|| {
                let chosen = row.matches.swap_remove(row.chosen);
                (reference.media_id, chosen.path, chosen.meta)
            })
        })
        .collect()
}

fn criterion_label(criterion: Criterion) -> Cow<'static, str> {
    match criterion {
        Criterion::Name => t!("relink.criterion_name"),
        Criterion::Extension => t!("relink.criterion_extension"),
        Criterion::Duration => t!("relink.criterion_duration"),
        Criterion::Frames => t!("relink.criterion_frames"),
        Criterion::Size => t!("relink.criterion_size"),
        Criterion::Codec => t!("relink.criterion_codec"),
        Criterion::AspectRatio => t!("relink.criterion_aspect_ratio"),
        Criterion::Title => t!("relink.criterion_title"),
        Criterion::Artist => t!("relink.criterion_artist"),
        Criterion::Waveform => t!("relink.criterion_waveform"),
        Criterion::Tag => t!("relink.criterion_tag"),
    }
}

fn show_setup(
    ui: &mut egui::Ui,
    base_dir: &Path,
    references: &[Reference],
    criteria: &mut Criteria,
) -> Action {
    ui.label(t!(
        "relink.force_intro",
        count = references.len(),
        dir = base_dir.display()
    ));
    ui.add_space(4.0);
    egui::Grid::new("forced_relink_criteria")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for criterion in Criterion::ALL {
                let unknown = references
                    .iter()
                    .filter(|r| !criterion.known_for(r))
                    .count();
                let mut enabled = criteria.is_enabled(criterion);
                let checkbox = ui.add_enabled(
                    unknown < references.len(),
                    egui::Checkbox::new(&mut enabled, criterion_label(criterion)),
                );
                let checkbox = if unknown > 0 {
                    checkbox.on_hover_text(t!(
                        "relink.unknown_for",
                        count = unknown,
                        total = references.len()
                    ))
                } else {
                    checkbox
                };
                if checkbox.changed() {
                    if enabled {
                        criteria.enabled.insert(criterion);
                    } else {
                        criteria.enabled.remove(&criterion);
                    }
                }
                ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal(|ui| show_criterion_setting(ui, criterion, criteria));
                });
                ui.end_row();
            }
        });
    ui.separator();
    let mut action = Action::None;
    ui.horizontal(|ui| {
        if ui.button(t!("common.cancel")).clicked() {
            action = Action::Close;
        }
        if ui
            .add_enabled(
                criteria.is_searchable(),
                egui::Button::new(t!("relink.search")),
            )
            .clicked()
        {
            action = Action::Search;
        }
    });
    action
}

fn show_criterion_setting(ui: &mut egui::Ui, criterion: Criterion, criteria: &mut Criteria) {
    let tolerance = |ui: &mut egui::Ui, value: &mut u32, suffix: &str| {
        ui.label("±");
        ui.add(
            egui::DragValue::new(value)
                .range(0..=1_000_000)
                .suffix(suffix),
        );
    };
    match criterion {
        Criterion::Duration => tolerance(ui, &mut criteria.duration_tolerance_ms, " ms"),
        Criterion::Frames => tolerance(
            ui,
            &mut criteria.frames_tolerance,
            &t!("relink.frames_suffix"),
        ),
        Criterion::Size => tolerance(ui, &mut criteria.size_tolerance_kib, " KiB"),
        Criterion::Waveform => {
            ui.label("≥");
            ui.add(
                egui::DragValue::new(&mut criteria.waveform_min_percent)
                    .range(1..=100)
                    .suffix("%"),
            );
        }
        Criterion::Tag => {
            ui.add(
                egui::TextEdit::singleline(&mut criteria.tag_key)
                    .hint_text(t!("relink.tag_key"))
                    .desired_width(110.0),
            );
            ui.label("=");
            ui.add(
                egui::TextEdit::singleline(&mut criteria.tag_value)
                    .hint_text(t!("relink.tag_value"))
                    .desired_width(140.0),
            );
        }
        _ => {}
    }
}

fn show_searching(ui: &mut egui::Ui, search: &RunningSearch) -> Action {
    let done = search.progress.done.load(Ordering::Relaxed);
    let total = search.progress.total.load(Ordering::Relaxed);
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(t!("relink.searching", done = done, total = total));
    });
    if total > 0 {
        ui.add(egui::ProgressBar::new(done as f32 / total as f32));
    }
    ui.separator();
    let mut action = Action::None;
    if ui.button(t!("common.cancel")).clicked() {
        action = Action::Back;
    }
    action
}

fn show_results(
    ui: &mut egui::Ui,
    base_dir: &Path,
    references: &[Reference],
    rows: &mut [ResultRow],
) -> Action {
    let shown = |path: &Path| {
        path.strip_prefix(base_dir)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    ui.label(t!("relink.results_intro"));
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .max_height(320.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("forced_relink_results")
                .num_columns(2)
                .spacing([16.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    for (i, (reference, row)) in references.iter().zip(rows.iter_mut()).enumerate()
                    {
                        let name = reference.path.file_name().map_or_else(
                            || reference.path.display().to_string(),
                            |n| n.to_string_lossy().into_owned(),
                        );
                        ui.add_enabled(
                            !row.matches.is_empty(),
                            egui::Checkbox::new(&mut row.selected, name),
                        )
                        .on_hover_text(reference.path.display().to_string());
                        match row.matches.len() {
                            0 => {
                                ui.weak(t!("relink.no_match"));
                            }
                            1 => {
                                ui.label(shown(&row.matches[0].path));
                            }
                            _ => {
                                egui::ComboBox::from_id_salt(("forced_relink_choice", i))
                                    .selected_text(shown(&row.matches[row.chosen].path))
                                    .width(280.0)
                                    .show_ui(ui, |ui| {
                                        for (j, m) in row.matches.iter().enumerate() {
                                            ui.selectable_value(&mut row.chosen, j, shown(&m.path));
                                        }
                                    });
                            }
                        }
                        ui.end_row();
                    }
                });
        });
    ui.separator();
    let selectable = rows.iter().filter(|r| !r.matches.is_empty()).count();
    let selected = rows
        .iter()
        .filter(|r| r.selected && !r.matches.is_empty())
        .count();
    let mut action = Action::None;
    ui.horizontal(|ui| {
        if ui.button(t!("relink.select_all")).clicked() {
            for row in rows.iter_mut() {
                row.selected = !row.matches.is_empty();
            }
        }
        if ui.button(t!("relink.deselect_all")).clicked() {
            for row in rows.iter_mut() {
                row.selected = false;
            }
        }
    });
    ui.horizontal(|ui| {
        if ui.button(t!("relink.back")).clicked() {
            action = Action::Back;
        }
        if ui.button(t!("common.cancel")).clicked() {
            action = Action::Close;
        }
        if ui
            .add_enabled(
                selected > 0,
                egui::Button::new(t!("relink.apply", count = selected)),
            )
            .clicked()
        {
            action = Action::Apply;
        }
    });
    if selectable == 0 {
        ui.weak(t!("relink.nothing_found"));
    }
    action
}

#[cfg(test)]
#[path = "tests/forced_relink_dialog.rs"]
mod tests;
