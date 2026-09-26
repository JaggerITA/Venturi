//! "Paste attributes" (Alt+V): copies onto the selected clips only the
//! attributes chosen in the dialog, taken from the clips in the clipboard.

use super::*;
use std::collections::HashSet;
use vv_core::{Clip, ClipAttributes, FrameIdx, Keyframed, TransformParam};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Attribute {
    Fades,
    BlendMode,
    Opacity,
    Position,
    Rotation,
    AnchorPoint,
    Zoom,
    Crop,
    CropSoftness,
    Flip,
    Filters,
    Transitions,
    Volume,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttributeGroup {
    Clip,
    Video,
    Audio,
}

impl Attribute {
    const ALL: [Attribute; 13] = [
        Attribute::Fades,
        Attribute::BlendMode,
        Attribute::Opacity,
        Attribute::Position,
        Attribute::Rotation,
        Attribute::AnchorPoint,
        Attribute::Zoom,
        Attribute::Crop,
        Attribute::CropSoftness,
        Attribute::Flip,
        Attribute::Filters,
        Attribute::Transitions,
        Attribute::Volume,
    ];

    fn group(self) -> AttributeGroup {
        match self {
            Attribute::Fades => AttributeGroup::Clip,
            Attribute::Volume => AttributeGroup::Audio,
            _ => AttributeGroup::Video,
        }
    }

    fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Attribute::Fades => t!("paste_attr.fades"),
            Attribute::BlendMode => t!("paste_attr.blend_mode"),
            Attribute::Opacity => t!("paste_attr.opacity"),
            Attribute::Position => t!("paste_attr.position"),
            Attribute::Rotation => t!("paste_attr.rotation"),
            Attribute::AnchorPoint => t!("paste_attr.anchor_point"),
            Attribute::Zoom => t!("paste_attr.zoom"),
            Attribute::Crop => t!("paste_attr.crop"),
            Attribute::CropSoftness => t!("paste_attr.crop_softness"),
            Attribute::Flip => t!("paste_attr.flip"),
            Attribute::Filters => t!("paste_attr.filters"),
            Attribute::Transitions => t!("paste_attr.transitions"),
            Attribute::Volume => t!("paste_attr.volume"),
        }
    }

    /// Transform tracks the attribute is made of; empty for the ones that
    /// are not a transform parameter.
    fn transform_params(self) -> &'static [TransformParam] {
        match self {
            Attribute::Opacity => &[TransformParam::Opacity],
            Attribute::Position => &[TransformParam::PositionX, TransformParam::PositionY],
            Attribute::Rotation => &[TransformParam::Rotation],
            Attribute::AnchorPoint => &[TransformParam::AnchorX, TransformParam::AnchorY],
            Attribute::Zoom => &[TransformParam::ZoomX, TransformParam::ZoomY],
            Attribute::Crop => &[
                TransformParam::CropLeft,
                TransformParam::CropTop,
                TransformParam::CropRight,
                TransformParam::CropBottom,
            ],
            Attribute::CropSoftness => &[TransformParam::CropSoftness],
            _ => &[],
        }
    }
}

/// What to do with the keyframes of an attribute when source and
/// destination clips have different durations.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyframeMode {
    MaintainTiming,
    StretchToFit,
}

pub(crate) struct PasteAttributesDialog {
    /// Name shown as the origin of the attributes.
    source_name: String,
    /// Clips whose attributes are read, one per track kind: a copied
    /// A/V group pastes the video attributes onto the video clips and the
    /// audio ones onto the audio clips.
    source_video: Option<Clip>,
    source_audio: Option<Clip>,
    targets: Vec<(usize, ClipId)>,
    video_targets: usize,
    audio_targets: usize,
    selected: HashSet<Attribute>,
    keyframe_mode: KeyframeMode,
}

impl PasteAttributesDialog {
    fn has_source_for(&self, kind: TrackKind) -> bool {
        match kind {
            TrackKind::Video => self.source_video.is_some(),
            TrackKind::Audio => self.source_audio.is_some(),
        }
    }

    /// A group is shown only if some target can receive it.
    fn group_applies(&self, group: AttributeGroup) -> bool {
        match group {
            AttributeGroup::Clip => !self.targets.is_empty(),
            AttributeGroup::Video => {
                self.video_targets > 0 && self.has_source_for(TrackKind::Video)
            }
            AttributeGroup::Audio => {
                self.audio_targets > 0 && self.has_source_for(TrackKind::Audio)
            }
        }
    }
}

impl VenturiApp {
    /// Opens the dialog on the current selection; does nothing if there is
    /// nothing to copy from or onto.
    pub(crate) fn open_paste_attributes_dialog(&mut self) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        if self.timeline_state.clipboard.is_empty() || self.timeline_state.selected.is_empty() {
            return;
        }
        let tl = &self.project.timelines[timeline_id];
        let source_of = |kind: TrackKind| {
            self.timeline_state
                .clipboard
                .iter()
                .find(|e| e.track_kind == kind)
                .map(|e| e.clip.clone())
        };
        let source_video = source_of(TrackKind::Video);
        let source_audio = source_of(TrackKind::Audio);
        let source_name = source_video
            .as_ref()
            .or(source_audio.as_ref())
            .map(|clip| self.clip_display_name(clip))
            .unwrap_or_default();

        let targets: Vec<(usize, ClipId)> = self
            .timeline_state
            .selected
            .iter()
            .copied()
            .filter(|&(track_index, clip_id)| {
                !tl.is_locked(track_index) && tl.clip(track_index, clip_id).is_some()
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let count_of = |kind: TrackKind| {
            targets
                .iter()
                .filter(|&&(track_index, _)| tl.tracks[track_index].kind == kind)
                .count()
        };
        let (video_targets, audio_targets) =
            (count_of(TrackKind::Video), count_of(TrackKind::Audio));

        self.paste_attributes = Some(PasteAttributesDialog {
            source_name,
            source_video,
            source_audio,
            targets,
            video_targets,
            audio_targets,
            selected: self.paste_attributes_selection.clone(),
            keyframe_mode: self.paste_attributes_keyframe_mode,
        });
    }

    fn clip_display_name(&self, clip: &Clip) -> String {
        match &clip.source {
            vv_core::ClipSource::Media(media_id) => self
                .project
                .media_pool
                .get(*media_id)
                .map(|item| {
                    item.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| item.path.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
            vv_core::ClipSource::SolidColor => t!("generator.solid_color").into_owned(),
            vv_core::ClipSource::Text => t!("generator.text").into_owned(),
        }
    }

    pub(crate) fn show_paste_attributes_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.paste_attributes else {
            return;
        };
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new(t!("paste_attr.title"))
            .id(egui::Id::new("paste_attributes_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                egui::Grid::new("paste_attributes_header")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.label(t!("paste_attr.from"));
                        ui.label(&dialog.source_name);
                        ui.end_row();
                        ui.label(t!("paste_attr.to"));
                        ui.label(t!(
                            "paste_attr.target_count",
                            video = dialog.video_targets,
                            audio = dialog.audio_targets
                        ));
                        ui.end_row();
                    });
                ui.separator();

                ui.label(egui::RichText::new(t!("paste_attr.keyframes")).strong());
                ui.horizontal(|ui| {
                    ui.radio_value(
                        &mut dialog.keyframe_mode,
                        KeyframeMode::MaintainTiming,
                        t!("paste_attr.maintain_timing"),
                    );
                    ui.radio_value(
                        &mut dialog.keyframe_mode,
                        KeyframeMode::StretchToFit,
                        t!("paste_attr.stretch_to_fit"),
                    );
                });

                for (group, title) in [
                    (AttributeGroup::Clip, t!("paste_attr.clip_attributes")),
                    (AttributeGroup::Video, t!("paste_attr.video_attributes")),
                    (AttributeGroup::Audio, t!("paste_attr.audio_attributes")),
                ] {
                    if !dialog.group_applies(group) {
                        continue;
                    }
                    let members: Vec<Attribute> = Attribute::ALL
                        .into_iter()
                        .filter(|a| a.group() == group)
                        .collect();
                    ui.separator();
                    let mut all = members.iter().all(|a| dialog.selected.contains(a));
                    if ui
                        .checkbox(&mut all, egui::RichText::new(title).strong())
                        .changed()
                    {
                        for attribute in &members {
                            if all {
                                dialog.selected.insert(*attribute);
                            } else {
                                dialog.selected.remove(attribute);
                            }
                        }
                    }
                    egui::Grid::new(("paste_attributes_group", group as usize))
                        .num_columns(3)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            for (i, attribute) in members.iter().enumerate() {
                                let mut on = dialog.selected.contains(attribute);
                                if ui.checkbox(&mut on, attribute.label()).changed() {
                                    if on {
                                        dialog.selected.insert(*attribute);
                                    } else {
                                        dialog.selected.remove(attribute);
                                    }
                                }
                                if i % 3 == 2 {
                                    ui.end_row();
                                }
                            }
                        });
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t!("common.cancel")).clicked() {
                        cancel = true;
                    }
                    if ui
                        .add_enabled(
                            !dialog.selected.is_empty(),
                            egui::Button::new(t!("paste_attr.apply")),
                        )
                        .clicked()
                    {
                        apply = true;
                    }
                });
            });
        if !open {
            cancel = true;
        }
        if !(apply || cancel) {
            return;
        }
        let dialog = self.paste_attributes.take().expect("just checked");
        // The choices stay for the next time, as in the other NLEs.
        self.paste_attributes_selection = dialog.selected.clone();
        self.paste_attributes_keyframe_mode = dialog.keyframe_mode;
        if apply {
            self.apply_paste_attributes(&dialog);
        }
    }

    fn apply_paste_attributes(&mut self, dialog: &PasteAttributesDialog) {
        let Some(timeline_id) = self.timeline_id else {
            return;
        };
        let mut commands: Vec<(usize, ClipId, ClipAttributes)> = Vec::new();
        {
            let tl = &self.project.timelines[timeline_id];
            for &(track_index, clip_id) in &dialog.targets {
                let (Some(track), Some(target)) =
                    (tl.tracks.get(track_index), tl.clip(track_index, clip_id))
                else {
                    continue;
                };
                if tl.is_locked(track_index) {
                    continue;
                }
                let source = match track.kind {
                    TrackKind::Video => dialog.source_video.as_ref(),
                    TrackKind::Audio => dialog.source_audio.as_ref(),
                };
                let Some(source) = source else {
                    continue;
                };
                if source.id == target.id {
                    continue;
                }
                commands.push((
                    track_index,
                    clip_id,
                    merged_attributes(source, target, &dialog.selected, dialog.keyframe_mode),
                ));
            }
        }
        if commands.is_empty() {
            return;
        }
        let mark = self.history.begin_group();
        for (track_index, clip_id, attributes) in commands {
            self.history.do_command(
                &mut self.project,
                Box::new(vv_core::SetClipAttributes::new(
                    timeline_id,
                    track_index,
                    clip_id,
                    attributes,
                )),
            );
        }
        self.history
            .end_group_as(mark, vv_core::CommandLabel::PasteAttributes);
    }
}

/// The attributes of `target` with the ones selected replaced by those of
/// `source`. Durations (fades, transitions) are clamped to the target clip,
/// which can be shorter than the source one.
fn merged_attributes(
    source: &Clip,
    target: &Clip,
    selected: &HashSet<Attribute>,
    mode: KeyframeMode,
) -> ClipAttributes {
    let mut attributes = ClipAttributes::of(target);
    let remap = KeyframeRemap::new(source, target, mode);
    let len = target.timeline_len.max(1);

    for attribute in selected {
        for param in attribute.transform_params() {
            *attributes.effects.transform.track_mut(*param) =
                remap.apply(source.effects.transform.track(*param));
        }
        match attribute {
            Attribute::Fades => {
                attributes.fade_in = source.fade_in.clamp(0, len);
                attributes.fade_out = source.fade_out.clamp(0, len);
            }
            Attribute::BlendMode => attributes.effects.blend_mode = source.effects.blend_mode,
            Attribute::Flip => attributes.effects.transform.flip = source.effects.transform.flip,
            Attribute::Filters => attributes.effects.filters = source.effects.filters.clone(),
            Attribute::Transitions => {
                let clamped = |t: &Option<vv_core::Transition>| {
                    t.clone().map(|mut t| {
                        t.duration = t.duration.clamp(1, len);
                        t
                    })
                };
                attributes.effects.transition_in = clamped(&source.effects.transition_in);
                attributes.effects.transition_out = clamped(&source.effects.transition_out);
            }
            Attribute::Volume => {
                attributes.effects.gain_db = remap.apply(&source.effects.gain_db);
            }
            _ => {}
        }
    }
    attributes
}

/// Source frames of the keyframes of `source` brought into the space of
/// `target`: the clips can start at a different point of their media and
/// last a different time.
struct KeyframeRemap {
    source_in: FrameIdx,
    target_in: FrameIdx,
    target_out: FrameIdx,
    /// `None` keeps the distances from the start of the clip (Maintain Timing).
    scale: Option<f64>,
}

impl KeyframeRemap {
    fn new(source: &Clip, target: &Clip, mode: KeyframeMode) -> Self {
        let scale = match mode {
            KeyframeMode::MaintainTiming => None,
            KeyframeMode::StretchToFit => {
                Some(target.source_len().max(1) as f64 / source.source_len().max(1) as f64)
            }
        };
        Self {
            source_in: source.source_in(),
            target_in: target.source_in(),
            target_out: target.source_out(),
            scale,
        }
    }

    fn frame(&self, frame: FrameIdx) -> FrameIdx {
        let offset = frame - self.source_in;
        let offset = match self.scale {
            Some(scale) => (offset as f64 * scale).round() as FrameIdx,
            None => offset,
        };
        self.target_in + offset
    }

    /// Keyframes outside the target clip are dropped: they would not be
    /// reachable and the trim would delete them anyway.
    fn apply<T: Clone>(&self, track: &Keyframed<T>) -> Keyframed<T> {
        let mut out = Keyframed::constant(track.default.clone());
        for (frame, value, interpolation) in track.keyframes() {
            let frame = self.frame(*frame);
            if (self.target_in..self.target_out).contains(&frame) {
                out.upsert(frame, value.clone(), *interpolation);
            }
        }
        out
    }
}

#[cfg(test)]
#[path = "tests/paste_attributes.rs"]
mod tests;
