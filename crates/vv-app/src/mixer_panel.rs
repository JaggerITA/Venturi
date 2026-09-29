//! Audio mixer: floating window with one console strip per audio track of
//! the timeline, plus the master.

use std::collections::HashMap;

use vv_audio::mixer::{MixMeters, StereoPeak, db_to_linear};
use vv_core::{
    ChannelStrip, MixerChannel, MixerParam, Project, SetMixerParam, SetTrackFlag, TimelineId,
    TrackFlag, TrackKind,
};

use crate::properties_panel::{BoxedCommand, drag_field};

const STRIP_WIDTH: f32 = 84.0;
const FADER_HEIGHT: f32 = 230.0;
const SCALE_WIDTH: f32 = 24.0;
const GROOVE_AREA_WIDTH: f32 = 34.0;
const METER_WIDTH: f32 = 12.0;
const HANDLE_SIZE: egui::Vec2 = egui::vec2(30.0, 18.0);
const KNOB_RADIUS: f32 = 14.0;
/// Of the knob travel, each side of the centre.
const KNOB_SWEEP: f32 = 135.0_f32 * std::f32::consts::PI / 180.0;
const METER_DECAY: f32 = 0.85;
const SCALE_MARKS: [f32; 9] = [12.0, 6.0, 0.0, -6.0, -12.0, -20.0, -30.0, -40.0, -60.0];
const MUTE_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 150, 190);

/// Fader position (0 bottom, 1 top) of the dB values in between: more room
/// around unity, where the mixing happens, than at the extremes.
const FADER_CURVE: [(f32, f32); 11] = [
    (vv_core::GAIN_DB_MIN, 0.0),
    (-60.0, 0.07),
    (-40.0, 0.16),
    (-30.0, 0.24),
    (-20.0, 0.34),
    (-12.0, 0.46),
    (-6.0, 0.58),
    (0.0, 0.7),
    (6.0, 0.8),
    (12.0, 0.88),
    (vv_core::GAIN_DB_MAX, 1.0),
];

pub(crate) fn fader_position(db: f32) -> f32 {
    interpolate(&FADER_CURVE, db, |(db, _)| db, |(_, pos)| pos)
}

pub(crate) fn fader_db(position: f32) -> f32 {
    interpolate(&FADER_CURVE, position, |(_, pos)| pos, |(db, _)| db)
}

fn interpolate(
    curve: &[(f32, f32)],
    x: f32,
    key: impl Fn((f32, f32)) -> f32,
    value: impl Fn((f32, f32)) -> f32,
) -> f32 {
    let (first, last) = (curve[0], curve[curve.len() - 1]);
    let x = x.clamp(key(first), key(last));
    for pair in curve.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if x <= key(b) {
            let t = (x - key(a)) / (key(b) - key(a));
            return value(a) + (value(b) - value(a)) * t;
        }
    }
    value(last)
}

pub(crate) fn format_pan(pan: f32) -> String {
    let percent = (pan * 100.0).round() as i32;
    match percent {
        0 => "C".to_owned(),
        p if p < 0 => format!("L{}", -p),
        p => format!("R{p}"),
    }
}

/// Meter levels with their fall, by strip.
#[derive(Default)]
pub(crate) struct MixerPanelState {
    levels: HashMap<MixerChannel, [f32; 2]>,
}

impl MixerPanelState {
    fn level(&mut self, channel: MixerChannel, peak: Option<&StereoPeak>) -> [f32; 2] {
        let (l, r) = peak.map_or((0.0, 0.0), StereoPeak::take);
        let level = self.levels.entry(channel).or_default();
        *level = [l.max(level[0] * METER_DECAY), r.max(level[1] * METER_DECAY)];
        *level
    }
}

pub(crate) fn show_mixer(
    ctx: &egui::Context,
    open: &mut bool,
    state: &mut MixerPanelState,
    project: &Project,
    timeline_id: Option<TimelineId>,
    meters: Option<&MixMeters>,
) -> Vec<BoxedCommand> {
    let mut commands: Vec<BoxedCommand> = Vec::new();
    egui::Window::new(t!("mixer.title"))
        .id(egui::Id::new("audio_mixer"))
        .open(open)
        .default_width(420.0)
        .show(ctx, |ui| {
            let Some((timeline_id, timeline)) =
                timeline_id.and_then(|id| project.timelines.get(id).map(|t| (id, t)))
            else {
                ui.label(t!("mixer.no_timeline"));
                return;
            };
            let mut falling = false;
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    for (index, track) in timeline.tracks_of_kind(TrackKind::Audio) {
                        let channel = MixerChannel::Track(index);
                        let level = state.level(channel, meters.and_then(|m| m.track(index)));
                        falling |= level.iter().any(|l| *l > 0.001);
                        let flags = Some((track.solo, track.muted));
                        let edit = strip(
                            ui,
                            channel,
                            &timeline.track_label(index),
                            &track.mix,
                            flags,
                            level,
                        );
                        commands.extend(edit.into_commands(timeline_id, channel));
                    }
                    let level = state.level(MixerChannel::Master, meters.map(MixMeters::master));
                    falling |= level.iter().any(|l| *l > 0.001);
                    let edit = strip(
                        ui,
                        MixerChannel::Master,
                        &t!("mixer.master"),
                        &timeline.master,
                        None,
                        level,
                    );
                    commands.extend(edit.into_commands(timeline_id, MixerChannel::Master));
                });
            });
            if falling {
                ui.ctx().request_repaint();
            }
        });
    commands
}

#[derive(Default)]
struct StripEdit {
    gain_db: Option<f32>,
    pan: Option<f32>,
    toggle_solo: Option<bool>,
    toggle_mute: Option<bool>,
}

impl StripEdit {
    fn into_commands(self, timeline: TimelineId, channel: MixerChannel) -> Vec<BoxedCommand> {
        let mut commands: Vec<BoxedCommand> = Vec::new();
        let params = [
            (MixerParam::GainDb, self.gain_db),
            (MixerParam::Pan, self.pan),
        ];
        for (param, value) in params {
            if let Some(value) = value {
                commands.push(Box::new(SetMixerParam::new(
                    timeline, channel, param, value,
                )));
            }
        }
        if let MixerChannel::Track(index) = channel {
            let flags = [
                (TrackFlag::Solo, self.toggle_solo),
                (TrackFlag::Muted, self.toggle_mute),
            ];
            for (flag, value) in flags {
                if let Some(value) = value {
                    commands.push(Box::new(SetTrackFlag::new(timeline, index, flag, value)));
                }
            }
        }
        commands
    }
}

/// `flags` are solo and mute, only for the tracks.
fn strip(
    ui: &mut egui::Ui,
    channel: MixerChannel,
    label: &str,
    mix: &ChannelStrip,
    flags: Option<(bool, bool)>,
    level: [f32; 2],
) -> StripEdit {
    let mut edit = StripEdit::default();
    let frame = egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::symmetric(4, 6));
    frame.show(ui, |ui| {
        ui.set_width(STRIP_WIDTH);
        ui.vertical_centered(|ui| {
            ui.push_id(channel, |ui| {
                edit.pan = pan_knob(ui, mix.pan);
                ui.small(format_pan(mix.pan));
                ui.add_space(4.0);
                solo_mute(ui, flags, &mut edit);
                ui.add_space(6.0);
                edit.gain_db = fader(ui, mix.gain_db, level);
                ui.add_space(4.0);
                let mut field = mix.gain_db as f64;
                let range = MixerParam::GainDb.range();
                if drag_field(
                    ui,
                    &mut field,
                    0.2,
                    *range.start() as f64..=*range.end() as f64,
                    1,
                    " dB",
                ) {
                    edit.gain_db = Some(field as f32);
                }
                ui.add_space(4.0);
                ui.strong(label);
            });
        });
    });
    edit
}

fn solo_mute(ui: &mut egui::Ui, flags: Option<(bool, bool)>, edit: &mut StripEdit) {
    let size = egui::vec2((STRIP_WIDTH - ui.spacing().item_spacing.x) / 2.0, 20.0);
    let Some((solo, muted)) = flags else {
        // Keeps the faders of all the strips on the same line.
        ui.allocate_space(egui::vec2(STRIP_WIDTH, size.y));
        return;
    };
    ui.horizontal(|ui| {
        let button = |ui: &mut egui::Ui, text: String, on: bool, color: egui::Color32| {
            let text = egui::RichText::new(text).small().strong();
            let text = if on {
                text.color(egui::Color32::BLACK)
            } else {
                text
            };
            let mut button = egui::Button::new(text).min_size(size);
            if on {
                button = button.fill(color);
            }
            ui.add(button).clicked()
        };
        if button(
            ui,
            t!("mixer.solo").into_owned(),
            solo,
            crate::theme::ACCENT,
        ) {
            edit.toggle_solo = Some(!solo);
        }
        if button(ui, t!("mixer.mute").into_owned(), muted, MUTE_COLOR) {
            edit.toggle_mute = Some(!muted);
        }
    });
}

/// Balance knob: drag up/right to turn it clockwise, Shift for precision,
/// double click to centre it.
fn pan_knob(ui: &mut egui::Ui, pan: f32) -> Option<f32> {
    let size = egui::Vec2::splat(KNOB_RADIUS * 2.0 + 8.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let response = response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    let mut changed = None;
    if response.double_clicked() {
        changed = Some(0.0);
    } else if response.dragged() {
        let delta = response.drag_delta();
        let fine = if ui.input(|i| i.modifiers.shift) {
            0.1
        } else {
            1.0
        };
        let moved = (delta.x - delta.y) / 100.0 * fine;
        if moved != 0.0 {
            changed = Some((pan + moved).clamp(-1.0, 1.0));
        }
    }

    let painter = ui.painter();
    let c = rect.center();
    let at = |angle: f32, radius: f32| c + radius * egui::vec2(angle.sin(), -angle.cos());
    let track: Vec<egui::Pos2> = (0..=24)
        .map(|i| {
            at(
                -KNOB_SWEEP + 2.0 * KNOB_SWEEP * i as f32 / 24.0,
                KNOB_RADIUS + 3.0,
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        track,
        egui::Stroke::new(1.0, egui::Color32::from_gray(80)),
    ));
    let visuals = ui.style().interact(&response);
    painter.circle(
        c,
        KNOB_RADIUS,
        egui::Color32::from_gray(if response.hovered() { 75 } else { 60 }),
        egui::Stroke::new(1.0, visuals.fg_stroke.color),
    );
    let angle = pan.clamp(-1.0, 1.0) * KNOB_SWEEP;
    let pointer = if pan.abs() < 0.005 {
        egui::Color32::from_gray(220)
    } else {
        crate::theme::ACCENT
    };
    painter.line_segment(
        [at(angle, 4.0), at(angle, KNOB_RADIUS - 2.0)],
        egui::Stroke::new(2.5, pointer),
    );
    let font = egui::FontId::proportional(9.0);
    let weak = ui.visuals().weak_text_color();
    painter.text(
        at(-KNOB_SWEEP, KNOB_RADIUS + 6.0),
        egui::Align2::RIGHT_TOP,
        "L",
        font.clone(),
        weak,
    );
    painter.text(
        at(KNOB_SWEEP, KNOB_RADIUS + 6.0),
        egui::Align2::LEFT_TOP,
        "R",
        font,
        weak,
    );
    changed
}

/// Console fader with its dB scale and the stereo meter of the strip. The
/// handle moves by the drag, not to the pointer: grabbing it never makes it
/// jump. Shift for precision, double click for unity.
fn fader(ui: &mut egui::Ui, gain_db: f32, level: [f32; 2]) -> Option<f32> {
    let size = egui::vec2(SCALE_WIDTH + GROOVE_AREA_WIDTH + METER_WIDTH, FADER_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let response = response.on_hover_cursor(egui::CursorIcon::ResizeVertical);
    let top = rect.top() + HANDLE_SIZE.y / 2.0;
    let bottom = rect.bottom() - HANDLE_SIZE.y / 2.0;
    let y_of = |position: f32| bottom - position * (bottom - top);
    let position = fader_position(gain_db);

    let mut changed = None;
    if response.double_clicked() {
        changed = Some(0.0);
    } else if response.dragged() {
        let dy = response.drag_delta().y;
        if dy != 0.0 {
            let fine = if ui.input(|i| i.modifiers.shift) {
                0.1
            } else {
                1.0
            };
            let moved = -dy / (bottom - top) * fine;
            changed = Some(fader_db((position + moved).clamp(0.0, 1.0)));
        }
    }

    let painter = ui.painter();
    let groove_x = rect.left() + SCALE_WIDTH + GROOVE_AREA_WIDTH / 2.0;
    let meter_rect = egui::Rect::from_min_max(
        egui::pos2(rect.right() - METER_WIDTH, top),
        egui::pos2(rect.right(), bottom),
    );
    let weak = ui.visuals().weak_text_color();
    let font = egui::FontId::proportional(9.0);
    for db in SCALE_MARKS {
        let y = y_of(fader_position(db));
        let text = if db > 0.0 {
            format!("+{db}")
        } else {
            format!("{db}")
        };
        painter.text(
            egui::pos2(rect.left() + SCALE_WIDTH - 3.0, y),
            egui::Align2::RIGHT_CENTER,
            text,
            font.clone(),
            weak,
        );
        let tick = if db == 0.0 { 8.0 } else { 5.0 };
        painter.line_segment(
            [
                egui::pos2(groove_x - tick - 4.0, y),
                egui::pos2(groove_x - 4.0, y),
            ],
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
    paint_meter(painter, meter_rect, level, &y_of);

    let handle = egui::Rect::from_center_size(egui::pos2(groove_x, y_of(position)), HANDLE_SIZE);
    let lit = response.hovered() || response.dragged();
    let base = if lit { 205 } else { 180 };
    painter.rect_filled(
        handle.translate(egui::vec2(0.0, 2.0)),
        3.0,
        egui::Color32::from_black_alpha(120),
    );
    painter.rect_filled(handle, 3.0, egui::Color32::from_gray(base));
    for (i, shade) in [(0.25, 25), (0.75, -25)] {
        let y = handle.top() + handle.height() * i;
        let color = egui::Color32::from_gray((base as i32 + shade).clamp(0, 255) as u8);
        painter.line_segment(
            [
                egui::pos2(handle.left() + 3.0, y),
                egui::pos2(handle.right() - 3.0, y),
            ],
            egui::Stroke::new(handle.height() / 2.0 - 3.0, color),
        );
    }
    painter.line_segment(
        [
            egui::pos2(handle.left() + 2.0, handle.center().y),
            egui::pos2(handle.right() - 2.0, handle.center().y),
        ],
        egui::Stroke::new(2.0, egui::Color32::from_gray(20)),
    );
    painter.rect_stroke(
        handle,
        3.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(50)),
        egui::StrokeKind::Inside,
    );
    changed
}

/// Two bars on the fader scale: green, yellow from -6 dBFS, red past 0.
fn paint_meter(
    painter: &egui::Painter,
    rect: egui::Rect,
    level: [f32; 2],
    y_of: &impl Fn(f32) -> f32,
) {
    const GREEN: egui::Color32 = egui::Color32::from_rgb(60, 200, 90);
    const YELLOW: egui::Color32 = egui::Color32::from_rgb(230, 200, 50);
    const RED: egui::Color32 = egui::Color32::from_rgb(220, 50, 50);
    let gap = 2.0;
    let bar_width = (rect.width() - gap) / 2.0;
    let zones = [
        (vv_core::GAIN_DB_MIN, -6.0, GREEN),
        (-6.0, 0.0, YELLOW),
        (0.0, vv_core::GAIN_DB_MAX, RED),
    ];
    for (i, linear) in level.into_iter().enumerate() {
        let x = rect.left() + i as f32 * (bar_width + gap);
        let bar = egui::Rect::from_min_max(
            egui::pos2(x, rect.top()),
            egui::pos2(x + bar_width, rect.bottom()),
        );
        painter.rect_filled(bar, 1.0, egui::Color32::from_gray(10));
        if linear <= db_to_linear(vv_core::GAIN_DB_MIN) {
            continue;
        }
        let db = 20.0 * linear.log10();
        for (from, to, color) in zones {
            if db <= from {
                break;
            }
            let (y_from, y_to) = (y_of(fader_position(from)), y_of(fader_position(db.min(to))));
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(bar.left(), y_to),
                    egui::pos2(bar.right(), y_from),
                ),
                0.0,
                color,
            );
        }
    }
}

#[cfg(test)]
#[path = "tests/mixer_panel.rs"]
mod tests;
