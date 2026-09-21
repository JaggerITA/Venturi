//! Editor di keyframe: finestra fluttuante con, sotto, una riga per ogni
//! parametro animato della clip selezionata e, sopra, la curva della riga
//! scelta. Disegnato col painter e con hit-test a mano, come la timeline:
//! i punti sono troppi e troppo piccoli per dei widget.
//!
//! I keyframe vivono in frame *sorgente* della clip: tutte le coordinate
//! qui sono in quello spazio, la conversione col playhead sta ai bordi.

use std::collections::HashSet;

use vv_core::{
    Clip, ClipId, EffectStack, FrameIdx, Interpolation, KeyframePick, KeyframeTarget, Project,
    TimelineId, TransformParam,
};

use crate::properties_panel::BoxedCommand;

const LABEL_WIDTH: f32 = 110.0;
const ROW_HEIGHT: f32 = 18.0;
const RULER_HEIGHT: f32 = 18.0;
const CURVE_HEIGHT: f32 = 170.0;
/// Raggio del bersaglio di un keyframe: più largo del punto disegnato,
/// che altrimenti sarebbe quasi impossibile prendere.
const PICK_RADIUS: f32 = 7.0;
const POINT_RADIUS: f32 = 4.0;
const HANDLE_RADIUS: f32 = 3.5;
const PLAYHEAD_COLOR: egui::Color32 = egui::Color32::from_rgb(220, 60, 60);
const CURVE_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 150, 230);

/// Cosa sta trascinando il puntatore.
#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Spostamento nel tempo della selezione. `applied` è il delta già
    /// mandato alla history: ogni frame se ne manda solo la differenza.
    Time { origin_frame: FrameIdx, applied: FrameIdx },
    /// Punto della curva: tempo come sopra, più il valore.
    Point { pick: KeyframePick, origin_frame: FrameIdx, applied: FrameIdx },
    /// Handle di una bezier: `outgoing` distingue quello del keyframe di
    /// partenza da quello del keyframe di arrivo del segmento.
    Handle { pick: KeyframePick, outgoing: bool },
}

#[derive(Debug, Default)]
pub(crate) struct KeyframeEditorState {
    /// La clip di cui si stanno mostrando i keyframe.
    clip: Option<(TimelineId, usize, ClipId)>,
    /// Riga di cui si vede la curva.
    row: Option<KeyframeTarget>,
    selection: HashSet<KeyframePick>,
    drag: Option<Drag>,
    /// Rettangolo di selezione in corso: origine, angolo corrente e se è
    /// partito nella curva (le due aree non se lo devono rubare).
    box_select: Option<(egui::Pos2, egui::Pos2, bool)>,
    /// Porzione di clip visibile (primo frame, durata) quando si è zoomato;
    /// `None` = tutta la clip.
    view: Option<(FrameIdx, FrameIdx)>,
}

impl KeyframeEditorState {
    /// La clip è cambiata sotto i piedi (altra selezione, undo): quel che
    /// era selezionato non esiste più.
    fn reset_for(&mut self, clip: (TimelineId, usize, ClipId)) {
        if self.clip != Some(clip) {
            self.clip = Some(clip);
            self.row = None;
            self.selection.clear();
            self.drag = None;
            self.box_select = None;
            self.view = None;
        }
    }
}

/// Le righe da mostrare: i parametri con almeno un keyframe.
fn rows(effects: &EffectStack) -> Vec<KeyframeTarget> {
    let mut rows: Vec<KeyframeTarget> = TransformParam::ALL
        .iter()
        .filter(|p| !effects.transform.track(**p).is_constant())
        .map(|p| KeyframeTarget::TransformParam(*p))
        .collect();
    if !effects.gain_db.is_constant() {
        rows.push(KeyframeTarget::Gain);
    }
    if effects.color.as_ref().is_some_and(|c| !c.is_constant()) {
        rows.push(KeyframeTarget::Color);
    }
    rows
}

/// I frame dei keyframe di una riga.
fn frames_of(effects: &EffectStack, target: KeyframeTarget) -> Vec<FrameIdx> {
    match target {
        KeyframeTarget::TransformParam(p) => {
            effects.transform.track(p).keyframes().iter().map(|k| k.0).collect()
        }
        KeyframeTarget::Gain => effects.gain_db.keyframes().iter().map(|k| k.0).collect(),
        KeyframeTarget::Color => effects
            .color
            .as_ref()
            .map(|c| c.keyframes().iter().map(|k| k.0).collect())
            .unwrap_or_default(),
    }
}

/// Valore e interpolazione di un keyframe scalare; `None` per il colore,
/// che non ha una curva da disegnare.
fn scalar_at(
    effects: &EffectStack,
    target: KeyframeTarget,
    frame: FrameIdx,
) -> Option<(f32, Interpolation)> {
    match target {
        KeyframeTarget::TransformParam(p) => effects.transform.track(p).keyframe_at(frame),
        KeyframeTarget::Gain => effects.gain_db.keyframe_at(frame),
        KeyframeTarget::Color => None,
    }
}

fn scalar_value_at(effects: &EffectStack, target: KeyframeTarget, frame: FrameIdx) -> Option<f32> {
    match target {
        KeyframeTarget::TransformParam(p) => Some(effects.transform.track(p).value_at(frame)),
        KeyframeTarget::Gain => Some(effects.gain_db.value_at(frame)),
        KeyframeTarget::Color => None,
    }
}

fn target_label(target: KeyframeTarget) -> String {
    use TransformParam as P;
    match target {
        KeyframeTarget::TransformParam(p) => match p {
            P::ZoomX => format!("{} X", t!("props.zoom")),
            P::ZoomY => format!("{} Y", t!("props.zoom")),
            P::PositionX => format!("{} X", t!("props.position")),
            P::PositionY => format!("{} Y", t!("props.position")),
            P::Rotation => t!("props.rotation").to_string(),
            P::AnchorX => format!("{} X", t!("props.anchor")),
            P::AnchorY => format!("{} Y", t!("props.anchor")),
            P::CropLeft => t!("props.crop_left").to_string(),
            P::CropTop => t!("props.crop_top").to_string(),
            P::CropRight => t!("props.crop_right").to_string(),
            P::CropBottom => t!("props.crop_bottom").to_string(),
            P::CropSoftness => t!("props.softness").to_string(),
            P::Opacity => t!("props.opacity").to_string(),
        },
        KeyframeTarget::Gain => t!("keyframes.gain").to_string(),
        KeyframeTarget::Color => t!("keyframes.color").to_string(),
    }
}

fn preset_label(interpolation: Interpolation) -> String {
    match interpolation {
        Interpolation::Hold => t!("keyframes.hold"),
        Interpolation::Linear => t!("keyframes.linear"),
        Interpolation::EaseInOut => t!("keyframes.ease_in_out"),
        Interpolation::EaseIn => t!("keyframes.ease_in"),
        Interpolation::EaseOut => t!("keyframes.ease_out"),
        Interpolation::Bezier { .. } => t!("keyframes.bezier"),
    }
    .to_string()
}

/// Mappa fra frame sorgente della clip e x sullo schermo.
#[derive(Clone, Copy)]
struct TimeAxis {
    left: f32,
    width: f32,
    first: FrameIdx,
    span: FrameIdx,
}

impl TimeAxis {
    /// `view` è la porzione di clip visibile: primo frame e durata.
    fn new(rect: egui::Rect, view: (FrameIdx, FrameIdx)) -> Self {
        Self {
            left: rect.left(),
            width: rect.width().max(1.0),
            first: view.0,
            span: view.1.max(1),
        }
    }

    /// Quanti frame vale uno spostamento orizzontale di `dx` pixel.
    fn delta(&self, dx: f32) -> FrameIdx {
        (dx / self.width * self.span as f32).round() as FrameIdx
    }

    fn x(&self, frame: FrameIdx) -> f32 {
        self.left + (frame - self.first) as f32 / self.span as f32 * self.width
    }

    fn frame(&self, x: f32) -> FrameIdx {
        self.first + ((x - self.left) / self.width * self.span as f32).round() as FrameIdx
    }
}

pub(crate) struct KeyframeEditorResponse {
    pub(crate) commands: Vec<BoxedCommand>,
    /// Frame di timeline a cui portare la testina.
    pub(crate) playhead: Option<FrameIdx>,
}

/// Disegna la finestra. `target` è la clip da mostrare (la prima
/// selezionata) e `playhead` il frame di timeline corrente.
#[allow(clippy::too_many_arguments)]
pub(crate) fn show_keyframe_editor(
    ctx: &egui::Context,
    open: &mut bool,
    state: &mut KeyframeEditorState,
    project: &Project,
    target: Option<(TimelineId, usize, ClipId)>,
    playhead: FrameIdx,
    // Zoom bloccato in proporzioni nell'inspector: qui X e Y si muovono
    // insieme, altrimenti l'editor romperebbe il vincolo.
    zoom_link: bool,
) -> KeyframeEditorResponse {
    let mut response = KeyframeEditorResponse { commands: Vec::new(), playhead: None };
    let mut window_open = *open;
    egui::Window::new(t!("keyframes.title"))
        .id(egui::Id::new("keyframe_editor"))
        .open(&mut window_open)
        .default_size([720.0, 340.0])
        .min_width(420.0)
        .show(ctx, |ui| {
            let clip = target.and_then(|(tl, track, id)| {
                project.timelines.get(tl).and_then(|t| t.clip(track, id))
            });
            let (Some((timeline, track_index, clip_id)), Some(clip)) = (target, clip) else {
                ui.label(t!("keyframes.no_clip"));
                return;
            };
            state.reset_for((timeline, track_index, clip_id));
            let rows = rows(&clip.effects);
            if rows.is_empty() {
                ui.label(t!("keyframes.no_keyframes"));
                return;
            }
            // Una riga sparita (undo, keyframe tolti) non deve restare aperta.
            if !state.row.is_some_and(|r| rows.contains(&r)) {
                state.row = Some(rows[0]);
            }
            state.selection.retain(|(t, f)| {
                rows.contains(t) && frames_of(&clip.effects, *t).contains(f)
            });

            show_toolbar(ui, state, zoom_link, (timeline, track_index, clip_id), &mut response);
            ui.separator();

            let axis_rect = |rect: egui::Rect| rect.with_min_x(rect.left() + LABEL_WIDTH);
            let head = clip.source_frame_at(playhead);
            let full = (
                clip.source_in(),
                (clip.source_out() - clip.source_in()).max(1),
            );
            handle_zoom(ui, state, axis_rect(ui.available_rect_before_wrap()), full);
            let view = state.view.unwrap_or(full);

            if let Some(row) = state.row {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), CURVE_HEIGHT),
                    egui::Sense::hover(),
                );
                let axis = TimeAxis::new(axis_rect(rect), view);
                draw_curve(
                    ui,
                    rect,
                    axis,
                    state,
                    clip,
                    (timeline, track_index, clip_id),
                    row,
                    head,
                    zoom_link,
                    &mut response,
                );
            }

            let ruler = ui
                .allocate_exact_size(
                    egui::vec2(ui.available_width(), RULER_HEIGHT),
                    egui::Sense::hover(),
                )
                .0;
            let axis = TimeAxis::new(axis_rect(ruler), view);
            draw_ruler(ui, ruler, axis, clip, project, timeline, head);
            let ruler_response =
                ui.interact(axis_rect(ruler), ui.id().with("kf_ruler"), egui::Sense::click_and_drag());
            if let Some(pos) = ruler_response.interact_pointer_pos() {
                response.playhead = Some(clip.timeline_frame_at(
                    axis.frame(pos.x).clamp(clip.source_in(), clip.source_out()),
                ));
            }

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                draw_rows(
                    ui,
                    state,
                    clip,
                    (timeline, track_index, clip_id),
                    &rows,
                    view,
                    head,
                    zoom_link,
                    &mut response,
                );
            });
        });
    *open = window_open;
    response
}

/// Alt+scroll (o pinch) zooma attorno al puntatore come sulla timeline;
/// lo scroll orizzontale fa scorrere la porzione visibile.
fn handle_zoom(
    ui: &egui::Ui,
    state: &mut KeyframeEditorState,
    rect: egui::Rect,
    full: (FrameIdx, FrameIdx),
) {
    let Some(pos) = ui.ctx().pointer_hover_pos().filter(|p| rect.contains(*p)) else {
        return;
    };
    let (zoom, pan) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta.x));
    if zoom == 1.0 && pan == 0.0 {
        return;
    }
    let axis = TimeAxis::new(rect, state.view.unwrap_or(full));
    let anchor = axis.frame(pos.x);
    let span = ((axis.span as f32 / zoom).round() as FrameIdx).clamp(2, full.1);
    // Il frame sotto il puntatore resta dov'è.
    let first = anchor
        - ((anchor - axis.first) as f32 * span as f32 / axis.span as f32).round() as FrameIdx
        - axis.delta(pan);
    state.view = (span < full.1).then(|| {
        (first.clamp(full.0, full.0 + full.1 - span), span)
    });
}

/// Con lo zoom bloccato in proporzioni X e Y sono gemelli: quel che si fa
/// al keyframe di uno va fatto a quello dell'altro allo stesso frame.
fn with_zoom_link(picks: Vec<KeyframePick>, zoom_link: bool) -> Vec<KeyframePick> {
    if !zoom_link {
        return picks;
    }
    let mut all = picks.clone();
    for (target, frame) in picks {
        let KeyframeTarget::TransformParam(param) = target else {
            continue;
        };
        let twin = match param {
            TransformParam::ZoomX => TransformParam::ZoomY,
            TransformParam::ZoomY => TransformParam::ZoomX,
            _ => continue,
        };
        let pick = (KeyframeTarget::TransformParam(twin), frame);
        if !all.contains(&pick) {
            all.push(pick);
        }
    }
    all
}

fn show_toolbar(
    ui: &mut egui::Ui,
    state: &mut KeyframeEditorState,
    zoom_link: bool,
    clip: (TimelineId, usize, ClipId),
    response: &mut KeyframeEditorResponse,
) {
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!state.selection.is_empty(), |ui| {
            for preset in Interpolation::PRESETS {
                if ui.button(preset_label(preset)).clicked() {
                    let picks =
                        with_zoom_link(state.selection.iter().copied().collect(), zoom_link);
                    response.commands.push(Box::new(vv_core::SetKeyframeInterpolation::new(
                        clip.0, clip.1, clip.2, picks, preset,
                    )));
                }
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(t!("keyframes.selection_hint"));
        });
    });
}

/// Tacche col timecode del frame di *timeline* corrispondente: è quello
/// che si legge sulla barra di riproduzione.
fn draw_ruler(
    ui: &egui::Ui,
    rect: egui::Rect,
    axis: TimeAxis,
    clip: &Clip,
    project: &Project,
    timeline: TimelineId,
    head: FrameIdx,
) {
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);
    let fps = project.timelines[timeline].fps.as_f64().max(1.0);
    // Una tacca ogni ~90 px, arrotondata a un numero tondo di frame.
    let per_tick = ((axis.span as f32 * 90.0 / axis.width.max(1.0)).ceil() as FrameIdx).max(1);
    let step = round_step(per_tick);
    let mut frame = axis.first - axis.first.rem_euclid(step);
    while frame <= axis.first + axis.span {
        if frame >= axis.first {
            let x = axis.x(frame);
            painter.line_segment(
                [egui::pos2(x, rect.bottom() - 5.0), egui::pos2(x, rect.bottom())],
                egui::Stroke::new(1.0, visuals.weak_text_color()),
            );
            painter.text(
                egui::pos2(x + 3.0, rect.top()),
                egui::Align2::LEFT_TOP,
                crate::timeline_ui::format_timecode(clip.timeline_frame_at(frame) as f64 / fps, fps),
                egui::FontId::proportional(10.0),
                visuals.weak_text_color(),
            );
        }
        frame += step;
    }
    let x = axis.x(head);
    painter.line_segment(
        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
        egui::Stroke::new(1.0, PLAYHEAD_COLOR),
    );
}

/// Passo "tondo" (1, 2, 5, 10, 20, 50, …) più vicino per eccesso.
fn round_step(minimum: FrameIdx) -> FrameIdx {
    let mut step = 1;
    while step < minimum {
        let digits = [1, 2, 5];
        let next = digits.iter().map(|d| d * step).find(|s| *s > step && *s >= minimum);
        step = next.unwrap_or(step * 10);
    }
    step
}

#[allow(clippy::too_many_arguments)]
fn draw_curve(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    axis: TimeAxis,
    state: &mut KeyframeEditorState,
    clip: &Clip,
    clip_ref: (TimelineId, usize, ClipId),
    row: KeyframeTarget,
    head: FrameIdx,
    zoom_link: bool,
    response: &mut KeyframeEditorResponse,
) {
    let plot = rect.with_min_x(axis.left);
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals().clone();
    painter.rect_filled(plot, 0.0, visuals.extreme_bg_color);

    let frames = frames_of(&clip.effects, row);
    if scalar_value_at(&clip.effects, row, axis.first).is_none() {
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            t!("keyframes.no_curve"),
            egui::FontId::proportional(12.0),
            visuals.weak_text_color(),
        );
        return;
    }

    let values: Vec<f32> = frames
        .iter()
        .filter_map(|f| scalar_value_at(&clip.effects, row, *f))
        .collect();
    let (mut low, mut high) = values
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    if (high - low).abs() < 1e-6 {
        low -= 1.0;
        high += 1.0;
    }
    let margin = (high - low) * 0.15;
    let (low, high) = (low - margin, high + margin);
    let y = |v: f32| plot.bottom() - (v - low) / (high - low) * plot.height();
    let value_at_y = |py: f32| low + (plot.bottom() - py) / plot.height() * (high - low);

    // Griglia e scala dei valori nella colonna di sinistra.
    for i in 0..=4 {
        let value = low + (high - low) * i as f32 / 4.0;
        let py = y(value);
        painter.line_segment(
            [egui::pos2(plot.left(), py), egui::pos2(plot.right(), py)],
            egui::Stroke::new(1.0, visuals.faint_bg_color),
        );
        painter.text(
            egui::pos2(plot.left() - 6.0, py),
            egui::Align2::RIGHT_CENTER,
            format!("{value:.2}"),
            egui::FontId::proportional(10.0),
            visuals.weak_text_color(),
        );
    }

    // La curva campionata com'è valutata davvero, interpolazione compresa.
    let mut points = Vec::new();
    let mut x = plot.left();
    while x <= plot.right() {
        let frame = axis.frame(x);
        if let Some(v) = scalar_value_at(&clip.effects, row, frame) {
            points.push(egui::pos2(x, y(v)));
        }
        x += 2.0;
    }
    painter.add(egui::Shape::line(points, egui::Stroke::new(1.5, CURVE_COLOR)));

    let head_x = axis.x(head);
    painter.line_segment(
        [egui::pos2(head_x, plot.top()), egui::pos2(head_x, plot.bottom())],
        egui::Stroke::new(1.0, PLAYHEAD_COLOR),
    );

    let point_pos = |frame: FrameIdx| {
        scalar_value_at(&clip.effects, row, frame).map(|v| egui::pos2(axis.x(frame), y(v)))
    };

    // Segmenti su cui mostrare gli handle: quello che esce da un keyframe
    // selezionato e quello che ci entra, così anche un solo keyframe
    // selezionato ne ha uno addosso.
    let mut segments: Vec<usize> = Vec::new();
    for (i, &frame) in frames.iter().enumerate() {
        if !state.selection.contains(&(row, frame)) {
            continue;
        }
        if i + 1 < frames.len() {
            segments.push(i);
        }
        if i > 0 {
            segments.push(i - 1);
        }
    }
    segments.sort_unstable();
    segments.dedup();

    let mut handles: Vec<(KeyframePick, bool, egui::Pos2)> = Vec::new();
    for i in segments {
        let (frame, next) = (frames[i], frames[i + 1]);
        let (Some((_, interp)), Some(from), Some(to)) = (
            scalar_at(&clip.effects, row, frame),
            point_pos(frame),
            point_pos(next),
        ) else {
            continue;
        };
        let Some((c1, c2)) = interp.control_points() else {
            continue;
        };
        let control = |c: [f32; 2]| {
            egui::pos2(from.x + (to.x - from.x) * c[0], from.y + (to.y - from.y) * c[1])
        };
        for (outgoing, pos) in [(true, control(c1)), (false, control(c2))] {
            let anchor = if outgoing { from } else { to };
            painter.line_segment([anchor, pos], egui::Stroke::new(1.0, visuals.weak_text_color()));
            painter.circle_filled(pos, HANDLE_RADIUS, visuals.weak_text_color());
            handles.push(((row, frame), outgoing, pos));
        }
    }

    for &frame in &frames {
        let Some(pos) = point_pos(frame) else { continue };
        let selected = state.selection.contains(&(row, frame));
        painter.circle(
            pos,
            POINT_RADIUS,
            if selected { PLAYHEAD_COLOR } else { visuals.extreme_bg_color },
            egui::Stroke::new(1.5, if selected { PLAYHEAD_COLOR } else { CURVE_COLOR }),
        );
    }

    let interaction = ui.interact(plot, ui.id().with("kf_curve"), egui::Sense::click_and_drag());
    let additive = ui.input(|i| i.modifiers.shift || i.modifiers.command);

    if interaction.drag_started() && let Some(pos) = interaction.interact_pointer_pos() {
        let handle = handles
            .iter()
            .find(|(_, _, p)| p.distance(pos) <= PICK_RADIUS)
            .map(|(pick, outgoing, _)| (*pick, *outgoing));
        let point = frames
            .iter()
            .filter_map(|f| point_pos(*f).map(|p| (*f, p)))
            .filter(|(_, p)| p.distance(pos) <= PICK_RADIUS)
            .min_by(|a, b| a.1.distance(pos).total_cmp(&b.1.distance(pos)))
            .map(|(f, _)| f);
        state.drag = match (handle, point) {
            (Some((pick, outgoing)), _) => Some(Drag::Handle { pick, outgoing }),
            (None, Some(frame)) => {
                if !additive && !state.selection.contains(&(row, frame)) {
                    state.selection.clear();
                }
                state.selection.insert((row, frame));
                Some(Drag::Point {
                    pick: (row, frame),
                    origin_frame: axis.frame(pos.x),
                    applied: 0,
                })
            }
            (None, None) => {
                if !additive {
                    state.selection.clear();
                }
                state.box_select = Some((pos, pos, true));
                None
            }
        };
    }

    if interaction.dragged() && let Some(pos) = interaction.interact_pointer_pos() {
        match state.drag {
            Some(Drag::Handle { pick, outgoing }) => {
                let next = frames.iter().find(|f| **f > pick.1).copied();
                if let (Some(next), Some((_, interp))) =
                    (next, scalar_at(&clip.effects, row, pick.1))
                    && let (Some(from), Some(to)) = (point_pos(pick.1), point_pos(next))
                    && let Some((c1, c2)) = interp.control_points()
                {
                    let span_x = to.x - from.x;
                    let (v0, v1) = (value_at_y(from.y), value_at_y(to.y));
                    let nx = if span_x.abs() > 1e-3 {
                        ((pos.x - from.x) / span_x).clamp(0.0, 1.0)
                    } else {
                        0.5
                    };
                    // Su un segmento piatto la y normalizzata non esiste:
                    // l'handle resta dov'è e si muove solo in orizzontale.
                    let ny = if (v1 - v0).abs() > 1e-6 {
                        (value_at_y(pos.y) - v0) / (v1 - v0)
                    } else if outgoing {
                        c1[1]
                    } else {
                        c2[1]
                    };
                    let (c1, c2) = if outgoing { ([nx, ny], c2) } else { (c1, [nx, ny]) };
                    response.commands.push(Box::new(vv_core::SetKeyframeInterpolation::new(
                        clip_ref.0,
                        clip_ref.1,
                        clip_ref.2,
                        with_zoom_link(vec![pick], zoom_link),
                        Interpolation::Bezier { c1, c2 },
                    )));
                }
            }
            Some(Drag::Point { pick, origin_frame, applied }) => {
                let wanted = axis.frame(pos.x) - origin_frame;
                let step = wanted - applied;
                let (tl, track, id) = clip_ref;
                if step != 0 {
                    let picks =
                        with_zoom_link(state.selection.iter().copied().collect(), zoom_link);
                    response.commands.push(Box::new(vv_core::MoveKeyframes::new(
                        tl, track, id, picks, step,
                    )));
                    state.selection = state
                        .selection
                        .iter()
                        .map(|(t, f)| (*t, f + step))
                        .collect();
                    state.drag = Some(Drag::Point {
                        pick: (pick.0, pick.1 + step),
                        origin_frame,
                        applied: wanted,
                    });
                }
                // Il valore segue il puntatore sul keyframe trascinato,
                // non su tutta la selezione: muoverli tutti in verticale
                // vorrebbe dire scale diverse per parametri diversi.
                let frame = pick.1 + step;
                // `clip` è ancora quello di prima dei comandi di questo
                // frame: l'interpolazione si legge alla vecchia posizione.
                if let Some((_, interp)) = scalar_at(&clip.effects, row, pick.1) {
                    let value = value_at_y(pos.y);
                    // Anche il valore va replicato sul gemello, o lo zoom
                    // bloccato resterebbe tale solo di nome.
                    for (target, _) in with_zoom_link(vec![(row, pick.1)], zoom_link) {
                        if scalar_at(&clip.effects, target, pick.1).is_none() {
                            continue;
                        }
                        response.commands.push(Box::new(vv_core::UpsertKeyframe::new(
                            tl,
                            track,
                            id,
                            frame,
                            keyframe_value(target, value),
                            interp,
                        )));
                    }
                }
            }
            _ => {}
        }
        if let Some((origin, _, true)) = state.box_select {
            state.box_select = Some((origin, pos, true));
        }
    }

    if let Some((origin, current, true)) = state.box_select {
        let rect = egui::Rect::from_two_pos(origin, current);
        painter.rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, visuals.selection.bg_fill),
            egui::StrokeKind::Inside,
        );
        if interaction.drag_stopped() {
            for frame in &frames {
                if point_pos(*frame).is_some_and(|p| rect.contains(p)) {
                    state.selection.insert((row, *frame));
                }
            }
            state.box_select = None;
        }
    }
    if interaction.drag_stopped() {
        state.drag = None;
    }
}

fn keyframe_value(target: KeyframeTarget, value: f32) -> vv_core::KeyframeValue {
    match target {
        KeyframeTarget::TransformParam(p) => vv_core::KeyframeValue::TransformParam(p, value),
        KeyframeTarget::Gain => vv_core::KeyframeValue::Gain(value),
        // Senza curva non si arriva qui; il colore si modifica dall'inspector.
        KeyframeTarget::Color => vv_core::KeyframeValue::Gain(value),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rows(
    ui: &mut egui::Ui,
    state: &mut KeyframeEditorState,
    clip: &Clip,
    clip_ref: (TimelineId, usize, ClipId),
    rows: &[KeyframeTarget],
    view: (FrameIdx, FrameIdx),
    head: FrameIdx,
    zoom_link: bool,
    response: &mut KeyframeEditorResponse,
) {
    // Anche lo spazio che avanza sotto l'ultima riga fa parte dell'area:
    // ci si deve poter cominciare un riquadro di selezione.
    let height = rows.len() as f32 * ROW_HEIGHT;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height.max(ui.available_height())),
        egui::Sense::hover(),
    );
    let track_rect = rect.with_min_x(rect.left() + LABEL_WIDTH);
    let axis = TimeAxis::new(track_rect, view);
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals().clone();

    let row_rect = |i: usize| {
        egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + i as f32 * ROW_HEIGHT),
            egui::vec2(rect.width(), ROW_HEIGHT),
        )
    };
    let diamond = |pos: egui::Pos2, selected: bool| {
        let color = if selected { PLAYHEAD_COLOR } else { visuals.weak_text_color() };
        egui::Shape::convex_polygon(
            vec![
                egui::pos2(pos.x, pos.y - 5.0),
                egui::pos2(pos.x + 4.0, pos.y),
                egui::pos2(pos.x, pos.y + 5.0),
                egui::pos2(pos.x - 4.0, pos.y),
            ],
            color,
            egui::Stroke::NONE,
        )
    };

    for (i, &row) in rows.iter().enumerate() {
        let r = row_rect(i);
        if state.row == Some(row) {
            painter.rect_filled(r, 0.0, visuals.faint_bg_color);
        }
        painter.text(
            egui::pos2(r.left() + LABEL_WIDTH - 6.0, r.center().y),
            egui::Align2::RIGHT_CENTER,
            target_label(row),
            egui::FontId::proportional(11.0),
            if state.row == Some(row) {
                visuals.strong_text_color()
            } else {
                visuals.text_color()
            },
        );
        let strip = r.with_min_x(track_rect.left());
        painter.line_segment(
            [
                egui::pos2(strip.left(), strip.center().y),
                egui::pos2(strip.right(), strip.center().y),
            ],
            egui::Stroke::new(1.0, visuals.faint_bg_color),
        );
        for frame in frames_of(&clip.effects, row) {
            let pos = egui::pos2(axis.x(frame), strip.center().y);
            painter.add(diamond(pos, state.selection.contains(&(row, frame))));
        }
    }

    let head_x = axis.x(head);
    painter.line_segment(
        [egui::pos2(head_x, rect.top()), egui::pos2(head_x, rect.bottom())],
        egui::Stroke::new(1.0, PLAYHEAD_COLOR),
    );

    let interaction = ui.interact(rect, ui.id().with("kf_rows"), egui::Sense::click_and_drag());
    let additive = ui.input(|i| i.modifiers.shift || i.modifiers.command);
    let row_at = |pos: egui::Pos2| {
        let i = ((pos.y - rect.top()) / ROW_HEIGHT).floor() as isize;
        (i >= 0).then(|| rows.get(i as usize).copied()).flatten()
    };
    let hit = |pos: egui::Pos2| -> Option<KeyframePick> {
        let row = row_at(pos)?;
        frames_of(&clip.effects, row)
            .into_iter()
            .map(|f| (f, (axis.x(f) - pos.x).abs()))
            .filter(|(_, d)| *d <= PICK_RADIUS)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(f, _)| (row, f))
    };

    if interaction.drag_started() && let Some(pos) = interaction.interact_pointer_pos() {
        if pos.x < track_rect.left() && let Some(row) = row_at(pos) {
            state.row = Some(row);
        } else if let Some(pick) = hit(pos) {
            state.row = Some(pick.0);
            if !additive && !state.selection.contains(&pick) {
                state.selection.clear();
            }
            state.selection.insert(pick);
            state.drag = Some(Drag::Time { origin_frame: axis.frame(pos.x), applied: 0 });
        } else {
            if !additive {
                state.selection.clear();
            }
            state.box_select = Some((pos, pos, false));
        }
    }

    if interaction.dragged()
        && let Some(pos) = interaction.interact_pointer_pos()
        && let Some(Drag::Time { origin_frame, applied }) = state.drag
    {
        let wanted = axis.frame(pos.x) - origin_frame;
        let step = wanted - applied;
        if step != 0 {
            let picks = with_zoom_link(state.selection.iter().copied().collect(), zoom_link);
            response.commands.push(Box::new(vv_core::MoveKeyframes::new(
                clip_ref.0, clip_ref.1, clip_ref.2, picks, step,
            )));
            state.selection = state.selection.iter().map(|(t, f)| (*t, f + step)).collect();
            state.drag = Some(Drag::Time { origin_frame, applied: wanted });
        }
    }

    if let Some((origin, current, false)) = state.box_select {
        let current = interaction.interact_pointer_pos().unwrap_or(current);
        state.box_select = Some((origin, current, false));
        let select = egui::Rect::from_two_pos(origin, current);
        painter.rect_stroke(
            select,
            0.0,
            egui::Stroke::new(1.0, visuals.selection.bg_fill),
            egui::StrokeKind::Inside,
        );
        if interaction.drag_stopped() {
            for (i, &row) in rows.iter().enumerate() {
                let y = row_rect(i).center().y;
                for frame in frames_of(&clip.effects, row) {
                    if select.contains(egui::pos2(axis.x(frame), y)) {
                        state.selection.insert((row, frame));
                    }
                }
            }
            state.box_select = None;
        }
    }

    if interaction.clicked() && let Some(pos) = interaction.interact_pointer_pos() {
        if pos.x < track_rect.left() && let Some(row) = row_at(pos) {
            state.row = Some(row);
        } else if let Some(pick) = hit(pos) {
            state.row = Some(pick.0);
            if !additive {
                state.selection.clear();
            }
            state.selection.insert(pick);
        }
    }

    if interaction.drag_stopped() {
        state.drag = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoom(param: TransformParam, frame: FrameIdx) -> KeyframePick {
        (KeyframeTarget::TransformParam(param), frame)
    }

    #[test]
    fn zoom_link_adds_the_twin_axis_once_and_only_for_zoom() {
        let picks = vec![zoom(TransformParam::ZoomX, 5), (KeyframeTarget::Gain, 5)];
        let linked = with_zoom_link(picks.clone(), true);
        assert!(linked.contains(&zoom(TransformParam::ZoomY, 5)));
        assert_eq!(linked.len(), 3, "il gain non ha gemelli");

        let both = with_zoom_link(
            vec![zoom(TransformParam::ZoomX, 5), zoom(TransformParam::ZoomY, 5)],
            true,
        );
        assert_eq!(both.len(), 2, "nessun doppione se sono già selezionati entrambi");

        assert_eq!(with_zoom_link(picks.clone(), false), picks);
    }

    #[test]
    fn the_time_axis_maps_the_visible_window_onto_the_rect() {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 0.0), egui::vec2(200.0, 10.0));
        let axis = TimeAxis::new(rect, (50, 100));
        assert_eq!(axis.x(50), 100.0);
        assert_eq!(axis.x(150), 300.0);
        assert_eq!(axis.frame(200.0), 100);
        assert_eq!(axis.delta(-20.0), -10);
    }
}
