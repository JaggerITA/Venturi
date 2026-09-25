//! Media pool and effects panels.

use super::*;

/// Progress ring; `None` = queued (background ring only).
pub(crate) fn proxy_progress_ring(ui: &mut egui::Ui, fraction: Option<f32>) -> egui::Response {
    const SIZE: f32 = 34.0;
    const STROKE: f32 = 3.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(SIZE, SIZE), egui::Sense::hover());
    let painter = ui.painter();
    let center = rect.center();
    let radius = (SIZE - STROKE) / 2.0;
    let track_color = ui.visuals().widgets.inactive.bg_fill;
    painter.circle_stroke(center, radius, egui::Stroke::new(STROKE, track_color));
    if let Some(fraction) = fraction {
        let fraction = fraction.clamp(0.0, 1.0);
        let segments = ((fraction * 48.0).ceil() as usize).max(1);
        let start = -std::f32::consts::FRAC_PI_2;
        let points: Vec<egui::Pos2> = (0..=segments)
            .map(|i| {
                let angle = start + std::f32::consts::TAU * fraction * i as f32 / segments as f32;
                center + radius * egui::vec2(angle.cos(), angle.sin())
            })
            .collect();
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(STROKE, ui.visuals().selection.bg_fill),
        ));
    }
    response
}

/// Label following the cursor while dragging a media.
pub(crate) fn show_drag_ghost(ui: &egui::Ui, id: egui::Id, label: &str) {
    let Some(pos) = ui.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    egui::Area::new(id.with("drag_ghost"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + egui::vec2(12.0, 12.0))
        .interactable(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(label);
            });
        });
}

pub(crate) fn effects_section_header(ui: &mut egui::Ui, title: &str) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, HEADER_HEIGHT), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, ui.visuals().widgets.inactive.weak_bg_fill);
    ui.painter().text(
        rect.left_center() + egui::vec2(6.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(13.0),
        ui.visuals().strong_text_color(),
    );
}

/// Effects panel entry: thumbnail on the left and name, draggable onto
/// the timeline.
pub(crate) fn effect_item(ui: &mut egui::Ui, generator: timeline_ui::Generator) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let id = ui.id().with(("effect_item", generator.label()));
    let resp = ui
        .interact(rect, id, egui::Sense::click_and_drag())
        .on_hover_text(t!("pool.drag_to_timeline"));
    let visuals = ui.visuals();
    let (bg, stroke) = if resp.hovered() || resp.dragged() {
        (visuals.widgets.hovered.weak_bg_fill, visuals.widgets.hovered.fg_stroke.color)
    } else {
        (visuals.widgets.inactive.weak_bg_fill, visuals.widgets.noninteractive.bg_stroke.color)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, bg);
    let thumb = egui::Rect::from_min_size(rect.min, egui::vec2(54.0, rect.height())).shrink(1.0);
    match generator {
        timeline_ui::Generator::SolidColor => {
            painter.rect_filled(thumb, 2.0, egui::Color32::from_rgb(106, 176, 204));
        }
        timeline_ui::Generator::Text => {
            painter.rect_filled(thumb, 2.0, egui::Color32::BLACK);
            painter.text(
                thumb.center(),
                egui::Align2::CENTER_CENTER,
                "Title",
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
        }
    }
    painter.rect_stroke(rect, 3.0, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    painter.text(
        egui::pos2(thumb.right() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        generator.label(),
        egui::FontId::proportional(13.0),
        visuals.text_color(),
    );
    resp.dnd_set_drag_payload(generator);
    if resp.dragged() {
        show_drag_ghost(ui, id, &generator.label());
    }
}

/// Filter entry in the Effects panel: dragged onto an existing video clip
/// (never onto empty space). The kind (`vv_core::FilterKind`) is the same one
/// saved in `EffectStack::filters`: no double representation between editor
/// and model.
pub(crate) fn filter_item(ui: &mut egui::Ui, filter: vv_core::FilterKind) {
    let label = timeline_ui::filter_label(filter);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let id = ui.id().with(("filter_item", &label));
    let resp = ui
        .interact(rect, id, egui::Sense::click_and_drag())
        .on_hover_text(t!("pool.drag_to_clip"));
    let visuals = ui.visuals();
    let (bg, stroke) = if resp.hovered() || resp.dragged() {
        (visuals.widgets.hovered.weak_bg_fill, visuals.widgets.hovered.fg_stroke.color)
    } else {
        (visuals.widgets.inactive.weak_bg_fill, visuals.widgets.noninteractive.bg_stroke.color)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, bg);
    let thumb = egui::Rect::from_min_size(rect.min, egui::vec2(54.0, rect.height())).shrink(1.0);
    painter.rect_filled(thumb, 2.0, egui::Color32::from_gray(40));
    timeline_ui::paint_gear_icon(&painter, thumb.center(), 11.0, egui::Color32::from_gray(220));
    painter.rect_stroke(rect, 3.0, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    painter.text(
        egui::pos2(thumb.right() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label.as_ref(),
        egui::FontId::proportional(13.0),
        visuals.text_color(),
    );
    resp.dnd_set_drag_payload(filter);
    if resp.dragged() {
        show_drag_ghost(ui, id, &label);
    }
}

/// Transition entry in the Effects panel: dragged near an edge (left or
/// right) of an existing video clip, never at its center nor onto empty
/// space. The kind (`vv_core::TransitionKind`) is the same one saved in
/// `EffectStack::transition_in`/`transition_out`.
pub(crate) fn transition_item(ui: &mut egui::Ui, kind: vv_core::TransitionKind) {
    let label = timeline_ui::transition_kind_label(kind);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 30.0), egui::Sense::hover());
    let id = ui.id().with(("transition_item", &label));
    let resp = ui
        .interact(rect, id, egui::Sense::click_and_drag())
        .on_hover_text(t!("pool.drag_to_clip_edge"));
    let visuals = ui.visuals();
    let (bg, stroke) = if resp.hovered() || resp.dragged() {
        (visuals.widgets.hovered.weak_bg_fill, visuals.widgets.hovered.fg_stroke.color)
    } else {
        (visuals.widgets.inactive.weak_bg_fill, visuals.widgets.noninteractive.bg_stroke.color)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, bg);
    let thumb = egui::Rect::from_min_size(rect.min, egui::vec2(54.0, rect.height())).shrink(1.0);
    painter.rect_filled(thumb, 2.0, egui::Color32::from_gray(40));
    timeline_ui::paint_bracket_icon(&painter, thumb.center(), thumb.height() * 0.6, vv_core::FadeEdge::Out, egui::Color32::from_gray(220));
    painter.rect_stroke(rect, 3.0, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    painter.text(
        egui::pos2(thumb.right() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label.as_ref(),
        egui::FontId::proportional(13.0),
        visuals.text_color(),
    );
    resp.dnd_set_drag_payload(kind);
    if resp.dragged() {
        show_drag_ghost(ui, id, &label);
    }
}

/// Width of the "Duration" column: the same in the header and in the rows,
/// so they stay aligned.
pub(crate) const DURATION_COL_W: f32 = 64.0;

/// Free background on the sides and below the items: always somewhere to
/// right click, even with the pool full.
pub(crate) const SIDE_PAD: i8 = 6;
pub(crate) const BOTTOM_PAD: f32 = 28.0;

/// Height of the media pool header bar.
pub(crate) const HEADER_HEIGHT: f32 = 22.0;

/// Column header of the media pool: every cell is clickable in full
/// (not just the text), as in a file manager list.
pub(crate) fn media_pool_header(ui: &mut egui::Ui, state: &mut media_pool::MediaPoolState) {
    use media_pool::SortKey;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, HEADER_HEIGHT),
        egui::Sense::hover(),
    );
    let duration_w = DURATION_COL_W.min(width);
    let (name_rect, duration_rect) = (
        egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.right() - duration_w, rect.bottom())),
        egui::Rect::from_min_max(egui::pos2(rect.right() - duration_w, rect.top()), rect.right_bottom()),
    );
    let sort = state.sort;
    for (key, label, cell) in [
        (SortKey::Name, t!("pool.name"), name_rect),
        (SortKey::Duration, t!("pool.duration"), duration_rect),
    ] {
        let resp = ui.interact(
            cell,
            ui.id().with(("media_pool_header", key as u8)),
            egui::Sense::click(),
        );
        let active = sort.key == key;
        let bg = if resp.hovered() {
            ui.visuals().widgets.hovered.weak_bg_fill
        } else if active {
            ui.visuals().widgets.active.weak_bg_fill
        } else {
            ui.visuals().widgets.inactive.weak_bg_fill
        };
        ui.painter().rect_filled(cell, 0.0, bg);
        let text_color = ui.visuals().strong_text_color();
        ui.painter().text(
            cell.left_center() + egui::vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.0),
            text_color,
        );
        if active {
            // Small triangle drawn by hand instead of a character: the ones
            // in system fonts are tall and pointy, this one is squashed.
            let c = egui::pos2(cell.right() - 10.0, cell.center().y);
            let (w, h) = (4.5, 2.5);
            let points = if sort.ascending {
                vec![
                    egui::pos2(c.x - w, c.y + h),
                    egui::pos2(c.x + w, c.y + h),
                    egui::pos2(c.x, c.y - h),
                ]
            } else {
                vec![
                    egui::pos2(c.x - w, c.y - h),
                    egui::pos2(c.x + w, c.y - h),
                    egui::pos2(c.x, c.y + h),
                ]
            };
            ui.painter().add(egui::Shape::convex_polygon(
                points,
                text_color,
                egui::Stroke::NONE,
            ));
        }
        if resp.clicked() {
            state.toggle_sort(key);
        }
    }
}

pub(crate) fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

/// Name text field of the item being renamed: `Some(Some(name))` to
/// confirm, `Some(None)` to cancel, `None` while still editing.
fn rename_field(ui: &mut egui::Ui, rename: &mut media_pool::Rename) -> Option<Option<String>> {
    let edit_id = ui.id().with("media_pool_rename");
    let mut output = egui::TextEdit::singleline(&mut rename.text)
        .id(edit_id)
        .desired_width(ui.available_width() - DURATION_COL_W)
        .show(ui);
    if rename.just_started {
        rename.just_started = false;
        output.response.request_focus();
        let end = egui::text::CCursor::new(rename.text.chars().count());
        output
            .state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
        output.state.store(ui.ctx(), edit_id);
    }
    if !output.response.lost_focus() {
        return None;
    }
    let name = rename.text.trim();
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) || name.is_empty() {
        Some(None)
    } else {
        Some(Some(name.to_string()))
    }
}

/// Thumbnail of a timeline in the pool: a film strip.
fn paint_film_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: egui::Color32,
    hole_color: egui::Color32,
) {
    let film = egui::Rect::from_center_size(rect.center(), egui::vec2(44.0, 28.0));
    painter.rect_filled(film, 2.0, color);
    const HOLES: usize = 6;
    let step = film.width() / HOLES as f32;
    for i in 0..HOLES {
        let x = film.left() + step * (i as f32 + 0.5);
        for y in [film.top() + 3.5, film.bottom() - 3.5] {
            painter.rect_filled(
                egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(4.0, 3.0)),
                0.5,
                hole_color,
            );
        }
    }
    let frames = film.shrink2(egui::vec2(3.0, 8.0));
    let gap = 3.0;
    let frame_w = (frames.width() - gap) / 2.0;
    for i in 0..2 {
        let left = frames.left() + i as f32 * (frame_w + gap);
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(left, frames.top()),
                egui::vec2(frame_w, frames.height()),
            ),
            1.0,
            hole_color,
        );
    }
}

impl VenturiApp {
    fn start_rename(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else { return };
        self.media_pool_state.renaming = Some(media_pool::Rename {
            media_id,
            text: file_label(&item.path),
            just_started: true,
        });
    }

    /// Not in the history, like creating a timeline.
    fn rename_timeline(&mut self, media_id: MediaId, name: String) {
        let unchanged = self
            .project
            .media_pool
            .get(media_id)
            .is_some_and(|item| file_label(&item.path) == name);
        if !unchanged {
            self.project.rename_timeline(media_id, name);
            self.unsaved_media = true;
        }
    }

    /// The copy is called "<name> copy", "<name> copy 2", ... and ends up
    /// selected.
    pub(crate) fn duplicate_timeline(&mut self, media_id: MediaId) {
        let Some(item) = self.project.media_pool.get(media_id) else { return };
        let base = format!("{} {}", file_label(&item.path), t!("pool.copy_suffix"));
        let taken: std::collections::HashSet<String> = self
            .project
            .media_pool
            .values()
            .filter(|m| m.compound.is_some())
            .map(|m| file_label(&m.path))
            .collect();
        let name = std::iter::once(base.clone())
            .chain((2..).map(|n| format!("{base} {n}")))
            .find(|name| !taken.contains(name))
            .expect("infinite candidates");
        if let Some(copy) = self.project.duplicate_timeline(media_id, name) {
            self.unsaved_media = true;
            self.media_pool_state.select_only([copy]);
        }
    }

    /// Contents of the Media pool section in the left column.
    pub(crate) fn show_media_pool(&mut self, ui: &mut egui::Ui, preview_action: &mut Option<MediaId>) {
        if let Some(worker) = &self.proxy_worker {
            let progress = worker.progress();
            let paused = worker.is_paused();
            if progress.finished < progress.total {
                ui.horizontal(|ui| {
                    let label = if paused { t!("pool.resume") } else { t!("pool.pause") };
                    if ui
                        .small_button(label)
                        .on_hover_text(t!("pool.proxy_generation"))
                        .clicked()
                    {
                        worker.set_paused(!paused);
                    }
                    ui.add(
                        egui::ProgressBar::new(progress.fraction)
                            .text(format!("Proxy {}/{}", progress.finished, progress.total)),
                    );
                });
                if !paused {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                }
            }
        }
        media_pool_header(ui, &mut self.media_pool_state);
        // `auto_shrink` off: the panel must fill the assigned width,
        // otherwise its resize springs back.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let items: Vec<(MediaId, String, vv_core::MediaMeta, u64, bool)> = self
                    .project
                    .media_pool
                    .iter()
                    .map(|(id, item)| {
                        (
                            id,
                            file_label(&item.path),
                            item.meta.clone(),
                            item.content_hash,
                            item.compound.is_some(),
                        )
                    })
                    .collect();
                let mut items = items;
                media_pool::sort_items(
                    &mut items,
                    self.media_pool_state.sort,
                    |(_, label, ..)| label.as_str(),
                    |(_, _, meta, ..)| {
                        meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9)
                    },
                );
                let order: Vec<MediaId> = items.iter().map(|(id, ..)| *id).collect();
                let drags: Vec<timeline_ui::MediaDrag> = items
                    .iter()
                    .map(|(id, _, meta, ..)| timeline_ui::MediaDrag::whole(*id, meta))
                    .collect();
                // Interacted with before the items: in egui the last one wins, so a click
                // on an item does not start the selection rectangle. The clip rect
                // is the viewport: `available_rect_before_wrap` is viewport-sized
                // but scrolls away with the content.
                let bg = ui.interact(
                    ui.clip_rect(),
                    ui.id().with("media_pool_bg"),
                    egui::Sense::click_and_drag(),
                );
                bg.context_menu(|ui| {
                    ui.menu_button(t!("pool.timelines"), |ui| {
                        if ui.button(t!("menu.import_otio")).clicked() {
                            self.request_project_switch(ProjectSwitch::ImportOtio);
                            ui.close();
                        }
                        if ui.button(t!("pool.new_timeline")).clicked() {
                            self.open_new_timeline_dialog();
                            ui.close();
                        }
                    });
                });
                let mut item_rects: Vec<(MediaId, egui::Rect)> = Vec::new();
                // The items leave a strip of background on the sides (and
                // `BOTTOM_PAD` below): with a full pool there would be no
                // free spot left to right click on.
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(SIDE_PAD, 0))
                    .show(ui, |ui| {
                for (id, label, meta, content_hash, is_timeline) in items {
                    let proxy_state = self
                        .proxy_worker
                        .as_ref()
                        .and_then(|w| w.state(content_hash));
                    let thumbnail = if is_timeline {
                        None
                    } else {
                        self.thumbnails.get(&content_hash).cloned().flatten()
                    };
                    let renaming = self
                        .media_pool_state
                        .renaming
                        .as_mut()
                        .filter(|r| r.media_id == id);
                    let is_renaming = renaming.is_some();
                    let mut rename_done = None;
                    let mut label_rect = egui::Rect::NOTHING;
                    let group_resp = ui
                        .group(|ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let thumb_size = egui::vec2(64.0, 36.0);
                                match &thumbnail {
                                    Some(texture) => {
                                        let tex_size = texture.size_vec2();
                                        let scale = (thumb_size.x / tex_size.x)
                                            .min(thumb_size.y / tex_size.y);
                                        let (rect, _) = ui.allocate_exact_size(
                                            thumb_size,
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(rect, 2.0, egui::Color32::BLACK);
                                        egui::Image::new(texture)
                                            .fit_to_exact_size(tex_size * scale)
                                            .paint_at(
                                                ui,
                                                egui::Rect::from_center_size(
                                                    rect.center(),
                                                    tex_size * scale,
                                                ),
                                            );
                                    }
                                    None => {
                                        let (rect, _) = ui.allocate_exact_size(
                                            thumb_size,
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            rect,
                                            2.0,
                                            ui.visuals().extreme_bg_color,
                                        );
                                        if is_timeline {
                                            paint_film_icon(
                                                ui.painter(),
                                                rect,
                                                ui.visuals().weak_text_color(),
                                                ui.visuals().extreme_bg_color,
                                            );
                                        } else if !meta.has_video {
                                            ui.painter().text(
                                                rect.center(),
                                                egui::Align2::CENTER_CENTER,
                                                "🔊",
                                                egui::FontId::proportional(18.0),
                                                ui.visuals().weak_text_color(),
                                            );
                                        }
                                    }
                                }
                                ui.vertical(|ui| {
                                    match renaming {
                                        Some(rename) => {
                                            rename_done = rename_field(ui, rename);
                                        }
                                        None => label_rect = ui.label(&label).rect,
                                    }
                                    ui.small(if meta.is_image() {
                                        t!("pool.meta_image", width = meta.width, height = meta.height)
                                    } else if meta.has_video {
                                        format!(
                                            "{}x{} · {:.2}fps · {}",
                                            meta.width,
                                            meta.height,
                                            meta.fps.as_f64(),
                                            if meta.has_audio { t!("pool.audio") } else { t!("pool.muted") }
                                        )
                                        .into()
                                    } else {
                                        t!(
                                            "pool.meta_audio",
                                            rate = meta.sample_rate,
                                            channels = meta.channels
                                        )
                                    });
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        // An image has no real duration.
                                        let duration_label = if meta.is_image() {
                                            "—".to_string()
                                        } else {
                                            format_duration(meta.duration_frames, meta.fps.as_f64())
                                        };
                                        ui.add_sized(
                                            egui::vec2(DURATION_COL_W, ui.available_height()),
                                            egui::Label::new(
                                                egui::RichText::new(duration_label).monospace(),
                                            ),
                                        );
                                        match proxy_state {
                                        Some(proxy_worker::ProxyState::Generating(f)) => {
                                            proxy_progress_ring(ui, Some(f))
                                                .on_hover_text(t!("pool.proxy_progress", percent = format!("{:.0}", f * 100.0)));
                                        }
                                        Some(proxy_worker::ProxyState::Queued) => {
                                            proxy_progress_ring(ui, None)
                                                .on_hover_text(t!("pool.proxy_queued"));
                                        }
                                        Some(proxy_worker::ProxyState::Failed) => {
                                            ui.colored_label(egui::Color32::RED, "!")
                                                .on_hover_text(t!("pool.proxy_failed"));
                                        }
                                        _ => {}
                                        }
                                    },
                                );
                            });
                        })
                        .response;
                    if proxy_state == Some(proxy_worker::ProxyState::Ready) {
                        let rect = group_resp.rect.shrink(1.0);
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.0, rect.height())),
                            1.0,
                            timeline_ui::PROXY_COLOR,
                        );
                    }
                    // Double click: preview. Drag: adds the media onto the timeline.
                    let interact_id = ui.id().with("media_pool_item").with(id);
                    // While renaming, the item must not steal the clicks of
                    // the text field.
                    let sense = if is_renaming {
                        egui::Sense::hover()
                    } else {
                        egui::Sense::click_and_drag()
                    };
                    let resp = ui
                        .interact(group_resp.rect, interact_id, sense)
                        .on_hover_text(
                            t!("pool.item_hint"),
                        );
                    if let Some(new_name) = rename_done {
                        self.media_pool_state.renaming = None;
                        if let Some(name) = new_name {
                            self.rename_timeline(id, name);
                        }
                    }
                    item_rects.push((id, group_resp.rect));
                    if self.media_pool_state.selected.contains(&id) {
                        ui.painter().rect_stroke(
                            group_resp.rect,
                            4.0,
                            egui::Stroke::new(2.0, egui::Color32::WHITE),
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().rect_filled(
                            group_resp.rect,
                            4.0,
                            egui::Color32::from_white_alpha(18),
                        );
                    }
                    if resp.clicked() {
                        let modifiers = ui.input(|i| i.modifiers);
                        let on_name = is_timeline
                            && modifiers.is_none()
                            && self.media_pool_state.selected.len() == 1
                            && self.media_pool_state.selected.contains(&id)
                            && resp
                                .interact_pointer_pos()
                                .is_some_and(|pos| label_rect.contains(pos));
                        self.media_pool_state.click(id, modifiers, &order);
                        self.media_pool_state.rename_pending =
                            on_name.then(|| (id, ui.input(|i| i.time)));
                    }
                    // Right click outside the selection replaces it, like a drag.
                    if resp.secondary_clicked() && !self.media_pool_state.selected.contains(&id) {
                        self.media_pool_state.click(id, egui::Modifiers::NONE, &order);
                    }
                    resp.context_menu(|ui| {
                        if is_timeline {
                            if ui.button(t!("pool.duplicate_timeline")).clicked() {
                                self.duplicate_timeline(id);
                                ui.close();
                            }
                            if ui.button(t!("pool.rename")).clicked() {
                                self.start_rename(id);
                                ui.close();
                            }
                            ui.separator();
                        }
                        let count = self.media_pool_state.selected.len().max(1);
                        let label = if count > 1 {
                            t!("pool.relink_many", count = count)
                        } else {
                            t!("pool.relink_one")
                        };
                        if ui.button(label).clicked() {
                            self.relink_media_dialog();
                            ui.close();
                        }
                    });
                    // Dragging an item outside the selection replaces it
                    // with that item (as on the timeline, see
                    // `timeline_ui::drag_group_for`).
                    if resp.drag_started() {
                        self.media_pool_state.rename_pending = None;
                    }
                    if resp.drag_started() && !self.media_pool_state.selected.contains(&id) {
                        self.media_pool_state.click(id, egui::Modifiers::NONE, &order);
                    }
                    // Dragging an item of the selection drags the whole
                    // selection, in panel order: the timeline appends them
                    // one after the other.
                    let payload = if self.media_pool_state.selected.len() > 1
                        && self.media_pool_state.selected.contains(&id)
                    {
                        timeline_ui::MediaDragSet {
                            items: drags
                                .iter()
                                .filter(|d| {
                                    self.media_pool_state.selected.contains(&d.media_id)
                                })
                                .copied()
                                .collect(),
                        }
                    } else {
                        timeline_ui::MediaDragSet::one(timeline_ui::MediaDrag::whole(
                            id, &meta,
                        ))
                    };
                    let dragged_count = payload.items.len();
                    resp.dnd_set_drag_payload(payload);
                    if resp.double_clicked() {
                        self.media_pool_state.rename_pending = None;
                        // A compound clip (or a project timeline, see
                        // `MediaItem::compound`) opens as a top level timeline:
                        // from the pool there is no parent to stack in the
                        // breadcrumb. "Preview" makes no sense for it.
                        match self.project.media_pool.get(id).and_then(|m| m.compound) {
                            Some(timeline_id) => self.open_timeline(timeline_id),
                            None => *preview_action = Some(id),
                        }
                    }
                    if resp.dragged() {
                        let ghost = if dragged_count > 1 {
                            t!("pool.items", count = dragged_count).into_owned()
                        } else {
                            label.clone()
                        };
                        show_drag_ghost(ui, interact_id, &ghost);
                    }
                }
                    });
                ui.add_space(BOTTOM_PAD);
                if let Some((id, clicked_at)) = self.media_pool_state.rename_pending {
                    let wait = ui.ctx().options(|o| o.input_options.max_double_click_delay);
                    let elapsed = ui.input(|i| i.time) - clicked_at;
                    if elapsed > wait {
                        self.media_pool_state.rename_pending = None;
                        self.start_rename(id);
                    } else {
                        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(
                            wait - elapsed,
                        ));
                    }
                }

                if bg.drag_started() {
                    if let Some(pos) = bg.interact_pointer_pos() {
                        self.media_pool_state.marquee = Some((pos, pos));
                    }
                } else if bg.dragged() {
                    if let (Some((_, end)), Some(pos)) =
                        (&mut self.media_pool_state.marquee, bg.interact_pointer_pos())
                    {
                        *end = pos;
                    }
                } else if bg.drag_stopped() {
                    if let Some((start, end)) = self.media_pool_state.marquee.take() {
                        let rect = egui::Rect::from_two_pos(start, end);
                        let hits = item_rects
                            .iter()
                            .filter(|(_, r)| r.intersects(rect))
                            .map(|(id, _)| *id);
                        self.media_pool_state.select_only(hits);
                    }
                } else if bg.clicked() {
                    self.media_pool_state.rename_pending = None;
                    self.media_pool_state.clear();
                }
                if let Some((start, end)) = self.media_pool_state.marquee {
                    let rect = egui::Rect::from_two_pos(start, end);
                    ui.painter().rect_filled(
                        rect,
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(100, 150, 255, 40),
                    );
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 150, 255)),
                        egui::StrokeKind::Inside,
                    );
                }
            });
    }

    /// Effects section: generators and effects to drag onto the timeline.
    pub(crate) fn show_effects_list(ui: &mut egui::Ui) {
        effects_section_header(ui, &t!("effects.generators"));
        egui::ScrollArea::vertical()
            .id_salt("effects_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for generator in timeline_ui::Generator::ALL {
                    effect_item(ui, generator);
                }
                ui.add_space(8.0);
                effects_section_header(ui, &t!("effects.filters"));
                for filter in timeline_ui::ALL_FILTER_KINDS {
                    filter_item(ui, filter);
                }
                ui.add_space(8.0);
                effects_section_header(ui, &t!("effects.transitions"));
                for transition in timeline_ui::ALL_TRANSITION_KINDS {
                    transition_item(ui, transition);
                }
            });
    }
}

#[cfg(test)]
#[path = "tests/media_pool_ui.rs"]
mod tests;
