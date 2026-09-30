//! Settings of the multiband compressor: on top its frequency response,
//! live, with the crossovers and the band gains to drag; below one strip
//! per band.

use vv_audio::dynamics::{BandActivity, CrossoverResponse};
use vv_audio::mixer::PROJECT_SAMPLE_RATE;
use vv_core::{AudioEffectKind, CompressorBand, MultibandCompressor};

use crate::mixer_panel::{knob, paint_fader_handle};
use crate::properties_panel::drag_field;

/// As wide as the three band strips below it, frames and gaps included.
const GRAPH_SIZE: egui::Vec2 = egui::vec2(3.0 * (STRIP_WIDTH + 18.0) + 16.0, 230.0);
const FREQ_MIN: f32 = 20.0;
const FREQ_MAX: f32 = 20_000.0;
const DB_TOP: f32 = 24.0;
const DB_BOTTOM: f32 = -30.0;
const CURVE_POINTS: usize = 240;
const BAND_COLORS: [egui::Color32; 3] = [
    egui::Color32::from_rgb(90, 160, 255),
    egui::Color32::from_rgb(110, 205, 120),
    egui::Color32::from_rgb(235, 190, 70),
];
const FADER_COLUMN_WIDTH: f32 = 76.0;
const STRIP_WIDTH: f32 = FADER_COLUMN_WIDTH + 2.0 * KNOB_CELL_WIDTH + 16.0;
const FADER_HEIGHT: f32 = 150.0;
const KNOB_RADIUS: f32 = 15.0;
const KNOB_CELL_WIDTH: f32 = 60.0;
/// The crossovers never get closer than this ratio.
const MIN_CROSSOVER_RATIO: f32 = 1.2;

/// Position of `freq` on the logarithmic axis, 0 at 20 Hz, 1 at 20 kHz.
pub(crate) fn freq_fraction(freq: f32) -> f32 {
    (freq / FREQ_MIN).ln() / (FREQ_MAX / FREQ_MIN).ln()
}

pub(crate) fn fraction_freq(t: f32) -> f32 {
    FREQ_MIN * (FREQ_MAX / FREQ_MIN).powf(t)
}

/// Knob position of `value` in `(min, max)`, logarithmic when `log`.
pub(crate) fn knob_fraction(value: f32, (min, max): (f32, f32), log: bool) -> f32 {
    let t = if log {
        (value / min).ln() / (max / min).ln()
    } else {
        (value - min) / (max - min)
    };
    t.clamp(0.0, 1.0)
}

pub(crate) fn knob_value(t: f32, (min, max): (f32, f32), log: bool) -> f32 {
    if log {
        min * (max / min).powf(t)
    } else {
        min + (max - min) * t
    }
}

fn band_edges(params: &MultibandCompressor) -> [f32; 4] {
    let [low, high] = params.crossovers_hz;
    [FREQ_MIN, low, high, FREQ_MAX]
}

fn format_freq(freq: f32) -> String {
    if freq >= 1000.0 {
        let k = freq / 1000.0;
        if k.fract() < 0.05 {
            format!("{k:.0}k")
        } else {
            format!("{k:.1}k")
        }
    } else {
        format!("{freq:.0}")
    }
}

/// The edited parameters, if changed. `activity`: the band levels and gain
/// reductions playing now.
pub(crate) fn show(
    ui: &mut egui::Ui,
    params: &MultibandCompressor,
    activity: &BandActivity,
) -> Option<AudioEffectKind> {
    let mut params = params.clone();
    let mut changed = graph(ui, &mut params, activity);
    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        for band in 0..3 {
            changed |= band_strip(ui, band, &mut params, activity);
        }
    });
    changed.then_some(AudioEffectKind::MultibandCompressor(params))
}

fn graph(ui: &mut egui::Ui, params: &mut MultibandCompressor, activity: &BandActivity) -> bool {
    let (rect, _) = ui.allocate_exact_size(GRAPH_SIZE, egui::Sense::hover());
    let x_of = |freq: f32| rect.left() + rect.width() * freq_fraction(freq);
    let y_of = |db: f32| {
        let t = (DB_TOP - db.clamp(DB_BOTTOM, DB_TOP)) / (DB_TOP - DB_BOTTOM);
        rect.top() + rect.height() * t
    };
    let freq_at = |x: f32| fraction_freq(((x - rect.left()) / rect.width()).clamp(0.0, 1.0));
    let db_at = |y: f32| DB_TOP - (y - rect.top()) / rect.height() * (DB_TOP - DB_BOTTOM);

    let mut changed = false;
    let pointer = ui.ctx().pointer_interact_pos();
    let mut active_crossover = None;
    for k in 0..2 {
        let x = x_of(params.crossovers_hz[k]);
        let hit = egui::Rect::from_x_y_ranges(x - 6.0..=x + 6.0, rect.y_range());
        let response = ui
            .interact(hit, ui.id().with(("crossover", k)), egui::Sense::drag())
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if response.dragged()
            && let Some(pointer) = pointer
        {
            let [low, high] = params.crossovers_hz;
            let (min, max) = if k == 0 {
                (FREQ_MIN, high / MIN_CROSSOVER_RATIO)
            } else {
                (low * MIN_CROSSOVER_RATIO, FREQ_MAX)
            };
            params.crossovers_hz[k] = freq_at(pointer.x).clamp(min, max).round();
            changed = true;
        }
        if response.hovered() || response.dragged() {
            active_crossover = Some(k);
        }
    }
    let mut active_band = None;
    for band in 0..3 {
        let edges = band_edges(params);
        let centre = (edges[band] * edges[band + 1]).sqrt();
        let spot = egui::pos2(x_of(centre), y_of(params.bands[band].makeup_db));
        let response = ui
            .interact(
                egui::Rect::from_center_size(spot, egui::Vec2::splat(18.0)),
                ui.id().with(("band_gain", band)),
                egui::Sense::drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeVertical);
        if response.dragged()
            && let Some(pointer) = pointer
        {
            let (min, max) = CompressorBand::MAKEUP_RANGE_DB;
            let db = (db_at(pointer.y).clamp(min, max) * 10.0).round() / 10.0;
            params.bands[band].makeup_db = db;
            changed = true;
        }
        if response.hovered() || response.dragged() {
            active_band = Some(band);
        }
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, egui::Color32::from_gray(14));
    let edges = band_edges(params);
    for band in 0..3 {
        let area =
            egui::Rect::from_x_y_ranges(x_of(edges[band])..=x_of(edges[band + 1]), rect.y_range());
        let alpha = if active_band == Some(band) {
            0.12
        } else {
            0.05
        };
        painter.rect_filled(area, 0.0, BAND_COLORS[band].gamma_multiply(alpha));
    }

    let grid = egui::Color32::from_gray(40);
    let label = egui::Color32::from_gray(110);
    let font = egui::FontId::proportional(9.0);
    for db in (DB_BOTTOM as i32..=DB_TOP as i32).step_by(6) {
        let y = y_of(db as f32);
        let stroke = if db == 0 {
            egui::Stroke::new(1.0, egui::Color32::from_gray(70))
        } else {
            egui::Stroke::new(1.0, grid)
        };
        painter.hline(rect.x_range(), y, stroke);
    }
    for freq in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10_000.0] {
        let x = x_of(freq);
        painter.vline(x, rect.y_range(), egui::Stroke::new(1.0, grid));
        painter.text(
            egui::pos2(x, rect.bottom() - 3.0),
            egui::Align2::CENTER_BOTTOM,
            format_freq(freq),
            font.clone(),
            label,
        );
    }

    // The static curve is the makeup alone; the live one takes the current
    // reduction off each band, and the space between them is what is being
    // squashed now.
    let response = CrossoverResponse::new(params, PROJECT_SAMPLE_RATE);
    let makeup = params.bands.map(|b| b.makeup_db);
    let live = [0, 1, 2].map(|b| makeup[b] - activity.reduction_db[b]);
    let points: Vec<(f32, f32, f32)> = (0..=CURVE_POINTS)
        .map(|i| {
            let freq = fraction_freq(i as f32 / CURVE_POINTS as f32);
            (
                x_of(freq),
                y_of(response.gain_db(makeup, freq)),
                y_of(response.gain_db(live, freq)),
            )
        })
        .collect();
    let squash = crate::theme::ACCENT.gamma_multiply(0.3);
    for pair in points.windows(2) {
        let [(x0, s0, l0), (x1, s1, l1)] = [pair[0], pair[1]];
        if l0 - s0 > 0.5 || l1 - s1 > 0.5 {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(x0, s0),
                    egui::pos2(x1, s1),
                    egui::pos2(x1, l1),
                    egui::pos2(x0, l0),
                ],
                squash,
                egui::Stroke::NONE,
            ));
        }
    }
    painter.add(egui::Shape::line(
        points.iter().map(|&(x, s, _)| egui::pos2(x, s)).collect(),
        egui::Stroke::new(1.5, egui::Color32::from_gray(150)),
    ));
    painter.add(egui::Shape::line(
        points.iter().map(|&(x, _, l)| egui::pos2(x, l)).collect(),
        egui::Stroke::new(2.0, crate::theme::ACCENT),
    ));

    for db in (DB_BOTTOM as i32 + 6..DB_TOP as i32).step_by(6) {
        let text = if db > 0 {
            format!("+{db}")
        } else {
            db.to_string()
        };
        painter.text(
            egui::pos2(rect.right() - 4.0, y_of(db as f32) - 1.0),
            egui::Align2::RIGHT_BOTTOM,
            text,
            font.clone(),
            label,
        );
    }
    for k in 0..2 {
        let x = x_of(params.crossovers_hz[k]);
        let lit = active_crossover == Some(k);
        let color = if lit {
            egui::Color32::from_gray(230)
        } else {
            egui::Color32::from_gray(130)
        };
        painter.add(egui::Shape::dashed_line(
            &[
                egui::pos2(x, rect.top() + 8.0),
                egui::pos2(x, rect.bottom() - 14.0),
            ],
            egui::Stroke::new(1.0, color),
            4.0,
            3.0,
        ));
        let tip = egui::pos2(x, rect.top() + 10.0);
        painter.add(egui::Shape::convex_polygon(
            vec![
                tip,
                tip + egui::vec2(6.0, -9.0),
                tip + egui::vec2(-6.0, -9.0),
            ],
            color,
            egui::Stroke::NONE,
        ));
        if lit {
            painter.text(
                egui::pos2(x + 8.0, rect.top() + 2.0),
                egui::Align2::LEFT_TOP,
                format!("{} Hz", params.crossovers_hz[k].round()),
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
        }
    }
    for band in 0..3 {
        let centre = (edges[band] * edges[band + 1]).sqrt();
        let spot = egui::pos2(x_of(centre), y_of(params.bands[band].makeup_db));
        let lit = active_band == Some(band);
        painter.circle(
            spot,
            if lit { 7.0 } else { 5.5 },
            BAND_COLORS[band],
            egui::Stroke::new(1.5, egui::Color32::from_gray(if lit { 255 } else { 20 })),
        );
        if lit {
            painter.text(
                spot + egui::vec2(10.0, -10.0),
                egui::Align2::LEFT_BOTTOM,
                format!("{:+.1} dB", params.bands[band].makeup_db),
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
        }
    }
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(60)),
        egui::StrokeKind::Inside,
    );
    changed
}

fn band_strip(
    ui: &mut egui::Ui,
    band: usize,
    params: &mut MultibandCompressor,
    activity: &BandActivity,
) -> bool {
    let edges = band_edges(params);
    let range = format!(
        "{}–{} Hz",
        format_freq(edges[band]),
        format_freq(edges[band + 1])
    );
    let settings = &mut params.bands[band];
    let mut changed = false;
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(STRIP_WIDTH);
            ui.vertical(|ui| {
                ui.push_id(band, |ui| {
                    ui.horizontal(|ui| {
                        let (dot, _) =
                            ui.allocate_exact_size(egui::Vec2::splat(10.0), egui::Sense::hover());
                        ui.painter()
                            .circle_filled(dot.center(), 4.5, BAND_COLORS[band]);
                        ui.strong(
                            [
                                t!("mixer.band_low"),
                                t!("mixer.band_mid"),
                                t!("mixer.band_high"),
                            ][band]
                                .clone(),
                        );
                        ui.weak(range);
                    });
                    ui.add_space(4.0);
                    ui.horizontal_top(|ui| {
                        let column = egui::vec2(FADER_COLUMN_WIDTH, 0.0);
                        let layout = egui::Layout::top_down(egui::Align::Center);
                        ui.allocate_ui_with_layout(column, layout, |ui| {
                            ui.set_width(FADER_COLUMN_WIDTH);
                            ui.small(t!("mixer.threshold"));
                            let level = activity.level[band];
                            let reduction = activity.reduction_db[band];
                            if let Some(db) =
                                threshold_fader(ui, settings.threshold_db, level, reduction)
                            {
                                settings.threshold_db = db;
                                changed = true;
                            }
                            changed |= value_field(
                                ui,
                                &mut settings.threshold_db,
                                CompressorBand::THRESHOLD_RANGE_DB,
                                0.2,
                                1,
                                " dB",
                            );
                        });
                        let d = CompressorBand::DEFAULT;
                        ui.vertical(|ui| {
                            ui.horizontal_top(|ui| {
                                changed |= param_knob(
                                    ui,
                                    &t!("mixer.ratio"),
                                    &mut settings.ratio,
                                    KnobScale::new(CompressorBand::RATIO_RANGE, true, d.ratio),
                                    (0.05, 1, ":1"),
                                );
                                changed |= param_knob(
                                    ui,
                                    &t!("mixer.makeup_short"),
                                    &mut settings.makeup_db,
                                    KnobScale::new(
                                        CompressorBand::MAKEUP_RANGE_DB,
                                        false,
                                        d.makeup_db,
                                    ),
                                    (0.1, 1, " dB"),
                                );
                            });
                            ui.add_space(6.0);
                            ui.horizontal_top(|ui| {
                                changed |= param_knob(
                                    ui,
                                    &t!("mixer.attack"),
                                    &mut settings.attack_ms,
                                    KnobScale::new(
                                        CompressorBand::ATTACK_RANGE_MS,
                                        true,
                                        d.attack_ms,
                                    ),
                                    (0.2, 1, " ms"),
                                );
                                changed |= param_knob(
                                    ui,
                                    &t!("mixer.release"),
                                    &mut settings.release_ms,
                                    KnobScale::new(
                                        CompressorBand::RELEASE_RANGE_MS,
                                        true,
                                        d.release_ms,
                                    ),
                                    (2.0, 0, " ms"),
                                );
                            });
                        });
                    });
                });
            });
        });
    changed
}

struct KnobScale {
    range: (f32, f32),
    log: bool,
    default: f32,
}

impl KnobScale {
    fn new(range: (f32, f32), log: bool, default: f32) -> Self {
        Self {
            range,
            log,
            default,
        }
    }
}

/// Label, knob and the numeric field under it; `field` is speed, decimals
/// and suffix of the field.
fn param_knob(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    scale: KnobScale,
    (speed, decimals, suffix): (f64, usize, &str),
) -> bool {
    let cell = egui::vec2(KNOB_CELL_WIDTH, 0.0);
    ui.allocate_ui_with_layout(cell, egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.set_width(KNOB_CELL_WIDTH);
        ui.small(label);
        let t = knob_fraction(*value, scale.range, scale.log);
        let default = knob_fraction(scale.default, scale.range, scale.log);
        let (_, turned) = knob(ui, t, default, KNOB_RADIUS);
        let mut changed = false;
        if let Some(t) = turned {
            *value = knob_value(t, scale.range, scale.log);
            changed = true;
        }
        changed |= value_field(ui, value, scale.range, speed, decimals, suffix);
        changed
    })
    .inner
}

/// A numeric field for an `f32` within `(min, max)`.
pub(crate) fn value_field(
    ui: &mut egui::Ui,
    value: &mut f32,
    (min, max): (f32, f32),
    speed: f64,
    decimals: usize,
    suffix: &str,
) -> bool {
    let mut field = *value as f64;
    let changed = drag_field(
        ui,
        &mut field,
        speed,
        min as f64..=max as f64,
        decimals,
        suffix,
    );
    if changed {
        *value = field as f32;
    }
    changed
}

/// Threshold fader over the band's level meter: the part of the level past
/// the threshold lights up, and the reduction it causes grows down beside
/// it. Moved by the drag like the mixer faders, double click for the default.
fn threshold_fader(
    ui: &mut egui::Ui,
    threshold_db: f32,
    level: f32,
    reduction_db: f32,
) -> Option<f32> {
    const SCALE_WIDTH: f32 = 22.0;
    const GROOVE_WIDTH: f32 = 26.0;
    const BAR_WIDTH: f32 = 7.0;
    const HANDLE: egui::Vec2 = egui::vec2(24.0, 14.0);
    const REDUCTION_RANGE_DB: f32 = 24.0;
    let (min, max) = CompressorBand::THRESHOLD_RANGE_DB;
    let size = egui::vec2(
        SCALE_WIDTH + GROOVE_WIDTH + 2.0 * BAR_WIDTH + 4.0,
        FADER_HEIGHT,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let response = response.on_hover_cursor(egui::CursorIcon::ResizeVertical);
    let top = rect.top() + HANDLE.y / 2.0;
    let bottom = rect.bottom() - HANDLE.y / 2.0;
    let y_of = |db: f32| bottom - (db.clamp(min, max) - min) / (max - min) * (bottom - top);

    let mut changed = None;
    if response.double_clicked() {
        changed = Some(CompressorBand::DEFAULT.threshold_db);
    } else if response.dragged() {
        let dy = response.drag_delta().y;
        if dy != 0.0 {
            let fine = if ui.input(|i| i.modifiers.shift) {
                0.1
            } else {
                1.0
            };
            let db = threshold_db - dy / (bottom - top) * (max - min) * fine;
            changed = Some(db.clamp(min, max));
        }
    }

    let painter = ui.painter();
    let weak = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(9.0);
    let groove_x = rect.left() + SCALE_WIDTH + GROOVE_WIDTH / 2.0;
    for db in [0.0, -12.0, -24.0, -36.0, -48.0, -60.0] {
        let y = y_of(db);
        painter.text(
            egui::pos2(rect.left() + SCALE_WIDTH - 3.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{db}"),
            font.clone(),
            weak,
        );
        painter.line_segment(
            [egui::pos2(groove_x - 9.0, y), egui::pos2(groove_x - 5.0, y)],
            egui::Stroke::new(1.0, weak),
        );
    }
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(groove_x - 2.0, top),
            egui::pos2(groove_x + 2.0, bottom),
        ),
        2.0,
        egui::Color32::from_gray(15),
    );

    let level_x = rect.right() - 2.0 * BAR_WIDTH - 2.0;
    let level_bar = egui::Rect::from_min_max(
        egui::pos2(level_x, top),
        egui::pos2(level_x + BAR_WIDTH, bottom),
    );
    painter.rect_filled(level_bar, 1.0, egui::Color32::from_gray(10));
    let level_db = 20.0 * level.max(1e-9).log10();
    if level_db > min {
        let threshold_y = y_of(threshold_db);
        let level_y = y_of(level_db);
        let under = egui::Rect::from_min_max(
            egui::pos2(level_bar.left(), level_y.max(threshold_y)),
            level_bar.right_bottom(),
        );
        painter.rect_filled(under, 0.0, egui::Color32::from_rgb(60, 200, 90));
        if level_y < threshold_y {
            let over = egui::Rect::from_x_y_ranges(level_bar.x_range(), level_y..=threshold_y);
            painter.rect_filled(over, 0.0, crate::theme::ACCENT);
        }
    }
    let reduction_bar = level_bar.translate(egui::vec2(BAR_WIDTH + 2.0, 0.0));
    painter.rect_filled(reduction_bar, 1.0, egui::Color32::from_gray(10));
    let depth = reduction_bar.height() * (reduction_db / REDUCTION_RANGE_DB).clamp(0.0, 1.0);
    if depth > 0.5 {
        let bar = egui::Rect::from_min_max(
            reduction_bar.left_top(),
            egui::pos2(reduction_bar.right(), reduction_bar.top() + depth),
        );
        painter.rect_filled(bar, 0.0, crate::theme::ERROR);
    }

    let handle = egui::Rect::from_center_size(egui::pos2(groove_x, y_of(threshold_db)), HANDLE);
    paint_fader_handle(painter, handle, response.hovered() || response.dragged());
    changed
}

#[cfg(test)]
#[path = "tests/compressor_panel.rs"]
mod tests;
