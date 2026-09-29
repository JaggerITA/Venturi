//! Audio mixer: floating window with one console strip per audio track of
//! the timeline, plus the master.

use std::collections::HashMap;

use vv_audio::mixer::{MixMeters, StereoPeak, db_to_linear};
use vv_core::{
    AddAudioEffect, AudioEffect, AudioEffectKind, ChannelStrip, CommandLabel, CompressorBand,
    MixerChannel, MixerParam, MoveAudioEffect, MultibandCompressor, Project, RemoveAudioEffect,
    SetAudioEffect, SetMixerParam, SetTrackFlag, Timeline, TimelineId, TrackFlag, TrackKind,
};

use crate::properties_panel::{BoxedCommand, drag_field, slider_field};

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
const EFFECT_ROW_HEIGHT: f32 = 20.0;
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
    /// The effect whose settings window is open.
    open_effect: Option<(TimelineId, MixerChannel, usize)>,
}

impl MixerPanelState {
    /// The settings window opened by `edit`, or following the removal of an
    /// effect above it in the same chain.
    fn follow_edit(&mut self, edit: &StripEdit, timeline: TimelineId, channel: MixerChannel) {
        if let Some(index) = edit.open_settings {
            self.open_effect = Some((timeline, channel, index));
        }
        let Some((tl, open_channel, open_index)) = &mut self.open_effect else {
            return;
        };
        if (*tl, *open_channel) != (timeline, channel) {
            return;
        }
        if let Some((from, to)) = edit.move_effect {
            *open_index = index_after_move(*open_index, from, to);
        }
        if let Some(removed) = edit.remove_effect {
            match (*open_index).cmp(&removed) {
                std::cmp::Ordering::Equal => self.open_effect = None,
                std::cmp::Ordering::Greater => *open_index -= 1,
                std::cmp::Ordering::Less => {}
            }
        }
    }

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
            // The strips with fewer effects leave the difference empty on
            // top, so the faders stay on one line.
            let effect_slots = timeline
                .tracks_of_kind(TrackKind::Audio)
                .map(|(_, t)| &t.mix)
                .chain([&timeline.master])
                .map(|strip| strip.effects.len())
                .max()
                .unwrap_or(0);
            let mut edits: Vec<(StripEdit, MixerChannel)> = Vec::new();
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
                            effect_slots,
                        );
                        edits.push((edit, channel));
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
                        effect_slots,
                    );
                    edits.push((edit, MixerChannel::Master));
                });
            });
            if falling {
                ui.ctx().request_repaint();
            }
            for (edit, channel) in edits {
                state.follow_edit(&edit, timeline_id, channel);
                commands.extend(edit.into_commands(timeline_id, channel));
            }
        });
    match timeline_id.and_then(|id| project.timelines.get(id).map(|t| (id, t))) {
        Some((id, timeline)) => commands.extend(effect_window(ctx, state, id, timeline)),
        None => state.open_effect = None,
    }
    commands
}

fn normalize_settings(ui: &mut egui::Ui, mut target_db: f32) -> Option<AudioEffectKind> {
    let changed = ui
        .horizontal(|ui| {
            ui.label(t!("mixer.normalize_target"));
            slider_field(
                ui,
                &mut target_db,
                vv_core::NORMALIZE_TARGET_MIN..=vv_core::NORMALIZE_TARGET_MAX,
                0.1,
                1,
            )
        })
        .inner;
    ui.small(t!("mixer.normalize_hint"));
    changed.then_some(AudioEffectKind::Normalize { target_db })
}

/// A numeric field for an `f32` within `(min, max)`.
fn value_field(
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

fn compressor_settings(ui: &mut egui::Ui, params: &MultibandCompressor) -> Option<AudioEffectKind> {
    let mut params = params.clone();
    let mut changed = false;
    let (min_hz, max_hz) = MultibandCompressor::CROSSOVER_RANGE_HZ;
    ui.horizontal(|ui| {
        ui.label(t!("mixer.crossovers"));
        let [low, high] = &mut params.crossovers_hz;
        // A drag moves the frequency by a share of itself, as on a log scale.
        changed |= value_field(ui, low, (min_hz, *high), *low as f64 * 0.005, 0, " Hz");
        changed |= value_field(ui, high, (*low, max_hz), *high as f64 * 0.005, 0, " Hz");
    });
    ui.add_space(4.0);
    type Field = fn(&mut CompressorBand) -> &mut f32;
    let rows: [(String, Field, (f32, f32), f64, usize, &str); 5] = [
        (
            t!("mixer.threshold").into_owned(),
            |b| &mut b.threshold_db,
            CompressorBand::THRESHOLD_RANGE_DB,
            0.2,
            1,
            " dB",
        ),
        (
            t!("mixer.ratio").into_owned(),
            |b| &mut b.ratio,
            CompressorBand::RATIO_RANGE,
            0.05,
            1,
            ":1",
        ),
        (
            t!("mixer.attack").into_owned(),
            |b| &mut b.attack_ms,
            CompressorBand::ATTACK_RANGE_MS,
            0.2,
            1,
            " ms",
        ),
        (
            t!("mixer.release").into_owned(),
            |b| &mut b.release_ms,
            CompressorBand::RELEASE_RANGE_MS,
            2.0,
            0,
            " ms",
        ),
        (
            t!("mixer.makeup").into_owned(),
            |b| &mut b.makeup_db,
            CompressorBand::MAKEUP_RANGE_DB,
            0.1,
            1,
            " dB",
        ),
    ];
    egui::Grid::new("multiband_bands")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            ui.label("");
            for name in [
                t!("mixer.band_low"),
                t!("mixer.band_mid"),
                t!("mixer.band_high"),
            ] {
                ui.strong(name);
            }
            ui.end_row();
            for (label, field, range, speed, decimals, suffix) in rows {
                ui.label(label);
                for band in &mut params.bands {
                    changed |= value_field(ui, field(band), range, speed, decimals, suffix);
                }
                ui.end_row();
            }
        });
    changed.then_some(AudioEffectKind::MultibandCompressor(params))
}

fn channel_label(timeline: &Timeline, channel: MixerChannel) -> String {
    match channel {
        MixerChannel::Track(index) => timeline.track_label(index),
        MixerChannel::Master => t!("mixer.master").into_owned(),
    }
}

fn channel_strip(timeline: &Timeline, channel: MixerChannel) -> Option<&ChannelStrip> {
    match channel {
        MixerChannel::Track(index) => timeline.tracks.get(index).map(|t| &t.mix),
        MixerChannel::Master => Some(&timeline.master),
    }
}

pub(crate) fn effect_name(kind: &AudioEffectKind) -> String {
    match kind {
        AudioEffectKind::Normalize { .. } => t!("mixer.effect_normalize").into_owned(),
        AudioEffectKind::MultibandCompressor(_) => t!("mixer.effect_multiband").into_owned(),
    }
}

/// Settings of the effect in `state.open_effect`: closed when the effect is
/// gone (undo, another timeline).
fn effect_window(
    ctx: &egui::Context,
    state: &mut MixerPanelState,
    timeline_id: TimelineId,
    timeline: &Timeline,
) -> Vec<BoxedCommand> {
    let mut commands: Vec<BoxedCommand> = Vec::new();
    let Some((open_timeline, channel, index)) = state.open_effect else {
        return commands;
    };
    let effect = channel_strip(timeline, channel).and_then(|strip| strip.effects.get(index));
    let Some(effect) = effect.filter(|_| open_timeline == timeline_id).cloned() else {
        state.open_effect = None;
        return commands;
    };
    let mut open = true;
    let title = format!(
        "{} — {}",
        effect_name(&effect.kind),
        channel_label(timeline, channel)
    );
    egui::Window::new(title)
        .id(egui::Id::new("audio_effect_settings"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            if !effect.enabled {
                ui.weak(t!("mixer.effect_disabled"));
            }
            let edited = match &effect.kind {
                AudioEffectKind::Normalize { target_db } => normalize_settings(ui, *target_db),
                AudioEffectKind::MultibandCompressor(params) => compressor_settings(ui, params),
            };
            if let Some(kind) = edited {
                let edited = AudioEffect {
                    kind,
                    ..effect.clone()
                };
                commands.push(Box::new(SetAudioEffect::new(
                    timeline_id,
                    channel,
                    index,
                    edited,
                    CommandLabel::EditAudioEffect,
                )));
            }
        });
    if !open {
        state.open_effect = None;
    }
    commands
}

#[derive(Default)]
struct StripEdit {
    gain_db: Option<f32>,
    pan: Option<f32>,
    toggle_solo: Option<bool>,
    toggle_mute: Option<bool>,
    add_effect: Option<AudioEffectKind>,
    remove_effect: Option<usize>,
    /// From, to (index afterwards).
    move_effect: Option<(usize, usize)>,
    set_effect: Option<(usize, AudioEffect)>,
    open_settings: Option<usize>,
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
        if let Some(kind) = self.add_effect {
            commands.push(Box::new(AddAudioEffect::new(
                timeline,
                channel,
                AudioEffect::new(kind),
            )));
        }
        if let Some((index, effect)) = self.set_effect {
            commands.push(Box::new(SetAudioEffect::new(
                timeline,
                channel,
                index,
                effect,
                CommandLabel::ToggleAudioEffect,
            )));
        }
        if let Some((from, to)) = self.move_effect {
            commands.push(Box::new(MoveAudioEffect::new(timeline, channel, from, to)));
        }
        if let Some(index) = self.remove_effect {
            commands.push(Box::new(RemoveAudioEffect::new(timeline, channel, index)));
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
    effect_slots: usize,
) -> StripEdit {
    let mut edit = StripEdit::default();
    let frame = egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::symmetric(4, 6));
    frame.show(ui, |ui| {
        ui.set_width(STRIP_WIDTH);
        ui.vertical_centered(|ui| {
            ui.push_id(channel, |ui| {
                effects(ui, channel, &mix.effects, effect_slots, &mut edit);
                ui.add_space(6.0);
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

/// Where an effect dragged from `from` ends up when dropped in the gap
/// before `slot` (`slot` = the length for the end); `None` if it stays.
pub(crate) fn moved_to(from: usize, slot: usize) -> Option<usize> {
    let to = if slot > from { slot - 1 } else { slot };
    (to != from).then_some(to)
}

/// Where the effect at `index` is after the one at `from` moved to `to`.
pub(crate) fn index_after_move(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

#[derive(Clone, Copy)]
struct EffectDrag {
    channel: MixerChannel,
    index: usize,
}

/// The insert chain, top to bottom, and the "+" that appends to it. The
/// effects are dragged to reorder them within the chain.
fn effects(
    ui: &mut egui::Ui,
    channel: MixerChannel,
    effects: &[AudioEffect],
    slots: usize,
    edit: &mut StripEdit,
) {
    let row = egui::vec2(STRIP_WIDTH, EFFECT_ROW_HEIGHT);
    for _ in effects.len()..slots {
        ui.allocate_space(row);
    }
    for (index, effect) in effects.iter().enumerate() {
        let name = effect_name(&effect.kind);
        let (fill, text) = if effect.enabled {
            (crate::theme::ACCENT_FILL, egui::Color32::WHITE)
        } else {
            (
                egui::Color32::from_gray(45),
                ui.visuals().weak_text_color().gamma_multiply(0.7),
            )
        };
        let button = egui::Button::new(egui::RichText::new(&name).small().color(text))
            .fill(fill)
            .truncate()
            .sense(egui::Sense::click_and_drag());
        let response = ui.add_sized(row, button).on_hover_text(&name);
        if response.clicked() {
            edit.open_settings = Some(index);
        }
        if response.drag_started() {
            response.dnd_set_drag_payload(EffectDrag { channel, index });
        }
        if response.dragged() {
            paint_drag_ghost(ui, &name, fill, text);
        }
        if let Some(drag) = response.dnd_hover_payload::<EffectDrag>()
            && drag.channel == channel
            && let Some(pointer) = ui.ctx().pointer_interact_pos()
        {
            let below = pointer.y > response.rect.center().y;
            let gap = ui.spacing().item_spacing.y / 2.0;
            let y = if below {
                response.rect.bottom() + gap
            } else {
                response.rect.top() - gap
            };
            ui.painter().hline(
                response.rect.x_range(),
                y,
                egui::Stroke::new(2.0, crate::theme::ACCENT),
            );
            if let Some(drag) = response.dnd_release_payload::<EffectDrag>()
                && let Some(to) = moved_to(drag.index, index + usize::from(below))
            {
                edit.move_effect = Some((drag.index, to));
            }
        }
        egui::Popup::context_menu(&response).show(|ui| {
            let toggle = if effect.enabled {
                t!("mixer.effect_disable")
            } else {
                t!("mixer.effect_enable")
            };
            if ui.button(toggle).clicked() {
                let toggled = AudioEffect {
                    enabled: !effect.enabled,
                    ..effect.clone()
                };
                edit.set_effect = Some((index, toggled));
            }
            if ui.button(t!("mixer.effect_remove")).clicked() {
                edit.remove_effect = Some(index);
            }
        });
    }
    let plus = ui
        .add_sized(row, egui::Button::new("+"))
        .on_hover_text(t!("mixer.add_effect"));
    egui::Popup::menu(&plus).show(|ui| {
        for kind in AudioEffectKind::ALL {
            if ui.button(effect_name(&kind)).clicked() {
                edit.add_effect = Some(kind);
            }
        }
    });
}

fn paint_drag_ghost(ui: &egui::Ui, name: &str, fill: egui::Color32, text: egui::Color32) {
    let Some(pointer) = ui.ctx().pointer_interact_pos() else {
        return;
    };
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("audio_effect_drag"),
    ));
    let rect = egui::Rect::from_center_size(pointer, egui::vec2(STRIP_WIDTH, EFFECT_ROW_HEIGHT));
    painter.rect_filled(rect, 3.0, fill.gamma_multiply(0.8));
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        name,
        egui::FontId::proportional(11.0),
        text,
    );
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
