//! Pannelli del media pool e degli effetti.

use super::*;

/// Anello di avanzamento; `None` = in coda (solo l'anello di sfondo).
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

/// Etichetta che segue il cursore mentre si trascina un media.
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

/// Voce del pannello Effects: miniatura a sinistra e nome, trascinabile
/// sulla timeline.
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

/// Larghezza della colonna "Durata": la stessa nell'intestazione e nelle
/// righe, così restano allineate.
pub(crate) const DURATION_COL_W: f32 = 64.0;

/// Altezza della barra di intestazione del media pool.
pub(crate) const HEADER_HEIGHT: f32 = 22.0;

/// Intestazione a colonne del media pool: ogni cella è cliccabile per
/// intero (non solo la scritta), come nella lista di un file manager.
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
            // Triangolino disegnato a mano invece di un carattere: quelli
            // dei font di sistema sono alti e appuntiti, questo è schiacciato.
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

impl VibeVideoApp {
    /// Contenuto della sezione Media pool nella colonna di sinistra.
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
        // `auto_shrink` spento: il pannello deve riempire la larghezza assegnata,
        // altrimenti il suo resize torna indietro.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let items: Vec<(MediaId, String, vv_core::MediaMeta, u64)> = self
                    .project
                    .media_pool
                    .iter()
                    .map(|(id, item)| {
                        (id, file_label(&item.path), item.meta.clone(), item.content_hash)
                    })
                    .collect();
                let mut items = items;
                media_pool::sort_items(
                    &mut items,
                    self.media_pool_state.sort,
                    |(_, label, ..)| label.as_str(),
                    |(_, _, meta, _)| {
                        meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9)
                    },
                );
                let order: Vec<MediaId> = items.iter().map(|(id, ..)| *id).collect();
                let drags: Vec<timeline_ui::MediaDrag> = items
                    .iter()
                    .map(|(id, _, meta, _)| timeline_ui::MediaDrag::whole(*id, meta))
                    .collect();
                // Interagito prima degli elementi: in egui vince l'ultimo, così un click
                // su un elemento non fa partire il rettangolo di selezione.
                let bg = ui.interact(
                    ui.available_rect_before_wrap(),
                    ui.id().with("media_pool_bg"),
                    egui::Sense::click_and_drag(),
                );
                let mut item_rects: Vec<(MediaId, egui::Rect)> = Vec::new();
                for (id, label, meta, content_hash) in items {
                    let proxy_state = self
                        .proxy_worker
                        .as_ref()
                        .and_then(|w| w.state(content_hash));
                    let thumbnail = self.thumbnails.get(&content_hash).cloned().flatten();
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
                                        if !meta.has_video {
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
                                    ui.label(&label);
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
                                        // Un'immagine non ha durata reale.
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
                    // Doppio click: anteprima. Trascinamento: sulla timeline aggiunge il media.
                    let interact_id = ui.id().with("media_pool_item").with(id);
                    let resp = ui
                        .interact(group_resp.rect, interact_id, egui::Sense::click_and_drag())
                        .on_hover_text(
                            t!("pool.item_hint"),
                        );
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
                        self.media_pool_state.click(id, modifiers, &order);
                    }
                    // Tasto destro fuori dalla selezione la sostituisce, come il drag.
                    if resp.secondary_clicked() && !self.media_pool_state.selected.contains(&id) {
                        self.media_pool_state.click(id, egui::Modifiers::NONE, &order);
                    }
                    resp.context_menu(|ui| {
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
                    // Trascinare un elemento fuori dalla selezione la
                    // sostituisce con lui (come in timeline, vedi
                    // `timeline_ui::drag_group_for`).
                    if resp.drag_started() && !self.media_pool_state.selected.contains(&id) {
                        self.media_pool_state.click(id, egui::Modifiers::NONE, &order);
                    }
                    // Trascinare un elemento della selezione trascina
                    // l'intera selezione, nell'ordine del pannello: la
                    // timeline le accoda una dopo l'altra.
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
                        *preview_action = Some(id);
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
                        self.media_pool_state.set_marquee_selection(hits);
                    }
                } else if bg.clicked() {
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

    /// Sezione Effects: effetti da trascinare sulla timeline.
    pub(crate) fn show_effects_list(ui: &mut egui::Ui) {
        effects_section_header(ui, &t!("effects.generators"));
        egui::ScrollArea::vertical()
            .id_salt("effects_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for generator in timeline_ui::Generator::ALL {
                    effect_item(ui, generator);
                }
            });
    }
}
