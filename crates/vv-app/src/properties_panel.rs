//! Properties panel (inspector): Video/Audio tabs, parameter rows
//! with keyframes, title editor.

use std::borrow::Cow;

use super::*;

/// Panel tabs: labels, the active one underlined.
pub(crate) fn properties_tab_bar(ui: &mut egui::Ui, current: &mut PropertiesTab) {
    let tabs = [
        (PropertiesTab::Video, t!("props.tab_video")),
        (PropertiesTab::Audio, t!("props.tab_audio")),
        (PropertiesTab::Selection, t!("props.tab_selection")),
    ];
    const TAB_HEIGHT: f32 = 26.0;
    const UNDERLINE: egui::Color32 = egui::Color32::from_rgb(220, 60, 60);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for (tab, label) in tabs {
            let active = *current == tab;
            let galley = ui.painter().layout_no_wrap(
                label.to_string(),
                egui::FontId::proportional(13.0),
                egui::Color32::PLACEHOLDER,
            );
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(galley.size().x + 24.0, TAB_HEIGHT),
                egui::Sense::click(),
            );
            if response.clicked() {
                *current = tab;
            }
            let color = if active {
                ui.visuals().strong_text_color()
            } else if response.hovered() {
                ui.visuals().text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            let text_pos = egui::pos2(
                rect.center().x - galley.size().x / 2.0,
                rect.center().y - galley.size().y / 2.0,
            );
            ui.painter().galley(text_pos, galley, color);
            if active {
                let y = rect.bottom() - 1.0;
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    egui::Stroke::new(2.0, UNDERLINE),
                );
            }
        }
    });
}

/// Title/Settings of the Video tab of a text clip: two full-width
/// halves.
pub(crate) fn video_subtab_bar(ui: &mut egui::Ui, current: &mut VideoSubTab) {
    ui.columns(2, |cols| {
        for (col, (tab, label)) in cols.iter_mut().zip([
            (VideoSubTab::Title, t!("props.subtab_title")),
            (VideoSubTab::Settings, t!("props.subtab_settings")),
        ]) {
            let button = egui::Button::selectable(*current == tab, label)
                .min_size(egui::vec2(col.available_width(), 22.0));
            if col.add(button).clicked() {
                *current = tab;
            }
        }
    });
}

/// Text alignment button: lines drawn like the classic icon,
/// shorter where the text does not reach the margin.
pub(crate) fn text_align_button(
    ui: &mut egui::Ui,
    selected: bool,
    align: vv_core::TextAlign,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(24.0, 20.0), egui::Sense::click());
    let visuals = ui.style().interact_selectable(&response, selected);
    let painter = ui.painter();
    if selected || response.hovered() {
        painter.rect_filled(rect, 3.0, visuals.weak_bg_fill);
    }
    let area = rect.shrink2(egui::vec2(6.0, 5.0));
    for (i, full) in [true, false, true, false].into_iter().enumerate() {
        let y = area.top() + i as f32 * area.height() / 3.0;
        let w = if full || align == vv_core::TextAlign::Justify {
            area.width()
        } else {
            area.width() * 0.6
        };
        let x0 = match align {
            vv_core::TextAlign::Left | vv_core::TextAlign::Justify => area.left(),
            vv_core::TextAlign::Center => area.center().x - w / 2.0,
            vv_core::TextAlign::Right => area.right() - w,
        };
        painter.line_segment(
            [egui::pos2(x0, y), egui::pos2(x0 + w, y)],
            egui::Stroke::new(1.5, visuals.fg_stroke.color),
        );
    }
    response
}

/// Title tab: edits `title` in place, `true` if anything changed.
pub(crate) fn title_editor(
    ui: &mut egui::Ui,
    title: &mut vv_core::TitleParams,
    timeline_size: (u32, u32),
    fonts: &mut FontCatalog,
) -> bool {
    use vv_core::{FontCase, HAnchor, TextAlign, VAnchor};
    let defaults = vv_core::TitleParams::default();
    let before = title.clone();
    let (frame_w, frame_h) = (timeline_size.0 as f32, timeline_size.1 as f32);

    ui.label(egui::RichText::new(t!("props.text")).strong());
    ui.add(
        egui::TextEdit::multiline(&mut title.content)
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );
    ui.add_space(4.0);

    let row = param_row(ui, &t!("props.font"), None, |ui| {
        let mut items = vec![(String::new(), "Sans-serif".to_string(), true)];
        items.extend(
            fonts
                .families()
                .iter()
                .map(|f| (f.clone(), f.clone(), true)),
        );
        preview_combo(
            ui,
            "title_font_family",
            &mut title.font_family,
            &items,
            Some(ui.available_width()),
            Some(320.0),
            None,
        )
    });
    if row.reset {
        title.font_family = defaults.font_family.clone();
    }

    let row = param_row(ui, &t!("props.style"), None, |ui| {
        let mut faces = fonts.faces(&title.font_family).to_vec();
        if faces.is_empty() {
            faces = [(400, false), (700, false), (400, true), (700, true)]
                .into_iter()
                .map(|(weight, italic)| vv_render::text::FontFace {
                    weight,
                    italic,
                    name: vv_render::text::face_name(weight, italic),
                })
                .collect();
        }
        let items: Vec<_> = faces
            .iter()
            .map(|face| ((face.weight, face.italic), face.name.clone(), true))
            .collect();
        let mut face = (title.font_weight, title.italic);
        let changed = preview_combo(
            ui,
            "title_font_face",
            &mut face,
            &items,
            Some(ui.available_width()),
            None,
            None,
        );
        if changed {
            (title.font_weight, title.italic) = face;
        }
        changed
    });
    if row.reset {
        title.font_weight = defaults.font_weight;
        title.italic = defaults.italic;
    }

    if color_row(ui, &t!("props.color"), &mut title.color).reset {
        title.color = defaults.color;
    }

    for (label, value, default, range, speed) in [
        (
            t!("props.size"),
            &mut title.size,
            defaults.size,
            1.0..=1000.0,
            1.0,
        ),
        (
            t!("props.tracking"),
            &mut title.tracking,
            defaults.tracking,
            -300.0..=1000.0,
            1.0,
        ),
        (
            t!("props.line_spacing"),
            &mut title.line_spacing,
            defaults.line_spacing,
            -200.0..=500.0,
            1.0,
        ),
    ] {
        let row = param_row(ui, &label, None, |ui| {
            slider_field(ui, value, range, speed, 0)
        });
        if row.reset {
            *value = default;
        }
    }

    let row = param_row(ui, &t!("props.decorations"), None, |ui| {
        let u = ui
            .selectable_label(title.underline, egui::RichText::new("U").underline())
            .on_hover_text(t!("props.underline"))
            .clicked();
        let s = ui
            .selectable_label(
                title.strikethrough,
                egui::RichText::new("S").strikethrough(),
            )
            .on_hover_text(t!("props.strikethrough"))
            .clicked();
        title.underline ^= u;
        title.strikethrough ^= s;
        u || s
    });
    if row.reset {
        title.underline = defaults.underline;
        title.strikethrough = defaults.strikethrough;
    }

    let row = param_row(ui, &t!("props.case"), None, |ui| {
        let label = |case| match case {
            FontCase::Mixed => t!("props.case_mixed"),
            FontCase::Upper => t!("props.case_upper"),
            FontCase::Lower => t!("props.case_lower"),
            FontCase::Title => t!("props.case_title"),
        };
        let items: Vec<_> = [
            FontCase::Mixed,
            FontCase::Upper,
            FontCase::Lower,
            FontCase::Title,
        ]
        .into_iter()
        .map(|case| (case, label(case).to_string(), true))
        .collect();
        preview_combo(
            ui,
            "title_font_case",
            &mut title.case,
            &items,
            Some(ui.available_width()),
            None,
            None,
        )
    });
    if row.reset {
        title.case = defaults.case;
    }

    let row = param_row(ui, &t!("props.align"), None, |ui| {
        let mut changed = false;
        for (align, hint) in [
            (TextAlign::Left, t!("props.align_left")),
            (TextAlign::Center, t!("props.align_center")),
            (TextAlign::Right, t!("props.align_right")),
            (TextAlign::Justify, t!("props.align_justify")),
        ] {
            if text_align_button(ui, title.align == align, align)
                .on_hover_text(hint)
                .clicked()
            {
                title.align = align;
                changed = true;
            }
        }
        changed
    });
    if row.reset {
        title.align = defaults.align;
    }

    let row = param_row(ui, &t!("props.anchor"), None, |ui| {
        let mut changed = false;
        for (anchor, text, hint) in [
            (HAnchor::Left, "⇤", t!("props.anchor_left")),
            (HAnchor::Center, "↔", t!("props.anchor_center")),
            (HAnchor::Right, "⇥", t!("props.anchor_right")),
        ] {
            if ui
                .selectable_label(title.anchor.0 == anchor, text)
                .on_hover_text(hint)
                .clicked()
            {
                title.anchor.0 = anchor;
                changed = true;
            }
        }
        ui.separator();
        for (anchor, text, hint) in [
            (VAnchor::Top, "⤒", t!("props.anchor_top")),
            (VAnchor::Middle, "↕", t!("props.anchor_center")),
            (VAnchor::Bottom, "⤓", t!("props.anchor_bottom")),
        ] {
            if ui
                .selectable_label(title.anchor.1 == anchor, text)
                .on_hover_text(hint)
                .clicked()
            {
                title.anchor.1 = anchor;
                changed = true;
            }
        }
        changed
    });
    if row.reset {
        title.anchor = defaults.anchor;
    }

    // Shown from the bottom-left corner, as in the reference
    // (960x540 = center of a 1080p frame); saved from the center.
    let row = param_row(ui, &t!("props.position"), None, |ui| {
        let mut x = title.position[0] + frame_w / 2.0;
        let mut y = title.position[1] + frame_h / 2.0;
        let changed = axis_field(ui, "X", &mut x, 1.0, 1, -frame_w..=frame_w * 2.0)
            | axis_field(ui, "Y", &mut y, 1.0, 1, -frame_h..=frame_h * 2.0);
        title.position = [x - frame_w / 2.0, y - frame_h / 2.0];
        changed
    });
    if row.reset {
        title.position = defaults.position;
    }

    ui.add_space(8.0);
    let shadow = &mut title.shadow;
    if title_section_header(ui, &t!("props.drop_shadow"), &mut shadow.enabled) {
        *shadow = vv_core::TitleShadow {
            enabled: shadow.enabled,
            ..Default::default()
        };
    }
    if shadow.enabled {
        let d = vv_core::TitleShadow::default();
        if color_row(ui, &t!("props.color"), &mut shadow.color).reset {
            shadow.color = d.color;
        }
        let row = param_row(ui, &t!("props.offset"), None, |ui| {
            axis_field(ui, "X", &mut shadow.offset[0], 0.5, 1, -frame_w..=frame_w)
                | axis_field(ui, "Y", &mut shadow.offset[1], 0.5, 1, -frame_h..=frame_h)
        });
        if row.reset {
            shadow.offset = d.offset;
        }
        for (label, value, default, range) in [
            (t!("props.blur"), &mut shadow.blur, d.blur, 0.0..=200.0),
            (
                t!("props.opacity"),
                &mut shadow.opacity,
                d.opacity,
                0.0..=100.0,
            ),
        ] {
            if param_row(ui, &label, None, |ui| {
                slider_field(ui, value, range, 0.5, 0)
            })
            .reset
            {
                *value = default;
            }
        }
    }

    ui.add_space(8.0);
    let bg = &mut title.background;
    if title_section_header(ui, &t!("props.background"), &mut bg.enabled) {
        *bg = vv_core::TitleBackground {
            enabled: bg.enabled,
            ..Default::default()
        };
    }
    if bg.enabled {
        let d = vv_core::TitleBackground::default();
        if color_row(ui, &t!("props.color"), &mut bg.color).reset {
            bg.color = d.color;
        }
        if color_row(ui, &t!("props.outline_color"), &mut bg.outline_color).reset {
            bg.outline_color = d.outline_color;
        }
        for (label, value, default, range, decimals) in [
            (
                t!("props.outline_width"),
                &mut bg.outline_width,
                d.outline_width,
                0.0..=100.0,
                0,
            ),
            (t!("props.width"), &mut bg.width, d.width, 0.0..=1.0, 3),
            (t!("props.height"), &mut bg.height, d.height, 0.0..=1.0, 3),
            (
                t!("props.corner_radius"),
                &mut bg.corner_radius,
                d.corner_radius,
                0.0..=0.5,
                3,
            ),
        ] {
            let speed = if decimals == 0 { 0.5 } else { 0.005 };
            let row = param_row(ui, &label, None, |ui| {
                slider_field(ui, value, range, speed, decimals)
            });
            if row.reset {
                *value = default;
            }
        }
        let row = param_row(ui, &t!("props.center"), None, |ui| {
            axis_field(ui, "X", &mut bg.center[0], 1.0, 1, -frame_w..=frame_w)
                | axis_field(ui, "Y", &mut bg.center[1], 1.0, 1, -frame_h..=frame_h)
        });
        if row.reset {
            bg.center = d.center;
        }
        let row = param_row(ui, &t!("props.opacity"), None, |ui| {
            slider_field(ui, &mut bg.opacity, 0.0..=100.0, 0.5, 0)
        });
        if row.reset {
            bg.opacity = d.opacity;
        }
    }

    *title != before
}

pub(crate) fn color_row(ui: &mut egui::Ui, label: &str, color: &mut vv_core::Rgba) -> RowResponse {
    param_row(ui, label, None, |ui| {
        let mut rgba: [f32; 4] = (*color).into();
        let changed = ui.color_edit_button_rgba_unmultiplied(&mut rgba).changed();
        *color = rgba.into();
        changed
    })
}

/// Header of a toggleable section: switch, title and reset of the
/// whole section (`true` if clicked).
pub(crate) fn title_section_header(ui: &mut egui::Ui, title: &str, enabled: &mut bool) -> bool {
    let mut reset = false;
    ui.horizontal(|ui| {
        toggle_switch(ui, enabled);
        ui.label(egui::RichText::new(title).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            reset = ui
                .small_button("↺")
                .on_hover_text(t!("props.reset_section"))
                .clicked();
        });
    });
    reset
}

pub(crate) fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(26.0, 14.0);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool(response.id, *on);
    let painter = ui.painter();
    let track = if *on {
        egui::Color32::from_rgb(200, 60, 60)
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };
    painter.rect_filled(rect, rect.height() / 2.0, track);
    let r = rect.height() / 2.0 - 2.0;
    let x = egui::lerp(rect.left() + r + 2.0..=rect.right() - r - 2.0, t);
    painter.circle_filled(egui::pos2(x, rect.center().y), r, egui::Color32::WHITE);
    response
}

/// Brings onto `target` only the fields the user changed (`before` ->
/// `after`, read from the primary clip): with several titles selected, changing
/// the color must not overwrite the text of the others.
pub(crate) fn apply_title_edit(
    target: &vv_core::TitleParams,
    before: &vv_core::TitleParams,
    after: &vv_core::TitleParams,
) -> vv_core::TitleParams {
    let mut out = target.clone();
    macro_rules! copy_changed {
        ($group:ident : $($name:ident),*) => {
            $(if before.$group.$name != after.$group.$name {
                out.$group.$name = after.$group.$name.clone();
            })*
        };
        ($($field:ident),*) => {
            $(if before.$field != after.$field {
                out.$field = after.$field.clone();
            })*
        };
    }
    copy_changed!(shadow: enabled, color, offset, blur, opacity);
    copy_changed!(
        background: enabled,
        color,
        outline_color,
        outline_width,
        width,
        height,
        corner_radius,
        center,
        opacity
    );
    copy_changed!(
        content,
        font_family,
        font_weight,
        italic,
        color,
        size,
        tracking,
        line_spacing,
        underline,
        strikethrough,
        case,
        align,
        anchor,
        position
    );
    out
}

/// Width of the label column in the parameters panel:
/// all right-aligned, as in the inspector of an NLE.
pub(crate) const PARAM_LABEL_WIDTH: f32 = 96.0;

/// The keyframe state of a panel row, for its diamond.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RowKeyframe {
    pub(crate) on_keyframe: bool,
    /// Nearest keyframes before/after the current frame, in source
    /// frames: where the navigation arrows lead.
    pub(crate) prev: Option<FrameIdx>,
    pub(crate) next: Option<FrameIdx>,
}

/// What happened in a panel row during this UI frame.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RowResponse {
    pub(crate) changed: bool,
    pub(crate) reset: bool,
    pub(crate) toggled_keyframe: bool,
    /// Source frame to move the playhead to (arrow clicked).
    pub(crate) goto: Option<FrameIdx>,
}

/// A row of the parameters panel: label, controls, the keyframe
/// diamond with its navigation arrows (absent for non-animatable
/// parameters) and the reset of that single row.
pub(crate) fn param_row(
    ui: &mut egui::Ui,
    label: &str,
    keyframe: Option<RowKeyframe>,
    contents: impl FnOnce(&mut egui::Ui) -> bool,
) -> RowResponse {
    let mut response = RowResponse::default();
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(PARAM_LABEL_WIDTH, 18.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                // Truncated, not wrapped nor overflowing: the panel takes as its
                // minimum width the one it has (`set_min_width`), so
                // a label wider than `PARAM_LABEL_WIDTH` widens it, and
                // on the next frame it widens again, without ever stopping.
                ui.add(egui::Label::new(label).truncate())
                    .on_hover_text(label);
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            response.reset = ui
                .small_button("↺")
                .on_hover_text(t!("props.reset_param"))
                .clicked();
            // Separates the keyframe controls from the reset, which sits right
            // to the right (we are in a right->left layout).
            ui.add_space(8.0);
            if let Some(keyframe) = keyframe {
                // Arrows and diamond in a fixed area: the diamond must not move
                // when an arrow disappears.
                let spacing = ui.spacing().item_spacing.x;
                let width = KEYFRAME_ARROW_SIZE.x * 2.0 + KEYFRAME_DIAMOND_SIZE.x + spacing * 2.0;
                ui.allocate_ui_with_layout(
                    egui::vec2(width, KEYFRAME_DIAMOND_SIZE.y),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        let prev =
                            keyframe_arrow(ui, "◀", keyframe.prev, &t!("props.prev_keyframe"));
                        response.toggled_keyframe =
                            keyframe_button(ui, keyframe.on_keyframe).clicked();
                        let next =
                            keyframe_arrow(ui, "▶", keyframe.next, &t!("props.next_keyframe"));
                        response.goto = prev.or(next);
                    },
                );
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                response.changed = contents(ui);
            });
        });
    });
    response
}

/// The effects of a clip targeted by the panel.
pub(crate) fn target_effects<'a>(
    tl: Option<&'a vv_core::Timeline>,
    t: &PanelTarget,
) -> Option<&'a vv_core::EffectStack> {
    tl?.clip(t.track_index, t.clip_id).map(|c| &c.effects)
}

/// Commands for the parameters changed relative to `before`, so the other
/// clips keep their own values. Default if the parameter is not animated,
/// a keyframe otherwise. Position moves by the same delta on all of them.
pub(crate) fn push_param_changes(
    pending: &mut Vec<BoxedCommand>,
    tl: Option<&vv_core::Timeline>,
    targets: &[PanelTarget],
    params: &[vv_core::TransformParam],
    transform: &vv_core::Transform,
    before: &vv_core::Transform,
) {
    let changed: Vec<_> = params
        .iter()
        .filter(|p| p.of(transform) != p.of(before))
        .collect();
    for t in targets {
        let Some(effects) = target_effects(tl, t) else {
            continue;
        };
        for &&param in &changed {
            let value = match param {
                vv_core::TransformParam::PositionX | vv_core::TransformParam::PositionY => {
                    effects.transform.track(param).value_at(t.source_frame) + param.of(transform)
                        - param.of(before)
                }
                _ => param.of(transform),
            };
            pending.push(if effects.transform.track(param).is_constant() {
                set_transform_param_default((t.timeline, t.track_index, t.clip_id), param, value)
            } else {
                upsert_transform_keyframe(
                    (t.timeline, t.track_index, t.clip_id),
                    t.source_frame,
                    param,
                    value,
                )
            });
        }
    }
}

/// A numeric field of a two-axis parameter (X/Y).
pub(crate) fn axis_field(
    ui: &mut egui::Ui,
    axis: &str,
    value: &mut f32,
    speed: f64,
    decimals: usize,
    range: std::ops::RangeInclusive<f32>,
) -> bool {
    ui.label(axis);
    let mut v = *value as f64;
    let changed = drag_field(
        ui,
        &mut v,
        speed,
        (*range.start() as f64)..=(*range.end() as f64),
        decimals,
        "",
    );
    if changed {
        *value = v as f32;
    }
    changed
}

/// Draggable numeric field. While dragging, the pointer is locked where it is
/// and the value follows the relative mouse movement: if it were
/// let run instead, the drag would stop once it reached the edge of the
/// screen. egui does not get there on its own, because with a locked pointer
/// it only receives `pointer.motion()` and no longer the absolute positions
/// `DragValue` is based on.
pub(crate) fn drag_field(
    ui: &mut egui::Ui,
    value: &mut f64,
    speed: f64,
    range: std::ops::RangeInclusive<f64>,
    decimals: usize,
    suffix: &str,
) -> bool {
    let mut shown = *value;
    let response = ui.add(
        egui::DragValue::new(&mut shown)
            .speed(0.0)
            .range(range.clone())
            .fixed_decimals(decimals)
            .min_decimals(decimals)
            .suffix(suffix)
            .update_while_editing(false),
    );
    let mut changed = false;
    if response.changed() {
        *value = shown;
        changed = true;
    }

    if response.drag_started() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::CursorGrab(
                egui::viewport::CursorGrab::Locked,
            ));
    }
    if response.dragged() {
        let (motion, precise) = ui.input(|i| (i.pointer.motion(), i.modifiers.shift));
        let dx = motion.map_or(0.0, |m| m.x) as f64;
        if dx != 0.0 {
            let speed = if precise { speed / 10.0 } else { speed };
            *value = (*value + dx * speed).clamp(*range.start(), *range.end());
            changed = true;
        }
    }
    if response.drag_stopped() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::CursorGrab(
                egui::viewport::CursorGrab::None,
            ));
    }
    changed
}

/// A single-value parameter: slider plus numeric field, as in the
/// reference inspector.
pub(crate) fn slider_field(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    speed: f64,
    decimals: usize,
) -> bool {
    // The row has already spent its share on label, keyframe and reset:
    // what is left (minus the numeric field) all goes to the slider.
    ui.spacing_mut().slider_width = (ui.available_width() - 76.0).clamp(80.0, 260.0);
    let slider = ui.add(
        egui::Slider::new(value, range.clone())
            .show_value(false)
            .trailing_fill(false),
    );
    let mut v = *value as f64;
    let drag_changed = drag_field(
        ui,
        &mut v,
        speed,
        (*range.start() as f64)..=(*range.end() as f64),
        decimals,
        "",
    );
    if drag_changed {
        *value = v as f32;
    }
    slider.changed() || drag_changed
}

/// Id of the currently open dropdown menu, for whoever needs to know there is
/// a preview in progress.
const OPEN_COMBO: &str = "preview_combo_open";

/// Is there an open dropdown applying the preview of its entries?
/// While it lasts, the changes must be grouped into a single undo step.
pub(crate) fn preview_combo_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<egui::Id>(egui::Id::new(OPEN_COMBO)))
        .is_some_and(|id| egui::ComboBox::is_open(ctx, id))
}

/// Dropdown with preview: the wheel scrolls it even when closed and, when
/// open, the entry under the pointer is applied immediately. If it closes
/// without confirming with a click (Esc, click outside) it goes back to the value
/// it had before opening.
///
/// `items` are `(value, label, enabled)`.
pub(crate) fn preview_combo<T>(
    ui: &mut egui::Ui,
    id_salt: &str,
    value: &mut T,
    items: &[(T, String, bool)],
    width: Option<f32>,
    height: Option<f32>,
    disabled_hint: Option<&str>,
) -> bool
where
    T: Clone + PartialEq + Send + Sync + 'static,
{
    let button_id = ui.make_persistent_id(id_salt);
    let original_id = button_id.with("preview_original");
    let was_open = egui::ComboBox::is_open(ui.ctx(), button_id);
    let before = value.clone();
    let mut confirmed = false;

    let selected_text = items
        .iter()
        .find(|(v, _, _)| v == value)
        .map(|(_, label, _)| label.clone())
        .unwrap_or_default();
    let mut combo = egui::ComboBox::from_id_salt(id_salt).selected_text(selected_text);
    if let Some(width) = width {
        combo = combo.width(width);
    }
    if let Some(height) = height {
        combo = combo.height(height);
    }

    let response = combo
        .show_ui(ui, |ui| {
            for (item, label, enabled) in items {
                let response = ui
                    .add_enabled_ui(*enabled, |ui| ui.selectable_label(item == value, label))
                    .inner;
                if !*enabled {
                    if let Some(hint) = disabled_hint {
                        response.on_disabled_hover_text(hint);
                    }
                    continue;
                }
                if response.clicked() {
                    *value = item.clone();
                    confirmed = true;
                } else if response.hovered() && item != value {
                    *value = item.clone();
                }
            }
        })
        .response;

    if !was_open && response.contains_pointer() {
        let notches: i32 = ui.input(|i| {
            i.raw
                .events
                .iter()
                .map(|e| match e {
                    egui::Event::MouseWheel { delta, .. } if delta.y != 0.0 => {
                        -delta.y.signum() as i32
                    }
                    _ => 0,
                })
                .sum()
        });
        if notches != 0 {
            let step = notches.signum() as isize;
            let start = items.iter().position(|(v, _, _)| v == value).unwrap_or(0) as isize;
            let mut i = start + step;
            while i >= 0 && i < items.len() as isize {
                if items[i as usize].2 {
                    *value = items[i as usize].0.clone();
                    break;
                }
                i += step;
            }
        }
        // The wheel over the menu must not also scroll the panel underneath.
        ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
    }

    let open = egui::ComboBox::is_open(ui.ctx(), button_id);
    if open {
        ui.ctx().data_mut(|d| {
            if !was_open {
                d.insert_temp(original_id, before.clone());
            }
            d.insert_temp(egui::Id::new(OPEN_COMBO), button_id);
        });
    } else if was_open {
        let original = ui.ctx().data_mut(|d| {
            let original = d.get_temp::<T>(original_id);
            d.remove::<T>(original_id);
            original
        });
        if let Some(original) = original
            && !confirmed
        {
            *value = original;
        }
    }

    *value != before
}

/// The lock holding the two zoom axes together.
pub(crate) fn link_button(ui: &mut egui::Ui, linked: &mut bool) -> egui::Response {
    let mut response = ui
        .selectable_label(*linked, "🔗")
        .on_hover_text(t!("props.link_zoom"));
    if response.clicked() {
        *linked = !*linked;
        response.mark_changed();
    }
    response
}

/// Keyframe diamond, drawn by hand: on some platforms (Asahi)
/// egui's fonts do not have ◇/◆.
pub(crate) fn keyframe_button(ui: &mut egui::Ui, on_keyframe: bool) -> egui::Response {
    let tooltip = if on_keyframe {
        t!("props.remove_keyframe")
    } else {
        t!("props.add_keyframe")
    };

    let (rect, response) = ui.allocate_exact_size(KEYFRAME_DIAMOND_SIZE, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let painter = ui.painter();
        painter.rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
        let c = rect.center();
        let r = 5.0;
        let diamond = vec![
            c + egui::vec2(0.0, -r),
            c + egui::vec2(r, 0.0),
            c + egui::vec2(0.0, r),
            c + egui::vec2(-r, 0.0),
        ];
        if on_keyframe {
            // Filled and red when the playhead is *on* a keyframe, as
            // in the reference inspector.
            painter.add(egui::Shape::convex_polygon(
                diamond,
                KEYFRAME_HERE_COLOR,
                egui::Stroke::NONE,
            ));
        } else {
            painter.add(egui::Shape::closed_line(diamond, visuals.fg_stroke));
        }
    }
    response.on_hover_text(tooltip)
}

/// Space for a keyframe navigation arrow: reserved even
/// when the arrow is absent, otherwise the diamond would move on every
/// playhead change.
pub(crate) const KEYFRAME_ARROW_SIZE: egui::Vec2 = egui::Vec2::new(16.0, 18.0);

/// Keyframe navigation arrow. Without keyframes on that side it
/// takes the same space, invisible.
pub(crate) fn keyframe_arrow(
    ui: &mut egui::Ui,
    label: &str,
    target: Option<FrameIdx>,
    tooltip: &str,
) -> Option<FrameIdx> {
    let button = egui::Button::new(label)
        .small()
        .min_size(KEYFRAME_ARROW_SIZE);
    match target {
        Some(frame) => ui
            .add(button)
            .on_hover_text(tooltip)
            .clicked()
            .then_some(frame),
        None => {
            ui.add_visible(false, button);
            None
        }
    }
}

/// Size of the keyframe diamond.
pub(crate) const KEYFRAME_DIAMOND_SIZE: egui::Vec2 = egui::Vec2::new(20.0, 20.0);

/// The red of the diamond when the playhead is on a keyframe.
pub(crate) const KEYFRAME_HERE_COLOR: egui::Color32 = egui::Color32::from_rgb(225, 70, 70);

/// The clip a panel command acts on: timeline, track, id.
pub(crate) type ClipRef = (TimelineId, usize, ClipId);

pub(crate) type BoxedCommand = Box<dyn vv_core::Command>;

pub(crate) fn set_transform_param_default(
    (tl, track, clip): ClipRef,
    param: vv_core::TransformParam,
    value: f32,
) -> BoxedCommand {
    Box::new(vv_core::set_clip_transform_param(
        tl, track, clip, param, value,
    ))
}

pub(crate) fn set_flip((tl, track, clip): ClipRef, value: [bool; 2]) -> BoxedCommand {
    Box::new(vv_core::set_clip_flip(tl, track, clip, value))
}

pub(crate) fn reset_transform_params(
    (tl, track, clip): ClipRef,
    params: Vec<vv_core::TransformParam>,
    reset_flip: bool,
) -> BoxedCommand {
    Box::new(vv_core::ResetTransformParams::new(
        tl, track, clip, params, reset_flip,
    ))
}

pub(crate) fn set_gain_default((tl, track, clip): ClipRef, value: f32) -> BoxedCommand {
    Box::new(vv_core::set_clip_gain(tl, track, clip, value))
}

pub(crate) fn reset_gain((tl, track, clip): ClipRef) -> BoxedCommand {
    Box::new(vv_core::reset_clip_gain(tl, track, clip))
}

pub(crate) fn set_color_default((tl, track, clip): ClipRef, value: vv_core::Rgba) -> BoxedCommand {
    Box::new(vv_core::SetClipColor::new(tl, track, clip, value))
}

pub(crate) fn set_title((tl, track, clip): ClipRef, value: vv_core::TitleParams) -> BoxedCommand {
    Box::new(vv_core::set_clip_title(tl, track, clip, value))
}

pub(crate) fn blend_mode_label(mode: vv_core::BlendMode) -> Cow<'static, str> {
    use vv_core::BlendMode as B;
    match mode {
        B::Normal => t!("blend.normal"),
        B::Add => t!("blend.add"),
        B::Multiply => t!("blend.multiply"),
        B::Screen => t!("blend.screen"),
        B::Overlay => t!("blend.overlay"),
        B::Darken => t!("blend.darken"),
        B::Lighten => t!("blend.lighten"),
        B::ColorDodge => t!("blend.color_dodge"),
        B::ColorBurn => t!("blend.color_burn"),
        B::HardLight => t!("blend.hard_light"),
        B::SoftLight => t!("blend.soft_light"),
        B::Difference => t!("blend.difference"),
        B::Exclusion => t!("blend.exclusion"),
        B::Subtract => t!("blend.subtract"),
        B::Divide => t!("blend.divide"),
    }
}

pub(crate) fn set_blend_mode(
    (tl, track, clip): ClipRef,
    value: vv_core::BlendMode,
) -> BoxedCommand {
    Box::new(vv_core::set_clip_blend_mode(tl, track, clip, value))
}

pub(crate) fn set_filters(
    (tl, track, clip): ClipRef,
    value: Vec<vv_core::ClipFilter>,
) -> BoxedCommand {
    Box::new(vv_core::set_clip_filters(tl, track, clip, value))
}

/// Command to save (or remove, with `value: None`) the transition
/// `sel`, single edge or crossing: the latter needs to find
/// `right_clip` from the existing crossing, it is not carried by
/// `TransitionSelection::Crossing` (which identifies only `left_clip`).
pub(crate) fn set_transition_command(
    project: &vv_core::Project,
    timeline_id: TimelineId,
    sel: timeline_ui::TransitionSelection,
    value: Option<vv_core::Transition>,
) -> BoxedCommand {
    match sel {
        timeline_ui::TransitionSelection::Edge((track_index, clip_id), edge) => Box::new(
            vv_core::set_clip_transition(timeline_id, track_index, clip_id, edge, value),
        ),
        timeline_ui::TransitionSelection::Crossing(track_index, left_clip) => {
            let right_clip = project.timelines[timeline_id]
                .tracks
                .get(track_index)
                .and_then(|t| t.crossing_from(left_clip))
                .map(|c| c.right_clip);
            let crossing_value = match (value, right_clip) {
                (Some(transition), Some(right_clip)) => Some(vv_core::CrossTransition {
                    left_clip,
                    right_clip,
                    transition,
                }),
                _ => None,
            };
            Box::new(vv_core::SetCrossTransition::new(
                timeline_id,
                track_index,
                left_clip,
                crossing_value,
            ))
        }
    }
}

pub(crate) fn upsert_keyframe(
    (tl, track, clip): ClipRef,
    frame: FrameIdx,
    value: vv_core::KeyframeValue,
) -> BoxedCommand {
    Box::new(vv_core::UpsertKeyframe::new(
        tl,
        track,
        clip,
        frame,
        value,
        vv_core::Interpolation::Linear,
    ))
}

pub(crate) fn upsert_transform_keyframe(
    clip: ClipRef,
    frame: FrameIdx,
    param: vv_core::TransformParam,
    value: f32,
) -> BoxedCommand {
    upsert_keyframe(
        clip,
        frame,
        vv_core::KeyframeValue::TransformParam(param, value),
    )
}

pub(crate) fn upsert_gain_keyframe(clip: ClipRef, frame: FrameIdx, value: f32) -> BoxedCommand {
    upsert_keyframe(clip, frame, vv_core::KeyframeValue::Gain(value))
}

pub(crate) fn upsert_color_keyframe(
    clip: ClipRef,
    frame: FrameIdx,
    value: vv_core::Rgba,
) -> BoxedCommand {
    upsert_keyframe(clip, frame, vv_core::KeyframeValue::Color(value))
}

pub(crate) fn remove_keyframe(
    (tl, track, clip): ClipRef,
    frame: FrameIdx,
    target: vv_core::KeyframeTarget,
) -> BoxedCommand {
    Box::new(vv_core::RemoveKeyframe::new(tl, track, clip, target, frame))
}

pub(crate) fn remove_transform_keyframe(
    clip: ClipRef,
    frame: FrameIdx,
    param: vv_core::TransformParam,
) -> BoxedCommand {
    remove_keyframe(clip, frame, vv_core::KeyframeTarget::TransformParam(param))
}

pub(crate) fn remove_gain_keyframe(clip: ClipRef, frame: FrameIdx) -> BoxedCommand {
    remove_keyframe(clip, frame, vv_core::KeyframeTarget::Gain)
}

pub(crate) fn remove_color_keyframe(clip: ClipRef, frame: FrameIdx) -> BoxedCommand {
    remove_keyframe(clip, frame, vv_core::KeyframeTarget::Color)
}

impl VenturiApp {
    /// The "Selection" tab: everything that is selected, video and audio
    /// together, with the data that used to sit at the top of the parameters
    /// panel (track, start, duration, current frame).
    pub(crate) fn show_selection_list(
        &self,
        ui: &mut egui::Ui,
        video_targets: &[PanelTarget],
        audio_targets: &[PanelTarget],
    ) {
        let rows: Vec<(Cow<str>, &PanelTarget)> = video_targets
            .iter()
            .map(|t| (t!("props.kind_video"), t))
            .chain(audio_targets.iter().map(|t| (t!("props.kind_audio"), t)))
            .collect();
        ui.label(t!("props.selected_clips", count = rows.len()));
        ui.add_space(4.0);
        egui::Grid::new("selection_list")
            .num_columns(6)
            .striped(true)
            .spacing(egui::vec2(10.0, 4.0))
            .show(ui, |ui| {
                for header in [
                    t!("props.col_kind"),
                    t!("props.col_track"),
                    t!("props.col_name"),
                    t!("props.col_start"),
                    t!("props.col_duration"),
                    t!("props.col_frame"),
                ] {
                    ui.label(egui::RichText::new(header).strong());
                }
                ui.end_row();
                for (kind, target) in rows {
                    let clip = self.timeline_id.and_then(|tid| {
                        self.project.timelines[tid].clip(target.track_index, target.clip_id)
                    });
                    let (name, len) = match clip {
                        Some(clip) => (
                            match &clip.source {
                                vv_core::ClipSource::Media(id) => self
                                    .project
                                    .media_pool
                                    .get(*id)
                                    .map(|item| file_label(&item.path))
                                    .unwrap_or_else(|| "⚠ offline".to_string()),
                                vv_core::ClipSource::SolidColor => {
                                    t!("generator.solid_color").into_owned()
                                }
                                vv_core::ClipSource::Text => t!("generator.text").into_owned(),
                            },
                            clip.timeline_len,
                        ),
                        None => ("?".to_string(), 0),
                    };
                    ui.label(kind);
                    ui.label(target.track_index.to_string());
                    ui.label(name);
                    ui.label(target.timeline_start.to_string());
                    ui.label(len.to_string());
                    ui.label(target.source_frame.to_string());
                    ui.end_row();
                }
            });
    }

    /// The values to show in the properties panel for a target clip,
    /// evaluated at its `source_frame`.
    pub(crate) fn clip_panel_info(&self, target: PanelTarget) -> Option<ClipPanelInfo> {
        let timeline_id = self.timeline_id?;
        let timeline_size = self.project.timelines[timeline_id].resolution;
        let clip = self.project.timelines[timeline_id].clip(target.track_index, target.clip_id)?;
        let frame = target.source_frame;
        // A keyframe outside the trim would take the playhead outside the clip.
        let in_clip = |f: &FrameIdx| (clip.source_in()..clip.source_out()).contains(f);
        Some(ClipPanelInfo {
            is_solid_color: target.is_solid_color,
            source_size: frame_provider::clip_source_size(&self.project, clip, timeline_size),
            timeline_size,
            params: vv_core::TransformParam::ALL
                .iter()
                .map(|p| {
                    let track = clip.effects.transform.track(*p);
                    RowKeyframe {
                        on_keyframe: track.keyframe_at(frame).is_some(),
                        prev: clip
                            .effects
                            .transform
                            .previous_keyframe(&[*p], frame)
                            .filter(in_clip),
                        next: clip
                            .effects
                            .transform
                            .next_keyframe(&[*p], frame)
                            .filter(in_clip),
                    }
                })
                .collect(),
            transform: clip.effects.transform.value_at(frame),
            gain_kf_here: clip.effects.gain_db.keyframe_at(frame).is_some(),
            gain: clip.effects.gain_db.value_at(frame),
            gain_prev: clip.effects.gain_db.keyframe_before(frame).filter(in_clip),
            gain_next: clip.effects.gain_db.keyframe_after(frame).filter(in_clip),
            color_kf_here: clip
                .effects
                .color
                .as_ref()
                .and_then(|k| k.keyframe_at(frame))
                .is_some(),
            color: clip
                .effects
                .color
                .as_ref()
                .map(|k| k.value_at(frame))
                .unwrap_or(DEFAULT_SOLID_COLOR),
            title: clip.effects.title.clone(),
            filters: clip.effects.filters.clone(),
            blend_mode: clip.effects.blend_mode,
        })
    }

    pub(crate) fn apply_effect_changes(
        &mut self,
        mut commands: Vec<BoxedCommand>,
        pointer_down: bool,
    ) {
        if !commands.is_empty() {
            if pointer_down && self.edit_drag_group.is_none() {
                self.edit_drag_group = Some(self.history.begin_group());
            }
            let cmd = if commands.len() == 1 {
                commands.remove(0)
            } else {
                Box::new(vv_core::CompositeCommand::new(
                    commands[0].label(),
                    commands,
                ))
            };
            self.history.do_command(&mut self.project, cmd);
        }
        if !pointer_down && let Some(mark) = self.edit_drag_group.take() {
            self.history.end_group(mark);
        }
    }

    /// Panel of a selected transition (single edge or crossing):
    /// takes the place of the Video/Audio/Selection tabs while it stays
    /// selected (see `TimelineState::selected_transition`).
    fn show_transition_panel(
        &mut self,
        ui: &mut egui::Ui,
        sel: timeline_ui::TransitionSelection,
        pending: &mut Vec<BoxedCommand>,
    ) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let tl = &self.project.timelines[timeline_id];
        // `before`/`max_duration` tell the two cases apart only here: from here
        // down the controls are identical, and the saving/removal at the
        // bottom picks the right command itself based on `sel`.
        let (before, max_duration) = match sel {
            timeline_ui::TransitionSelection::Edge((track_index, clip_id), edge) => {
                let Some(clip) = tl.clip(track_index, clip_id) else {
                    return;
                };
                let Some(transition) = (match edge {
                    vv_core::FadeEdge::In => clip.effects.transition_in.clone(),
                    vv_core::FadeEdge::Out => clip.effects.transition_out.clone(),
                }) else {
                    return;
                };
                (transition, clip.timeline_len.max(1))
            }
            timeline_ui::TransitionSelection::Crossing(track_index, left_clip) => {
                let Some(track) = tl.tracks.get(track_index) else {
                    return;
                };
                let Some(crossing) = track.crossing_from(left_clip) else {
                    return;
                };
                let max_duration = match (
                    track.clip(crossing.left_clip),
                    track.clip(crossing.right_clip),
                ) {
                    (Some(left), Some(right)) => {
                        (2 * left.timeline_len.min(right.timeline_len)).max(1)
                    }
                    _ => return,
                };
                (crossing.transition.clone(), max_duration)
            }
        };
        // The clip (or the pair) was deleted or the transition removed
        // from under the selection (e.g. undo): nothing to show, already handled
        // above with the `return`s.
        let mut transition = before.clone();
        let fps = tl.fps.as_f64().max(1.0);

        ui.heading(t!("props.transition"));
        ui.label(timeline_ui::transition_kind_label(transition.kind));
        ui.separator();

        ui.horizontal(|ui| {
            ui.label(t!("props.duration"));
            let mut secs = transition.duration as f64 / fps;
            if drag_field(
                ui,
                &mut secs,
                0.02,
                0.0..=(max_duration as f64 / fps),
                2,
                " s",
            ) {
                transition.duration = ((secs * fps).round() as FrameIdx).clamp(1, max_duration);
            }
            let mut frames = transition.duration as f64;
            if drag_field(ui, &mut frames, 0.25, 1.0..=max_duration as f64, 0, "") {
                transition.duration = (frames.round() as FrameIdx).clamp(1, max_duration);
            }
            ui.label(t!("props.frames"));
        });

        ui.horizontal(|ui| {
            ui.label(t!("props.direction"));
            let items: Vec<_> = vv_core::PushDirection::ALL
                .iter()
                .map(|d| (*d, timeline_ui::push_direction_label(*d).to_string(), true))
                .collect();
            preview_combo(
                ui,
                "transition_direction",
                &mut transition.direction,
                &items,
                None,
                None,
                None,
            );
        });

        ui.horizontal(|ui| {
            ui.label(t!("props.ease"));
            let items: Vec<_> = vv_core::Ease::ALL
                .iter()
                .map(|e| (*e, timeline_ui::ease_label(*e).to_string(), true))
                .collect();
            preview_combo(
                ui,
                "transition_ease",
                &mut transition.ease,
                &items,
                None,
                None,
                None,
            );
        });

        ui.horizontal(|ui| {
            ui.label(t!("props.transition_curve"));
            ui.add(egui::Slider::new(&mut transition.curve, 0.0..=1.0));
        });

        if transition != before {
            pending.push(set_transition_command(
                &self.project,
                timeline_id,
                sel,
                Some(transition),
            ));
        }

        ui.add_space(8.0);
        if ui.button(t!("props.remove_transition")).clicked() {
            pending.push(set_transition_command(
                &self.project,
                timeline_id,
                sel,
                None,
            ));
            self.timeline_state.selected_transition = None;
        }
    }

    /// Returns the effect changes and the playhead jump, if any,
    /// to be applied after the drawing.
    pub(crate) fn show_properties_panel(
        &mut self,
        ui: &mut egui::Ui,
        video_targets: &[PanelTarget],
        audio_targets: &[PanelTarget],
    ) -> (Vec<BoxedCommand>, Option<FrameIdx>) {
        let mut pending_effects: Vec<BoxedCommand> = Vec::new();
        // The panel's keyframe navigation arrows move the
        // playhead: applied after the drawing, like the effect changes.
        let mut pending_playhead: Option<FrameIdx> = None;

        if self.settings.panels.inspector_open {
            egui::Panel::right("properties")
                .resizable(true)
                .default_size(self.settings.panels.inspector_width)
                .show(ui, |ui| {
                    // Without it, the panel would shrink to its content and its resize
                    // would spring back.
                    ui.set_min_width(ui.available_width());

                    // Fixed bar: a floating one would cover the resets on the right.
                    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                        )
                        .show(ui, |ui| {
                        let selected_count = self.timeline_state.selected.len();
                        if let Some(sel) = self.timeline_state.selected_transition {
                            self.show_transition_panel(ui, sel, &mut pending_effects);
                        } else if selected_count > 0 {
                            properties_tab_bar(ui, &mut self.properties_tab);
                            ui.separator();

                            // Values from the first clip of the tab, changes to all of them.
                            if self.properties_tab == PropertiesTab::Selection {
                                self.show_selection_list(ui, video_targets, audio_targets);
                            } else {
                            let targets = if self.properties_tab == PropertiesTab::Audio {
                                audio_targets
                            } else {
                                video_targets
                            };
                            let primary = targets.first().copied();
                            let info = primary.and_then(|t| self.clip_panel_info(t));

                            match (primary, info) {
                                (Some(primary), Some(info)) => {
                                    let ClipPanelInfo {
                                        is_solid_color,
                                        source_size,
                                        timeline_size,
                                        mut transform,
                                        gain_kf_here,
                                        mut gain,
                                        gain_prev,
                                        gain_next,
                                        color_kf_here,
                                        mut color,
                                        ..
                                    } = info.clone();
                                    if targets.len() > 1 {
                                        ui.small(t!("props.applies_to_all", count = targets.len()));
                                    }
                                    ui.separator();

                                    let is_text = self.properties_tab == PropertiesTab::Video
                                        && info.title.is_some();
                                    if is_text {
                                        video_subtab_bar(ui, &mut self.video_subtab);
                                        ui.add_space(4.0);
                                    }
                                    match self.properties_tab {
                                        // The Selection tab does not get here:
                                        // it was served earlier, without a primary clip.
                                        PropertiesTab::Selection => {}
                                        PropertiesTab::Video
                                            if is_text && self.video_subtab == VideoSubTab::Title =>
                                        {
                                            let before = info.title.clone().unwrap_or_default();
                                            let mut title = before.clone();
                                            if title_editor(ui, &mut title, timeline_size, &mut self.fonts) {
                                                let tl = self.timeline_id.map(|id| &self.project.timelines[id]);
                                                for t in targets.iter().filter(|t| t.is_text) {
                                                    let Some(current) = tl
                                                        .and_then(|tl| tl.tracks.get(t.track_index))
                                                        .and_then(|tr| tr.clip(t.clip_id))
                                                        .and_then(|c| c.effects.title.as_ref())
                                                    else {
                                                        continue;
                                                    };
                                                    pending_effects.push(set_title((t.timeline, t.track_index, t.clip_id), apply_title_edit(current, &before, &title)));
                                                }
                                            }
                                        }
                                        PropertiesTab::Video => {
                                            use vv_core::TransformParam as P;
                                            let (frame_w, frame_h) =
                                                (timeline_size.0 as f32, timeline_size.1 as f32);
                                            let (source_w, source_h) =
                                                (source_size.0 as f32, source_size.1 as f32);

                                            // Diamond of a row: on a keyframe if all its parameters are.
                                            let row_keyframe = |params: &[P]| RowKeyframe {
                                                on_keyframe: params
                                                    .iter()
                                                    .all(|p| info.params[p.index()].on_keyframe),
                                                prev: params
                                                    .iter()
                                                    .filter_map(|p| info.params[p.index()].prev)
                                                    .max(),
                                                next: params
                                                    .iter()
                                                    .filter_map(|p| info.params[p.index()].next)
                                                    .min(),
                                            };

                                            let section_reset = |ui: &mut egui::Ui, title: &str| {
                                                let mut clicked = false;
                                                ui.horizontal(|ui| {
                                                    ui.label(egui::RichText::new(title).strong());
                                                    ui.with_layout(
                                                        egui::Layout::right_to_left(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            clicked = ui
                                                                .small_button("↺")
                                                                .on_hover_text(t!("props.reset_section"))
                                                                .clicked();
                                                        },
                                                    );
                                                });
                                                clicked
                                            };

                                            const TRANSFORM_PARAMS: [P; 7] = [
                                                P::ZoomX,
                                                P::ZoomY,
                                                P::PositionX,
                                                P::PositionY,
                                                P::Rotation,
                                                P::AnchorX,
                                                P::AnchorY,
                                            ];
                                            const CROP_PARAMS: [P; 5] = [
                                                P::CropLeft,
                                                P::CropTop,
                                                P::CropRight,
                                                P::CropBottom,
                                                P::CropSoftness,
                                            ];

                                            // Rows with the parameters their diamond animates; resetting a row
                                            // clears them, keyframes included.
                                            let mut rows: Vec<(Vec<P>, RowResponse)> = Vec::new();
                                            let mut reset_groups: Vec<(Vec<P>, bool)> = Vec::new();

                                            if section_reset(ui, &t!("props.transform")) {
                                                reset_groups.push((TRANSFORM_PARAMS.to_vec(), true));
                                            }

                                            let zoom_params = vec![P::ZoomX, P::ZoomY];
                                            let row = param_row(
                                                ui,
                                                &t!("props.zoom"),
                                                Some(row_keyframe(&zoom_params)),
                                                |ui| {
                                                    let mut changed = axis_field(
                                                        ui,
                                                        "X",
                                                        &mut transform.zoom[0],
                                                        0.01,
                                                        3,
                                                        0.01..=20.0,
                                                    );
                                                    let y_changed = axis_field(
                                                        ui,
                                                        "Y",
                                                        &mut transform.zoom[1],
                                                        0.01,
                                                        3,
                                                        0.01..=20.0,
                                                    );
                                                    if link_button(ui, &mut self.zoom_link).changed()
                                                        && self.zoom_link
                                                    {
                                                        transform.zoom[1] = transform.zoom[0];
                                                        changed = true;
                                                    }
                                                    if self.zoom_link {
                                                        // The link works both
                                                        // ways: whichever was
                                                        // moved dictates the other.
                                                        if changed {
                                                            transform.zoom[1] = transform.zoom[0];
                                                        } else if y_changed {
                                                            transform.zoom[0] = transform.zoom[1];
                                                        }
                                                    }
                                                    changed || y_changed
                                                },
                                            );
                                            rows.push((zoom_params, row));

                                            let position_params = vec![P::PositionX, P::PositionY];
                                            let row = param_row(
                                                ui,
                                                &t!("props.position"),
                                                Some(row_keyframe(&position_params)),
                                                |ui| {
                                                    let x = axis_field(
                                                        ui,
                                                        "X",
                                                        &mut transform.position[0],
                                                        1.0,
                                                        1,
                                                        -frame_w..=frame_w,
                                                    );
                                                    let y = axis_field(
                                                        ui,
                                                        "Y",
                                                        &mut transform.position[1],
                                                        1.0,
                                                        1,
                                                        -frame_h..=frame_h,
                                                    );
                                                    x || y
                                                },
                                            );
                                            rows.push((position_params, row));

                                            let rotation_params = vec![P::Rotation];
                                            let row = param_row(
                                                ui,
                                                &t!("props.rotation"),
                                                Some(row_keyframe(&rotation_params)),
                                                |ui| {
                                                    slider_field(
                                                        ui,
                                                        &mut transform.rotation,
                                                        -180.0..=180.0,
                                                        0.5,
                                                        1,
                                                    )
                                                },
                                            );
                                            rows.push((rotation_params, row));

                                            let anchor_params = vec![P::AnchorX, P::AnchorY];
                                            let row = param_row(
                                                ui,
                                                &t!("props.anchor_point"),
                                                Some(row_keyframe(&anchor_params)),
                                                |ui| {
                                                    let x = axis_field(
                                                        ui,
                                                        "X",
                                                        &mut transform.anchor[0],
                                                        1.0,
                                                        1,
                                                        -frame_w..=frame_w,
                                                    );
                                                    let y = axis_field(
                                                        ui,
                                                        "Y",
                                                        &mut transform.anchor[1],
                                                        1.0,
                                                        1,
                                                        -frame_h..=frame_h,
                                                    );
                                                    x || y
                                                },
                                            );
                                            rows.push((anchor_params, row));

                                            // The flip is not animated: no
                                            // diamond, only the reset.
                                            let flip_row = param_row(ui, &t!("props.flip"), None, |ui| {
                                                let x = ui
                                                    .selectable_label(transform.flip[0], "⬌")
                                                    .on_hover_text(t!("props.flip_h"))
                                                    .clicked();
                                                let y = ui
                                                    .selectable_label(transform.flip[1], "⬍")
                                                    .on_hover_text(t!("props.flip_v"))
                                                    .clicked();
                                                transform.flip[0] ^= x;
                                                transform.flip[1] ^= y;
                                                x || y
                                            });
                                            if flip_row.changed {
                                                let tl = self.timeline_id.map(|id| &self.project.timelines[id]);
                                                for t in targets {
                                                    let Some(effects) = target_effects(tl, t) else {
                                                        continue;
                                                    };
                                                    // Only the clicked axis.
                                                    let mut flip = effects.transform.flip;
                                                    for axis in 0..2 {
                                                        if transform.flip[axis] != info.transform.flip[axis] {
                                                            flip[axis] = transform.flip[axis];
                                                        }
                                                    }
                                                    pending_effects.push(set_flip((t.timeline, t.track_index, t.clip_id), flip));
                                                }
                                            }
                                            if flip_row.reset {
                                                reset_groups.push((Vec::new(), true));
                                            }

                                            ui.add_space(6.0);
                                            if section_reset(ui, &t!("props.cropping")) {
                                                reset_groups.push((CROP_PARAMS.to_vec(), false));
                                            }

                                            for (label, param, index, limit) in [
                                                (t!("props.crop_left"), P::CropLeft, 0, source_w),
                                                (t!("props.crop_right"), P::CropRight, 2, source_w),
                                                (t!("props.crop_top"), P::CropTop, 1, source_h),
                                                (t!("props.crop_bottom"), P::CropBottom, 3, source_h),
                                            ] {
                                                let params = vec![param];
                                                let row = param_row(
                                                    ui,
                                                    &label,
                                                    Some(row_keyframe(&params)),
                                                    |ui| {
                                                        slider_field(
                                                            ui,
                                                            &mut transform.crop[index],
                                                            0.0..=limit,
                                                            1.0,
                                                            1,
                                                        )
                                                    },
                                                );
                                                rows.push((params, row));
                                            }
                                            // The two opposite crops cannot
                                            // eat the whole frame from each other:
                                            // at least one pixel is left.
                                            transform.crop[0] =
                                                transform.crop[0].min(source_w - 1.0 - transform.crop[2]);
                                            transform.crop[1] =
                                                transform.crop[1].min(source_h - 1.0 - transform.crop[3]);

                                            let softness_params = vec![P::CropSoftness];
                                            let row = param_row(
                                                ui,
                                                &t!("props.softness"),
                                                Some(row_keyframe(&softness_params)),
                                                |ui| {
                                                    let limit = source_w.min(source_h) / 2.0;
                                                    slider_field(
                                                        ui,
                                                        &mut transform.crop_softness,
                                                        -limit..=limit,
                                                        1.0,
                                                        1,
                                                    )
                                                },
                                            );
                                            rows.push((softness_params, row));

                                            ui.add_space(6.0);
                                            let mut blend_mode = info.blend_mode;
                                            let mut blend_changed = false;
                                            if section_reset(ui, &t!("props.composite")) {
                                                reset_groups.push((vec![P::Opacity], false));
                                                blend_mode = vv_core::BlendMode::Normal;
                                                blend_changed = true;
                                            }

                                            let row = param_row(ui, &t!("props.composite_mode"), None, |ui| {
                                                let items: Vec<_> = vv_core::BlendMode::ALL
                                                    .iter()
                                                    .map(|mode| (*mode, blend_mode_label(*mode).to_string(), true))
                                                    .collect();
                                                preview_combo(
                                                    ui,
                                                    "clip_blend_mode",
                                                    &mut blend_mode,
                                                    &items,
                                                    Some(ui.available_width()),
                                                    None,
                                                    None,
                                                )
                                            });
                                            blend_changed |= row.changed;
                                            if row.reset {
                                                blend_mode = vv_core::BlendMode::Normal;
                                                blend_changed = true;
                                            }
                                            if blend_changed {
                                                for t in targets {
                                                    pending_effects.push(set_blend_mode((t.timeline, t.track_index, t.clip_id), blend_mode));
                                                }
                                            }

                                            let opacity_params = vec![P::Opacity];
                                            let row = param_row(
                                                ui,
                                                &t!("props.opacity"),
                                                Some(row_keyframe(&opacity_params)),
                                                |ui| {
                                                    slider_field(
                                                        ui,
                                                        &mut transform.opacity,
                                                        0.0..=100.0,
                                                        0.5,
                                                        2,
                                                    )
                                                },
                                            );
                                            rows.push((opacity_params, row));

                                            let tl = self.timeline_id.map(|id| &self.project.timelines[id]);
                                            for (params, row) in &rows {
                                                if row.changed {
                                                    push_param_changes(
                                                        &mut pending_effects,
                                                        tl,
                                                        targets,
                                                        params,
                                                        &transform,
                                                        &info.transform,
                                                    );
                                                }
                                                if row.toggled_keyframe {
                                                    let on_keyframe = params
                                                        .iter()
                                                        .all(|p| info.params[p.index()].on_keyframe);
                                                    for t in targets {
                                                        // The keyframe pins the value
                                                        // each clip already has.
                                                        let Some(effects) = target_effects(tl, t) else {
                                                            continue;
                                                        };
                                                        for p in params {
                                                            pending_effects.push(if on_keyframe {
                                                                remove_transform_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame, *p)
                                                            } else {
                                                                upsert_transform_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame, *p, effects
                                                                        .transform
                                                                        .track(*p)
                                                                        .value_at(t.source_frame))
                                                            });
                                                        }
                                                    }
                                                }
                                                if row.reset {
                                                    reset_groups.push((params.clone(), false));
                                                }
                                                // The arrows give a source frame, the playhead wants a timeline frame.
                                                if let Some(source_frame) = row.goto {
                                                    pending_playhead = self
                                                        .timeline_id
                                                        .and_then(|tid| {
                                                            self.project.timelines[tid]
                                                                .clip(primary.track_index, primary.clip_id)
                                                        })
                                                        .map(|c| c.timeline_frame_at(source_frame));
                                                }
                                            }

                                            for (params, reset_flip) in reset_groups {
                                                for t in targets {
                                                    pending_effects.push(
                                                        reset_transform_params((t.timeline, t.track_index, t.clip_id), params.clone(), reset_flip),
                                                    );
                                                }
                                            }

                                            // One row per filter, in the order they were
                                            // dragged onto the clip from the Effects
                                            // panel; empty until there is one.
                                            if !info.filters.is_empty() {
                                                ui.add_space(6.0);
                                                ui.label(egui::RichText::new(t!("props.filters")).strong());
                                                for filter in &info.filters {
                                                    let mut enabled = filter.enabled;
                                                    let reset = title_section_header(
                                                        ui,
                                                        &timeline_ui::filter_label(filter.kind),
                                                        &mut enabled,
                                                    );
                                                    if reset {
                                                        enabled = true;
                                                    }
                                                    if reset || enabled != filter.enabled {
                                                        let tl = self.timeline_id.map(|id| &self.project.timelines[id]);
                                                        for t in targets {
                                                            let Some(effects) = target_effects(tl, t) else {
                                                                continue;
                                                            };
                                                            if let Some(pos) =
                                                                effects.filters.iter().position(|f| f.kind == filter.kind)
                                                            {
                                                                let mut new_filters = effects.filters.clone();
                                                                new_filters[pos].enabled = enabled;
                                                                pending_effects.push(set_filters(
                                                                    (t.timeline, t.track_index, t.clip_id),
                                                                    new_filters,
                                                                ));
                                                            }
                                                        }
                                                    }
                                                }
                                            }

                                            // The color applies only to generator
                                            // clips: the other selected video
                                            // clips stay out of it.
                                            if is_solid_color {
                                                let solid: Vec<&PanelTarget> =
                                                    targets.iter().filter(|t| t.is_solid_color).collect();
                                                ui.add_space(6.0);
                                                ui.horizontal(|ui| {
                                                    ui.label(egui::RichText::new(t!("props.color")).strong());
                                                    if keyframe_button(ui, color_kf_here).clicked()
                                                    {
                                                        let tl = self.timeline_id.map(|id| &self.project.timelines[id]);
                                                        for t in &solid {
                                                            let own = target_effects(tl, t)
                                                                .and_then(|e| e.color.as_ref())
                                                                .map_or(color, |k| k.value_at(t.source_frame));
                                                            pending_effects.push(if color_kf_here {
                                                                remove_color_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame)
                                                            } else {
                                                                upsert_color_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame, own)
                                                            });
                                                        }
                                                    }
                                                });
                                                let mut rgba: [f32; 4] = color.into();
                                                if ui
                                                    .color_edit_button_rgba_unmultiplied(&mut rgba)
                                                    .changed()
                                                {
                                                    color = rgba.into();
                                                    let tl = self.timeline_id.map(|id| &self.project.timelines[id]);
                                                    for t in &solid {
                                                        let constant = target_effects(tl, t)
                                                            .is_none_or(|e| e.color.as_ref().is_none_or(|k| k.is_constant()));
                                                        pending_effects.push(if constant {
                                                            set_color_default((t.timeline, t.track_index, t.clip_id), color)
                                                        } else {
                                                            upsert_color_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame, color)
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                        PropertiesTab::Audio => {
                                            let row = param_row(
                                                ui,
                                                &t!("props.volume"),
                                                Some(RowKeyframe {
                                                    on_keyframe: gain_kf_here,
                                                    prev: gain_prev,
                                                    next: gain_next,
                                                }),
                                                |ui| {
                                                    slider_field(
                                                        ui,
                                                        &mut gain,
                                                        vv_core::GAIN_DB_MIN..=vv_core::GAIN_DB_MAX,
                                                        0.2,
                                                        1,
                                                    )
                                                },
                                            );
                                            let tl = self.timeline_id.map(|id| &self.project.timelines[id]);
                                            if row.changed {
                                                for t in targets {
                                                    let constant = target_effects(tl, t)
                                                        .is_none_or(|e| e.gain_db.is_constant());
                                                    pending_effects.push(if constant {
                                                        set_gain_default((t.timeline, t.track_index, t.clip_id), gain)
                                                    } else {
                                                        upsert_gain_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame, gain)
                                                    });
                                                }
                                            }
                                            if row.toggled_keyframe {
                                                for t in targets {
                                                    let Some(effects) = target_effects(tl, t) else {
                                                        continue;
                                                    };
                                                    pending_effects.push(if gain_kf_here {
                                                        remove_gain_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame)
                                                    } else {
                                                        upsert_gain_keyframe((t.timeline, t.track_index, t.clip_id), t.source_frame, effects.gain_db.value_at(t.source_frame))
                                                    });
                                                }
                                            }
                                            if row.reset {
                                                for t in targets {
                                                    pending_effects.push(reset_gain((t.timeline, t.track_index, t.clip_id)));
                                                }
                                            }
                                            if let Some(source_frame) = row.goto {
                                                pending_playhead = self
                                                    .timeline_id
                                                    .and_then(|tid| {
                                                        self.project.timelines[tid]
                                                            .clip(primary.track_index, primary.clip_id)
                                                    })
                                                    .map(|c| c.timeline_frame_at(source_frame));
                                            }
                                        }
                                    }
                                }
                                _ => {
                                    ui.small(if self.properties_tab == PropertiesTab::Audio {
                                        t!("props.no_audio_clip")
                                    } else {
                                        t!("props.no_video_clip")
                                    });
                                }
                            }
                            }
                        } else if let Some(timeline_id) = self.timeline_id {
                            let tl = &self.project.timelines[timeline_id];
                            ui.heading(t!("props.timeline"));
                            ui.label(tl.name.clone());
                            ui.label(format!(
                                "{}x{} · {:.2} fps",
                                tl.resolution.0,
                                tl.resolution.1,
                                tl.fps.as_f64()
                            ));
                            ui.label(t!("props.track_count", count = tl.tracks.len()));
                            let playhead_secs =
                                self.timeline_state.playhead as f64 / tl.fps.as_f64().max(1.0);
                            ui.label(t!(
                                "props.playhead",
                                frame = self.timeline_state.playhead,
                                secs = format!("{playhead_secs:.2}")
                            ));
                            ui.small(t!("props.no_clip"));
                        } else {
                            ui.label(t!("props.import_to_start"));
                        }
                        });
                });
            if let Some(state) = egui::PanelState::load(ui.ctx(), egui::Id::new("properties")) {
                self.settings.panels.inspector_width = state.size().x;
            }
        }
        (pending_effects, pending_playhead)
    }
}
