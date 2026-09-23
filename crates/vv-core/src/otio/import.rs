//! Positions arrive in seconds at an arbitrary rate and are quantized to
//! the timeline frame. Tracks are read by accumulating the seconds and
//! rounding the start and end of each element, so no drift accumulates
//! along the track. A file exported by Venturi comes back identical thanks
//! to `metadata.venturi`.

use super::{MeasureTitle, OtioError, generator};
use crate::model::{
    Clip, ClipSource, Ease, EffectStack, FrameIdx, Interpolation, Keyframed, LinkGroupId, MediaId,
    MediaItem, MediaMeta, Project, PushDirection, Rational, Rgba, Timeline, Track, TrackKind,
    TransformParam, Transition, TransitionKind,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Metadata of a media and its `content_hash`, or a readable error.
pub type ProbeResult = Result<(MediaMeta, u64), String>;

pub struct OtioImport {
    pub project: Project,
    /// What was not imported or was approximated, for the user.
    pub warnings: Vec<OtioWarning>,
}

/// The text is composed by the UI, in its own language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtioWarning {
    EffectIgnored { effect: String, clips: usize },
    EffectPartlyIgnored { effect: String, clips: usize },
    SpeedNotApplied { clip: String, percent: i64 },
    UnsupportedInStack { schema: String },
    TrackKindIgnored { kind: Option<String> },
    TransitionIgnored,
    UnsupportedItem { schema: String },
    ClipWithoutDuration { clip: String },
    ClipDisabled { clip: String },
    ClipShorterThanAFrame { clip: String },
    AudioOnlyOnVideoTrack { clip: String },
    UnsupportedReference { clip: String, schema: String },
    UnsupportedUrl { url: String },
    MediaUnreadable { path: PathBuf, error: String },
}

pub fn import_otio(
    path: &Path,
    mut probe: impl FnMut(&Path) -> ProbeResult,
    measure: Option<MeasureTitle>,
) -> Result<OtioImport, OtioError> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let base_dir = path.parent().unwrap_or(Path::new("."));
    project_from_otio(&value, base_dir, &mut probe, measure)
}

/// `base_dir` resolves relative `target_url`s.
pub fn project_from_otio(
    value: &Value,
    base_dir: &Path,
    probe: &mut dyn FnMut(&Path) -> ProbeResult,
    measure: Option<MeasureTitle>,
) -> Result<OtioImport, OtioError> {
    let mut timelines = Vec::new();
    collect_timelines(value, &mut timelines);
    if timelines.is_empty() {
        return Err(OtioError::Format("no timeline in the OTIO file".into()));
    }
    let mut importer = Importer {
        project: Project::default(),
        warnings: Vec::new(),
        media: HashMap::new(),
        ignored_effects: BTreeMap::new(),
        base_dir,
        probe,
        measure,
    };
    for timeline in timelines {
        importer.timeline(timeline);
    }
    for ((effect, partial), clips) in std::mem::take(&mut importer.ignored_effects) {
        importer.warn(match partial {
            true => OtioWarning::EffectPartlyIgnored { effect, clips },
            false => OtioWarning::EffectIgnored { effect, clips },
        });
    }
    Ok(OtioImport {
        project: importer.project,
        warnings: importer.warnings,
    })
}

struct Importer<'a> {
    project: Project,
    warnings: Vec<OtioWarning>,
    media: HashMap<PathBuf, Option<MediaId>>,
    /// Effect name → how many clips had it: one warning per effect,
    /// not one per clip. `partial`: something of the effect was translated.
    ignored_effects: BTreeMap<(String, bool), usize>,
    base_dir: &'a Path,
    probe: &'a mut dyn FnMut(&Path) -> ProbeResult,
    measure: Option<MeasureTitle<'a>>,
}

/// Space of linked group numbers in the file: ours and Resolve's must not
/// be mixed.
#[derive(PartialEq, Eq, Hash)]
enum GroupKey {
    Venturi(u64),
    Resolve(u64),
}

/// A clip read from a file not our own, a candidate for automatic
/// video+audio linking.
struct ForeignClip {
    track: usize,
    index: usize,
}

impl Importer<'_> {
    fn timeline(&mut self, otio: &Value) {
        let venturi = &otio["metadata"]["venturi"];
        let fps = serde_json::from_value::<Rational>(venturi["fps"].clone())
            .ok()
            .or_else(|| otio["global_start_time"]["rate"].as_f64().map(Rational::from_fps))
            .or_else(|| first_item_rate(otio).map(Rational::from_fps))
            .unwrap_or(Rational::new(30, 1));

        let mut groups: HashMap<GroupKey, LinkGroupId> = HashMap::new();
        let mut foreign = Vec::new();
        let mut tracks = Vec::new();
        for otio_track in children(&otio["tracks"]) {
            if schema(otio_track) != "Track" {
                self.warn(OtioWarning::UnsupportedInStack { schema: schema(otio_track).to_owned() });
                continue;
            }
            let kind = match otio_track["kind"].as_str() {
                Some("Video") => TrackKind::Video,
                Some("Audio") => TrackKind::Audio,
                other => {
                    self.warn(OtioWarning::TrackKindIgnored { kind: other.map(str::to_owned) });
                    continue;
                }
            };
            let mut track = Track::new(kind);
            track.muted = otio_track["enabled"] == false;
            let mut cursor = 0.0;
            let mut pending_in = None;
            for item in children(otio_track) {
                let start = to_frames(cursor, fps);
                let duration = match schema(item) {
                    // Takes no time on the track: it overlaps its neighbours.
                    "Transition" => {
                        pending_in = self.transition(item, fps, &mut track);
                        continue;
                    }
                    "Gap" => item_duration(item),
                    "Clip" => {
                        let (duration, clip) =
                            self.clip(item, kind, fps, start, cursor, &mut groups);
                        if let Some((mut clip, is_foreign)) = clip {
                            if let Some(transition) = pending_in.take() {
                                clip.effects.transition_in = Some(transition);
                            }
                            if is_foreign {
                                foreign.push(ForeignClip {
                                    track: tracks.len(),
                                    index: track.clips.len(),
                                });
                            }
                            track.clips.push(clip);
                        }
                        duration
                    }
                    other => {
                        self.warn(OtioWarning::UnsupportedItem { schema: other.to_owned() });
                        item_duration(item)
                    }
                };
                cursor += duration;
            }
            tracks.push(track);
        }
        if !tracks.iter().any(|t| t.kind == TrackKind::Video) {
            tracks.insert(0, Track::new(TrackKind::Video));
            for clip in &mut foreign {
                clip.track += 1;
            }
        }
        self.link_foreign_clips(&mut tracks, &foreign);

        let resolution = serde_json::from_value::<(u32, u32)>(venturi["resolution"].clone())
            .ok()
            .or_else(|| self.first_video_resolution(&tracks))
            .unwrap_or((1920, 1080));
        self.project.timelines.insert(Timeline {
            name: otio["name"].as_str().unwrap_or("Timeline").to_owned(),
            fps,
            resolution,
            tracks,
        });
    }

    /// A transition straddles the cut: `in_offset` extends into the clip
    /// before it, which takes it as its `transition_out`, and `out_offset`
    /// into the one after, returned so the caller can attach it.
    fn transition(&mut self, item: &Value, fps: Rational, track: &mut Track) -> Option<Transition> {
        let venturi = &item["metadata"]["venturi"];
        let saved = serde_json::from_value::<Transition>(venturi["transition"].clone()).ok();
        let offset = |key: &str| to_frames(seconds(&item[key]).unwrap_or(0.0), fps);
        let (into_previous, into_next) = (offset("in_offset"), offset("out_offset"));
        if into_previous <= 0 && into_next <= 0 {
            return None;
        }
        let build = |duration: FrameIdx| match &saved {
            Some(transition) => Transition { duration, ..transition.clone() },
            None => {
                let effect = &item["metadata"]["Resolve_OTIO"]["Effects"];
                Transition {
                    kind: TransitionKind::Push,
                    duration,
                    direction: PushDirection::Left,
                    ease: resolve_ease(effect),
                    curve: resolve_curve(effect, duration),
                }
            }
        };
        if into_previous > 0 {
            match track.clips.last_mut() {
                Some(clip) => clip.effects.transition_out = Some(build(into_previous)),
                None => self.warn(OtioWarning::TransitionIgnored),
            }
        }
        (into_next > 0).then(|| build(into_next))
    }

    /// Duration occupied on the track (even if the clip is not imported)
    /// and the clip, with `true` if it does not come from Venturi.
    #[allow(clippy::too_many_arguments)]
    fn clip(
        &mut self,
        item: &Value,
        kind: TrackKind,
        fps: Rational,
        timeline_start: FrameIdx,
        cursor: f64,
        groups: &mut HashMap<GroupKey, LinkGroupId>,
    ) -> (f64, Option<(Clip, bool)>) {
        let name = item["name"].as_str().unwrap_or("");
        let reference = match item.get("media_references") {
            Some(refs) => {
                let key = item["active_media_reference_key"].as_str().unwrap_or("DEFAULT_MEDIA");
                &refs[key]
            }
            None => &item["media_reference"],
        };
        let Some((source_start, duration)) =
            time_range(&item["source_range"]).or_else(|| time_range(&reference["available_range"]))
        else {
            self.warn(OtioWarning::ClipWithoutDuration { clip: name.to_owned() });
            return (0.0, None);
        };
        let timeline_len = to_frames(cursor + duration, fps) - timeline_start;
        if item["enabled"] == false {
            self.warn(OtioWarning::ClipDisabled { clip: name.to_owned() });
            return (duration, None);
        }
        if timeline_len < 1 {
            self.warn(OtioWarning::ClipShorterThanAFrame { clip: name.to_owned() });
            return (duration, None);
        }
        let venturi = &item["metadata"]["venturi"];
        let ours = !venturi["effects"].is_null();
        let mut effects = serde_json::from_value::<EffectStack>(venturi["effects"].clone())
            .unwrap_or_default();
        let mut fades = (
            venturi["fade_in"].as_i64().unwrap_or(0) as FrameIdx,
            venturi["fade_out"].as_i64().unwrap_or(0) as FrameIdx,
        );
        // Seconds into the media at the start of the clip and the media fps, to
        // translate the effect keyframes of other editors.
        let (source, rate, source_offset, media_start) = match schema(reference) {
            "ExternalReference" => {
                let url = reference["target_url"].as_str().unwrap_or("");
                let Some(media_id) = self.media(url) else {
                    return (duration, None);
                };
                let meta = &self.project.media_pool[media_id].meta;
                if kind == TrackKind::Video && !meta.has_video {
                    self.warn(OtioWarning::AudioOnlyOnVideoTrack { clip: name.to_owned() });
                    return (duration, None);
                }
                let media_fps = meta.fps;
                let rate = Rational::conform_rate(fps, media_fps);
                let available_start =
                    time_range(&reference["available_range"]).map_or(0.0, |(start, _)| start);
                let secs = source_start - available_start;
                let source_frame = secs * media_fps.as_f64();
                let offset = if (source_frame - source_frame.round()).abs() < 1e-6 {
                    rate.scale_round(source_frame.round() as FrameIdx)
                } else {
                    to_frames(secs, fps)
                };
                (ClipSource::Media(media_id), rate, offset.max(0), Some((secs, media_fps)))
            }
            "GeneratorReference" if reference["generator_kind"] == "Solid Color" => {
                if effects.color.is_none() {
                    let color = generator::read_solid_color(reference).unwrap_or(Rgba::BLACK);
                    effects.color = Some(Keyframed::constant(color));
                }
                (ClipSource::SolidColor, Rational::one(), 0, None)
            }
            "GeneratorReference" if reference["generator_kind"] == "Rich" => {
                if effects.title.is_none() {
                    let frame = self.timeline_resolution(venturi, &ClipSource::Text);
                    effects.title = Some(generator::read_text(reference, frame, self.measure));
                }
                (ClipSource::Text, Rational::one(), 0, None)
            }
            other => {
                self.warn(OtioWarning::UnsupportedReference { clip: name.to_owned(), schema: other.to_owned() });
                return (duration, None);
            }
        };

        let resolve = &item["metadata"]["Resolve_OTIO"];
        // Our own file already carries everything in `venturi`: translating
        // Resolve's effects again would fight with it.
        if !ours {
            let frame = self.timeline_resolution(venturi, &source);
            let media = self.media_resolution(&source).unwrap_or(frame);
            let fit = (frame.0 / media.0).min(frame.1 / media.1);
            let context = ResolveContext {
                media_start,
                rate: item["source_range"]["start_time"]["rate"]
                    .as_f64()
                    .unwrap_or(fps.as_f64()),
                display: (media.0 * fit, media.1 * fit),
                media,
            };
            for effect in children_of(item, "effects") {
                self.effect(effect, &mut effects, &mut fades, &context);
            }
        }
        // The speed survives in the project but nothing plays it back yet:
        // the clip would show its first frames at 1x.
        if !effects.speed.is_constant() || effects.speed.default != 1.0 {
            self.warn(OtioWarning::SpeedNotApplied {
                clip: name.to_owned(),
                percent: (effects.speed.default * 100.0).round() as i64,
            });
        }

        let group_key = venturi["linked_group"]
            .as_u64()
            .map(GroupKey::Venturi)
            .or_else(|| resolve["Link Group ID"].as_u64().map(GroupKey::Resolve));
        let is_foreign = group_key.is_none();
        let linked_group = group_key
            .map(|key| *groups.entry(key).or_insert_with(|| self.project.alloc_link_group_id()));
        let audio_stream_index = venturi["audio_stream_index"]
            .as_u64()
            .or_else(|| resolve_source_track(resolve))
            .unwrap_or(0) as usize;
        let clip = Clip {
            id: self.project.alloc_clip_id(),
            source,
            source_offset,
            timeline_start,
            timeline_len,
            effects,
            linked_group,
            audio_stream_index,
            rate,
            disabled: false,
            fade_in: fades.0.clamp(0, timeline_len),
            fade_out: fades.1.clamp(0, timeline_len),
            display_color: serde_json::from_value(venturi["display_color"].clone()).unwrap_or(None),
        };
        (duration, Some((clip, is_foreign)))
    }

    /// Brings into `effects` and `fades` what it knows how to translate; the
    /// rest ends up in `ignored_effects`, except for effects that are off or
    /// at their defaults (Resolve exports them all).
    fn effect(
        &mut self,
        effect: &Value,
        effects: &mut EffectStack,
        fades: &mut (FrameIdx, FrameIdx),
        context: &ResolveContext,
    ) {
        if schema(effect) == "LinearTimeWarp" {
            if let Some(scalar) = effect["time_scalar"].as_f64() {
                effects.speed = Keyframed::constant(scalar as f32);
            }
            return;
        }
        let resolve = &effect["metadata"]["Resolve_OTIO"];
        if resolve.is_null() {
            let name = effect["effect_name"].as_str().unwrap_or_else(|| schema(effect));
            *self.ignored_effects.entry((name.to_owned(), false)).or_default() += 1;
            return;
        }
        if resolve["Enabled"] == false {
            return;
        }
        let name = resolve["Effect Name"].as_str().unwrap_or("Resolve Effect");
        let (mut translated, mut untranslated) = (false, false);
        for parameter in children_of(resolve, "Parameters") {
            let id = parameter["Parameter ID"].as_str().unwrap_or("");
            if resolve_parameter(name, id, parameter, effects, fades, context) {
                translated = true;
            } else if !is_default_parameter(parameter) {
                untranslated = true;
            }
        }
        if untranslated {
            *self.ignored_effects.entry((name.to_owned(), translated)).or_default() += 1;
        }
    }

    /// Resolution of the timeline, which the clip gets fitted into.
    fn timeline_resolution(&self, venturi: &Value, source: &ClipSource) -> (f32, f32) {
        serde_json::from_value::<(u32, u32)>(venturi["resolution"].clone())
            .ok()
            .map(|(w, h)| (w as f32, h as f32))
            .or_else(|| self.media_resolution(source))
            .unwrap_or((1920.0, 1080.0))
    }

    fn media_resolution(&self, source: &ClipSource) -> Option<(f32, f32)> {
        match source {
            ClipSource::Media(id) => self
                .project
                .media_pool
                .get(*id)
                .map(|m| (m.meta.width as f32, m.meta.height as f32)),
            ClipSource::SolidColor | ClipSource::Text => None,
        }
    }

    /// The media at `target_url`, probed once per file.
    fn media(&mut self, url: &str) -> Option<MediaId> {
        let Some(path) = url_to_path(url, self.base_dir) else {
            self.warn(OtioWarning::UnsupportedUrl { url: url.to_owned() });
            return None;
        };
        if let Some(media) = self.media.get(&path) {
            return *media;
        }
        let media = match (self.probe)(&path) {
            Ok((meta, content_hash)) => Some(self.project.media_pool.insert(MediaItem {
                path: path.clone(),
                meta,
                content_hash,
                compound: None,
            })),
            Err(e) => {
                self.warn(OtioWarning::MediaUnreadable { path: path.clone(), error: e });
                None
            }
        };
        self.media.insert(path, media);
        media
    }

    /// Other editors export video and audio of the same media as separate
    /// clips: the ones that coincide in everything get relinked.
    fn link_foreign_clips(&mut self, tracks: &mut [Track], foreign: &[ForeignClip]) {
        let mut by_span: HashMap<(MediaId, FrameIdx, FrameIdx, FrameIdx), Vec<&ForeignClip>> =
            HashMap::new();
        for clip_ref in foreign {
            let clip = &tracks[clip_ref.track].clips[clip_ref.index];
            if let ClipSource::Media(media_id) = clip.source {
                let key = (media_id, clip.timeline_start, clip.source_offset, clip.timeline_len);
                by_span.entry(key).or_default().push(clip_ref);
            }
        }
        for members in by_span.into_values() {
            let kinds = |kind| members.iter().any(|m| tracks[m.track].kind == kind);
            if !(kinds(TrackKind::Video) && kinds(TrackKind::Audio)) {
                continue;
            }
            let group = self.project.alloc_link_group_id();
            for member in members {
                tracks[member.track].clips[member.index].linked_group = Some(group);
            }
        }
    }

    fn first_video_resolution(&self, tracks: &[Track]) -> Option<(u32, u32)> {
        tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video)
            .flat_map(|t| &t.clips)
            .find_map(|clip| match clip.source {
                ClipSource::Media(id) => {
                    self.project.media_pool.get(id).map(|m| (m.meta.width, m.meta.height))
                }
                ClipSource::SolidColor | ClipSource::Text => None,
            })
    }

    fn warn(&mut self, warning: OtioWarning) {
        self.warnings.push(warning);
    }
}

/// What is needed to bring a Resolve parameter back into our units:
/// `media_start` (seconds into the media at the start of the clip and the
/// media fps) and `rate` (fps of Resolve's keyframes) place the keyframes,
/// `frame` and `media` denormalize the values.
struct ResolveContext {
    media_start: Option<(f64, Rational)>,
    rate: f64,
    display: (f32, f32),
    media: (f32, f32),
}

/// `true` if the parameter was translated. The `multiplier` is the inverse
/// of the one used on export: see `otio::resolve`.
fn resolve_parameter(
    effect: &str,
    id: &str,
    parameter: &Value,
    effects: &mut EffectStack,
    fades: &mut (FrameIdx, FrameIdx),
    context: &ResolveContext,
) -> bool {
    use TransformParam::*;
    let mut track = |param, multiplier| {
        resolve_track(parameter, effects, param, multiplier, context);
        true
    };
    match (effect, id) {
        ("Transform", "transformationZoomX") => track(ZoomX, 1.0),
        ("Transform", "transformationZoomY") => track(ZoomY, 1.0),
        ("Transform", "transformationPan") => track(PositionX, context.display.0),
        ("Transform", "transformationTilt") => track(PositionY, context.display.1),
        ("Transform", "transformationRotationAngle") => track(Rotation, -1.0),
        ("Transform", "transformationAnchorPoint") => {
            resolve_point(parameter, effects, [AnchorX, AnchorY], context.display, context);
            true
        }
        ("Transform", "transformationFlipX") => resolve_flip(parameter, effects, 0),
        ("Transform", "transformationFlipY") => resolve_flip(parameter, effects, 1),
        ("Cropping", "cropLeft") => track(CropLeft, context.media.0),
        ("Cropping", "cropRight") => track(CropRight, context.media.0),
        ("Cropping", "cropTop") => track(CropTop, context.media.1),
        ("Cropping", "cropBottom") => track(CropBottom, context.media.1),
        ("Cropping", "cropSoftness") => track(CropSoftness, 1.0),
        ("Composite", "opacity") => track(Opacity, 1.0),
        ("Composite", "composite mode") => match parameter["Parameter Value"]
            .as_u64()
            .and_then(super::resolve::blend_mode)
        {
            Some(blend) => {
                effects.blend_mode = blend;
                true
            }
            // Resolve has modes we do not: let the warning through.
            None => false,
        },
        ("Video Faders", "videoFaderIn")
        | ("Fairlight Clip Volume and Fades", "faderIn") => {
            fades.0 = resolve_frames(parameter);
            true
        }
        ("Video Faders", "videoFaderOut")
        | ("Fairlight Clip Volume and Fades", "faderOut") => {
            fades.1 = resolve_frames(parameter);
            true
        }
        ("Fairlight Clip Volume and Fades", "volume") => {
            resolve_volume(parameter, effects, context);
            true
        }
        _ => false,
    }
}

fn resolve_flip(parameter: &Value, effects: &mut EffectStack, axis: usize) -> bool {
    if let Some(flipped) = parameter["Parameter Value"].as_bool() {
        effects.transform.flip[axis] = flipped;
    }
    true
}

fn resolve_frames(parameter: &Value) -> FrameIdx {
    parameter["Parameter Value"].as_f64().unwrap_or(0.0).round().max(0.0) as FrameIdx
}

fn resolve_track(
    parameter: &Value,
    effects: &mut EffectStack,
    param: TransformParam,
    multiplier: f32,
    context: &ResolveContext,
) {
    let track = effects.transform.track_mut(param);
    if let Some(value) = parameter["Parameter Value"].as_f64() {
        track.default = value as f32 * multiplier;
    }
    for (frame, value) in resolve_keyframes(parameter, context) {
        let Some(value) = value.as_f64() else { continue };
        track.upsert(frame, value as f32 * multiplier, Interpolation::Linear);
    }
}

/// A `POINTF` feeds two tracks at once.
fn resolve_point(
    parameter: &Value,
    effects: &mut EffectStack,
    params: [TransformParam; 2],
    multiplier: (f32, f32),
    context: &ResolveContext,
) {
    let axis = |value: &Value, i: usize| value.get(i).and_then(Value::as_f64);
    let multiplier = [multiplier.0, multiplier.1];
    for (i, param) in params.into_iter().enumerate() {
        let track = effects.transform.track_mut(param);
        if let Some(value) = axis(&parameter["Parameter Value"], i) {
            track.default = value as f32 * multiplier[i];
        }
        for (frame, value) in resolve_keyframes(parameter, context) {
            let Some(value) = axis(&value, i) else { continue };
            track.upsert(frame, value as f32 * multiplier[i], Interpolation::Linear);
        }
    }
}

/// Resolve indexes its keyframes by timeline frame from the start of the
/// clip; ours go on the source frame shown there.
fn resolve_keyframes<'v>(
    parameter: &'v Value,
    context: &ResolveContext,
) -> Vec<(FrameIdx, &'v Value)> {
    let (Some(keyframes), Some((start_secs, media_fps))) =
        (parameter["Key Frames"].as_object(), context.media_start)
    else {
        return Vec::new();
    };
    keyframes
        .iter()
        .filter_map(|(frame, keyframe)| {
            let frame = frame.parse::<f64>().ok()?;
            let secs = start_secs + frame / context.rate;
            Some(((secs * media_fps.as_f64()).round() as FrameIdx, &keyframe["Value"]))
        })
        .collect()
}

fn is_default_parameter(parameter: &Value) -> bool {
    let no_keyframes = parameter["Key Frames"].as_object().is_none_or(|k| k.is_empty());
    no_keyframes && parameter["Parameter Value"] == parameter["Default Parameter Value"]
}

/// Resolve's volume is in dB, like `gain_db`; the keyframes become linear
/// keyframes on the corresponding source frame.
fn resolve_volume(parameter: &Value, effects: &mut EffectStack, context: &ResolveContext) {
    if let Some(db) = parameter["Parameter Value"].as_f64() {
        effects.gain_db.default = db as f32;
    }
    for (frame, value) in resolve_keyframes(parameter, context) {
        let Some(db) = value.as_f64() else { continue };
        effects.gain_db.upsert(frame, db as f32, Interpolation::Linear);
    }
}

/// The intensity of the ease, read back from the longest bezier handle of
/// the progress curve (see `resolve::transition_curve`).
fn resolve_curve(effect: &Value, duration: FrameIdx) -> f32 {
    let handles = children_of(effect, "Parameters")
        .find(|p| p["Parameter ID"] == "transitionCustomCurvesKeyframes")
        .and_then(|p| p["Key Frames"].as_object())
        .into_iter()
        .flatten()
        .flat_map(|(_, key)| ["InBez", "OutBez"].map(|h| key[h][h][0].as_f64()))
        .flatten()
        .map(f64::abs);
    let longest = handles.fold(0.0, f64::max);
    match duration > 0 {
        true => (longest as f32 / (0.6 * duration as f32)).clamp(0.0, 1.0),
        false => 0.5,
    }
}

/// The `ease` of a Resolve transition, by position in `Ease::ALL`.
fn resolve_ease(effect: &Value) -> Ease {
    children_of(effect, "Parameters")
        .find(|p| p["Parameter ID"] == "ease")
        .and_then(|p| p["Parameter Value"].as_u64())
        .and_then(|i| Ease::ALL.get(i as usize).copied())
        .unwrap_or(Ease::InOut)
}

/// The audio stream of the media Resolve takes the clip's channels from,
/// if they all come from the same one.
fn resolve_source_track(resolve: &Value) -> Option<u64> {
    let mut tracks = children_of(resolve, "Channels").map(|c| c["Source Track ID"].as_u64());
    let first = tracks.next()??;
    tracks.all(|t| t == Some(first)).then_some(first)
}

fn collect_timelines<'v>(value: &'v Value, out: &mut Vec<&'v Value>) {
    match schema(value) {
        "Timeline" => out.push(value),
        "SerializableCollection" => {
            for child in children(value) {
                collect_timelines(child, out);
            }
        }
        _ => {}
    }
}

/// The schema name without the version (`"Clip.2"` → `"Clip"`).
fn schema(value: &Value) -> &str {
    let full = value["OTIO_SCHEMA"].as_str().unwrap_or("");
    full.split('.').next().unwrap_or(full)
}

fn children(value: &Value) -> impl Iterator<Item = &Value> {
    children_of(value, "children")
}

fn children_of<'v>(value: &'v Value, key: &str) -> impl Iterator<Item = &'v Value> {
    value[key].as_array().into_iter().flatten()
}

fn seconds(time: &Value) -> Option<f64> {
    let rate = time["rate"].as_f64()?;
    (rate > 0.0).then(|| time["value"].as_f64().unwrap_or(0.0) / rate)
}

/// `(start, duration)` in seconds.
fn time_range(range: &Value) -> Option<(f64, f64)> {
    Some((seconds(&range["start_time"])?, seconds(&range["duration"])?))
}

/// Duration of a non-imported element: `source_range`, or that of the
/// children (in sequence for a track, the longest for a stack).
fn item_duration(item: &Value) -> f64 {
    if let Some((_, duration)) = time_range(&item["source_range"]) {
        return duration;
    }
    let durations = children(item)
        .filter(|child| schema(child) != "Transition")
        .map(item_duration);
    match schema(item) {
        "Stack" => durations.fold(0.0, f64::max),
        _ => durations.sum(),
    }
}

fn first_item_rate(timeline: &Value) -> Option<f64> {
    children(&timeline["tracks"])
        .flat_map(children)
        .find_map(|item| item["source_range"]["duration"]["rate"].as_f64())
}

fn to_frames(secs: f64, fps: Rational) -> FrameIdx {
    (secs * fps.as_f64()).round() as FrameIdx
}

fn url_to_path(url: &str, base_dir: &Path) -> Option<PathBuf> {
    let Some(rest) = url.strip_prefix("file://") else {
        if url.contains("://") || url.is_empty() {
            return None;
        }
        return Some(base_dir.join(url));
    };
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let mut iter = rest.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BlendMode, ClipId};
    use crate::otio::timeline_to_otio;
    use serde_json::json;

    fn meta(fps: Rational, duration_frames: FrameIdx) -> MediaMeta {
        MediaMeta {
            duration_frames,
            fps,
            width: 1280,
            height: 720,
            has_video: true,
            has_audio: true,
            sample_rate: 48_000,
            channels: 2,
            audio_streams: 1,
        }
    }

    fn probe_from(
        media: Vec<(&'static str, MediaMeta)>,
    ) -> impl FnMut(&Path) -> ProbeResult {
        move |path| {
            media
                .iter()
                .find(|(p, _)| Path::new(p) == path)
                .map(|(_, m)| (m.clone(), 42))
                .ok_or_else(|| "file not found".to_owned())
        }
    }

    fn span(clip: &Clip) -> (FrameIdx, FrameIdx, FrameIdx, Rational) {
        (clip.timeline_start, clip.source_offset, clip.timeline_len, clip.rate)
    }

    /// Exporting and importing a Venturi project returns the same clips,
    /// even split mid source frame.
    #[test]
    fn a_venturi_export_imports_back_unchanged() {
        let media_meta = meta(Rational::new(30_000, 1001), 1000);
        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: "/tmp/a.mp4".into(),
            meta: media_meta.clone(),
            content_hash: 42,
            compound: None,
        });
        let timeline_id = project.timelines.insert(Timeline {
            name: "Montaggio".into(),
            fps: Rational::new(30, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track::new(TrackKind::Video),
                Track::new(TrackKind::Video),
                Track::new(TrackKind::Audio),
            ],
        });
        let rate = Rational::conform_rate(Rational::new(30, 1), media_meta.fps);
        let group = project.alloc_link_group_id();
        let mut video = Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(media),
            100,
            900,
            20,
            rate,
        );
        video.linked_group = Some(group);
        let mut audio = video.clone();
        audio.id = ClipId(2);
        audio.audio_stream_index = 1;
        audio.effects.gain_db.upsert(10, -6.0, Interpolation::Linear);
        let mut color =
            Clip::from_source_range(ClipId(3), ClipSource::SolidColor, 0, 45, 300, Rational::one());
        color.effects.color = Some(Keyframed::constant(Rgba { r: 0.2, g: 0.4, b: 0.6, a: 1.0 }));
        color.fade_in = 5;
        color.fade_out = 7;
        color.effects.transition_in = Some(Transition {
            kind: TransitionKind::Push,
            duration: 9,
            direction: PushDirection::Up,
            ease: Ease::In,
            curve: 0.25,
        });
        let tl = &mut project.timelines[timeline_id];
        tl.tracks[0].clips.push(video);
        tl.tracks[1].clips.push(color);
        tl.tracks[2].clips.push(audio);
        tl.tracks[2].muted = true;
        let mut split = crate::SplitClip::new(timeline_id, 0, ClipId(1), 500);
        crate::Command::apply(&mut split, &mut project);

        let otio = timeline_to_otio(&project, timeline_id, None);
        let mut probe = probe_from(vec![("/tmp/a.mp4", media_meta.clone())]);
        let imported = project_from_otio(&otio, Path::new("/"), &mut probe, None).unwrap();
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);

        let original = &project.timelines[timeline_id];
        let (_, back) = imported.project.timelines.iter().next().unwrap();
        assert_eq!(back.name, "Montaggio");
        assert_eq!(back.fps, original.fps);
        assert_eq!(back.resolution, original.resolution);
        assert_eq!(back.tracks.len(), 3);
        for (a, b) in original.tracks.iter().zip(&back.tracks) {
            assert_eq!((a.kind, a.muted), (b.kind, b.muted));
            let spans_a: Vec<_> = a.clips.iter().map(span).collect();
            let spans_b: Vec<_> = b.clips.iter().map(span).collect();
            assert_eq!(spans_a, spans_b);
        }
        let audio = &back.tracks[2].clips[0];
        assert_eq!(audio.audio_stream_index, 1);
        assert_eq!(audio.effects.gain_db.value_at(10), -6.0);
        assert!(audio.linked_group.is_some());
        assert_eq!(back.tracks[0].clips[0].linked_group, audio.linked_group);
        assert_eq!(back.tracks[0].clips[1].linked_group, None, "right half unlinked");
        let generated = &back.tracks[1].clips[0];
        let color = generated.effects.color.as_ref().unwrap().default;
        assert_eq!((color.r, color.g, color.b), (0.2, 0.4, 0.6));
        assert_eq!((generated.fade_in, generated.fade_out), (5, 7));
        assert_eq!(
            generated.effects.transition_in.as_ref().map(|t| (t.direction, t.duration, t.ease)),
            Some((PushDirection::Up, 9, Ease::In)),
            "the transition comes back whole from metadata.venturi"
        );
    }

    /// Generators go back and forth through Resolve's own blocks: the same
    /// file, with our metadata stripped, has to rebuild the title from the
    /// Qt rich text and the colour from the hex.
    #[test]
    fn reads_back_the_generators_without_our_metadata() {
        let measure = |_: &crate::model::TitleParams| {
            crate::TitleMetrics { block: (440.0, 176.0), padding: 20.0 }
        };
        let mut project = Project::default();
        let timeline_id = project.timelines.insert(Timeline {
            name: "Generatori".into(),
            fps: Rational::new(30, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Video)],
        });
        let mut color =
            Clip::from_source_range(ClipId(1), ClipSource::SolidColor, 0, 60, 0, Rational::one());
        color.effects.color =
            Some(Keyframed::constant(Rgba { r: 0.231_372_55, g: 0.709_803_94, b: 0.207_843_14, a: 1.0 }));
        let mut text =
            Clip::from_source_range(ClipId(2), ClipSource::Text, 0, 60, 0, Rational::one());
        let title = crate::model::TitleParams {
            content: "Two\nlines & <>".into(),
            font_family: "Open Sans".into(),
            font_weight: 700,
            italic: true,
            underline: true,
            size: 72.0,
            align: crate::model::TextAlign::Left,
            anchor: (crate::model::HAnchor::Right, crate::model::VAnchor::Bottom),
            position: [192.0, -108.0],
            background: crate::model::TitleBackground {
                enabled: true,
                width: 0.25,
                height: 0.2,
                corner_radius: 0.1,
                ..Default::default()
            },
            ..Default::default()
        };
        text.effects.title = Some(title.clone());
        project.timelines[timeline_id].tracks[0].clips.push(color);
        project.timelines[timeline_id].tracks[1].clips.push(text);

        let mut otio = timeline_to_otio(&project, timeline_id, Some(&measure));
        for track in otio["tracks"]["children"].as_array_mut().unwrap() {
            for clip in track["children"].as_array_mut().unwrap() {
                clip["metadata"]["venturi"] = json!(null);
            }
        }
        let mut probe = probe_from(vec![]);
        let imported =
            project_from_otio(&otio, Path::new("/"), &mut probe, Some(&measure)).unwrap();
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);

        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        let color = tl.tracks[0].clips[0].effects.color.as_ref().unwrap().default;
        assert_eq!(((color.r * 255.0).round(), (color.g * 255.0).round()), (59.0, 181.0));

        let back = tl.tracks[1].clips[0].effects.title.as_ref().unwrap();
        assert_eq!(back.content, title.content, "text and lines from the HTML blob");
        assert_eq!(back.font_family, "Open Sans");
        assert_eq!((back.font_weight, back.size), (700, 72.0));
        assert!(back.italic && back.underline && !back.strikethrough);
        assert_eq!(back.align, crate::model::TextAlign::Left);
        assert_eq!(back.anchor, title.anchor, "index 8 of the 3x3 grid");
        assert!((back.position[0] - 192.0).abs() < 0.01 && (back.position[1] + 108.0).abs() < 0.01);
        assert!(
            (back.background.corner_radius - title.background.corner_radius).abs() < 1e-4,
            "the radius comes back in our units: {}",
            back.background.corner_radius
        );
    }

    /// A clip exported by Resolve: the transform lives in `Effect.1` items
    /// with normalized values and keyframes on the frames of the clip.
    #[test]
    fn reads_the_transform_of_a_resolve_clip() {
        let parameter = |id: &str, value: Value| {
            json!({
                "Parameter ID": id,
                "Parameter Value": value,
                "Default Parameter Value": 0.0,
                "Variant Type": "Double",
                "Key Frames": {},
            })
        };
        let effect = |name: &str, parameters: Value| {
            json!({
                "OTIO_SCHEMA": "Effect.1",
                "name": "",
                "effect_name": "Resolve Effect",
                "metadata": { "Resolve_OTIO": {
                    "Effect Name": name,
                    "Name": name,
                    "Enabled": true,
                    "Parameters": parameters,
                }},
            })
        };
        let otio = json!({
            "OTIO_SCHEMA": "Timeline.1",
            "name": "From Resolve",
            "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [{
                "OTIO_SCHEMA": "Track.1",
                "kind": "Video",
                "children": [{
                    "OTIO_SCHEMA": "Clip.2",
                    "name": "one",
                    "source_range": range(0.0, 100.0, 24.0),
                    "media_references": { "DEFAULT_MEDIA": {
                        "OTIO_SCHEMA": "ExternalReference.1",
                        "target_url": B_ROLL,
                        "available_range": range(0.0, 2400.0, 24.0),
                    }},
                    "effects": [
                        json!({
                            "OTIO_SCHEMA": "LinearTimeWarp.1",
                            "name": "",
                            "effect_name": "",
                            "time_scalar": 2.0,
                        }),
                        effect("Transform", json!([
                            parameter("transformationZoomX", json!(1.07)),
                            parameter("transformationPan", json!(0.05)),
                            parameter("transformationTilt", json!(-0.05)),
                            parameter("transformationRotationAngle", json!(7.8)),
                            json!({
                                "Parameter ID": "transformationAnchorPoint",
                                "Parameter Value": [0.1, 0.0],
                                "Default Parameter Value": [0.0, 0.0],
                                "Variant Type": "POINTF",
                                "Key Frames": {
                                    "0": { "Value": [0.1, 0.0], "Variant Type": "POINTF" },
                                    "10": { "Value": [0.5, 0.0], "Variant Type": "POINTF" },
                                },
                            }),
                            json!({
                                "Parameter ID": "transformationFlipY",
                                "Parameter Value": true,
                                "Default Parameter Value": false,
                                "Variant Type": "Bool",
                            }),
                        ])),
                        effect("Cropping", json!([parameter("cropTop", json!(0.25))])),
                        effect("Composite", json!([
                            parameter("opacity", json!(80.0)),
                            json!({
                                "Parameter ID": "composite mode",
                                "Parameter Value": 5,
                                "Default Parameter Value": 0,
                                "Variant Type": "UInt",
                            }),
                        ])),
                        effect("Video Faders", json!([parameter("videoFaderIn", json!(12.0))])),
                    ],
                }],
            }]},
        });
        let mut probe = probe_from(vec![("/media/b roll.mov", meta(Rational::new(24, 1), 2400))]);
        let imported = project_from_otio(&otio, Path::new("/media"), &mut probe, None).unwrap();
        assert_eq!(
            imported.warnings,
            vec![OtioWarning::SpeedNotApplied { clip: "one".into(), percent: 200 }],
            "speed is kept but not played back"
        );

        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        let clip = &tl.tracks[0].clips[0];
        assert_eq!(clip.effects.speed.default, 2.0);
        assert_eq!(clip.effects.transform.flip, [false, true]);
        let t = clip.effects.transform.value_at(0);
        assert_eq!(t.zoom[0], 1.07);
        assert_eq!(t.position, [64.0, -36.0], "denormalized on 1280x720");
        assert_eq!(t.rotation, -7.8, "opposite direction to Resolve's");
        assert_eq!(t.anchor[0], 128.0);
        assert_eq!(t.crop[1], 180.0, "crop is in media pixels");
        assert_eq!(t.opacity, 80.0);
        assert_eq!(clip.effects.blend_mode, BlendMode::Screen);
        assert_eq!(clip.fade_in, 12);
        assert_eq!(
            clip.effects.transform.value_at(10).anchor[0],
            640.0,
            "keyframe on the matching source frame"
        );
    }

    fn rt(value: f64, rate: f64) -> Value {
        json!({ "OTIO_SCHEMA": "RationalTime.1", "value": value, "rate": rate })
    }

    fn range(start: f64, duration: f64, rate: f64) -> Value {
        json!({
            "OTIO_SCHEMA": "TimeRange.1",
            "start_time": rt(start, rate),
            "duration": rt(duration, rate),
        })
    }

    const B_ROLL: &str = "file:///media/b%20roll.mov";

    /// Source at 24 fps from frame `start` for `duration`.
    fn clip_1(name: &str, url: &str, start: f64, duration: f64, enabled: bool) -> Value {
        json!({
            "OTIO_SCHEMA": "Clip.1",
            "name": name,
            "source_range": range(start, duration, 24.0),
            "enabled": enabled,
            "effects": [],
            "media_reference": {
                "OTIO_SCHEMA": "ExternalReference.1",
                "target_url": url,
                // Media with a start timecode of 01:00:00:00 at 24 fps.
                "available_range": range(86_400.0, 2400.0, 24.0),
            },
        })
    }

    /// File from another editor: `Clip.1`, times in the media rate with a
    /// start timecode, transitions, disabled clips and missing media;
    /// video and audio of the same stretch must be relinked.
    #[test]
    fn a_foreign_file_imports_with_warnings_for_what_is_skipped() {
        let fps = 24_000.0 / 1001.0;
        let otio = json!({
            "OTIO_SCHEMA": "SerializableCollection.1",
            "children": [{
                "OTIO_SCHEMA": "Timeline.1",
                "name": "From Resolve",
                "global_start_time": rt(86_400.0, fps),
                "tracks": {
                    "OTIO_SCHEMA": "Stack.1",
                    "children": [
                        {
                            "OTIO_SCHEMA": "Track.1",
                            "kind": "Video",
                            "children": [
                                { "OTIO_SCHEMA": "Gap.1", "source_range": range(0.0, 24.0, fps) },
                                clip_1("one", B_ROLL, 86_448.0, 48.0, true),
                                { "OTIO_SCHEMA": "Transition.1", "in_offset": rt(6.0, fps) },
                                clip_1("disabled", B_ROLL, 86_400.0, 24.0, false),
                                clip_1("lost", "file:///media/missing.mov", 86_400.0, 24.0, true),
                                clip_1("two", "b roll.mov", 86_400.0, 12.0, true),
                            ],
                        },
                        {
                            "OTIO_SCHEMA": "Track.1",
                            "kind": "Audio",
                            "enabled": false,
                            "children": [
                                { "OTIO_SCHEMA": "Gap.1", "source_range": range(0.0, 24.0, fps) },
                                clip_1("one", B_ROLL, 86_448.0, 48.0, true),
                            ],
                        },
                    ],
                },
            }],
        });
        let mut probe = probe_from(vec![("/media/b roll.mov", meta(Rational::new(24, 1), 2400))]);
        let imported = project_from_otio(&otio, Path::new("/media"), &mut probe, None).unwrap();

        assert_eq!(imported.warnings.len(), 2, "{:?}", imported.warnings);
        assert_eq!(imported.warnings[0], OtioWarning::ClipDisabled { clip: "disabled".into() });
        assert!(matches!(&imported.warnings[1], OtioWarning::MediaUnreadable { path, .. } if path.ends_with("missing.mov")));
        assert_eq!(imported.project.media_pool.len(), 1, "same file probed once");

        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        assert_eq!(tl.fps, Rational::new(24_000, 1001));
        assert_eq!(tl.resolution, (1280, 720), "from the first video media");
        let video = &tl.tracks[0].clips;
        assert_eq!(video.len(), 2);
        assert_eq!((video[0].timeline_start, video[0].timeline_len), (24, 48));
        assert_eq!(video[0].source_in(), 48, "from the media start timecode");
        assert_eq!(video[1].timeline_start, 24 + 48 + 24 + 24, "after disabled and lost");
        assert_eq!(video[1].source_in(), 0, "relative path");
        let transition = video[0].effects.transition_out.as_ref().expect("outgoing transition");
        assert_eq!(transition.duration, 6, "in_offset reaches into the previous clip");

        let audio = &tl.tracks[1];
        assert!(audio.muted);
        assert!(video[0].linked_group.is_some());
        assert_eq!(audio.clips[0].linked_group, video[0].linked_group);
        assert_eq!(video[1].linked_group, None);
    }

    fn resolve_effect(name: &str, enabled: bool, parameters: Value) -> Value {
        json!({
            "OTIO_SCHEMA": "Effect.1",
            "effect_name": "Resolve Effect",
            "metadata": { "Resolve_OTIO": {
                "Effect Name": name,
                "Enabled": enabled,
                "Parameters": parameters,
            }},
        })
    }

    fn volume(value: f64, keyframes: Value) -> Value {
        json!([{
            "Parameter ID": "volume",
            "Default Parameter Value": 0.0,
            "Parameter Value": value,
            "Key Frames": keyframes,
        }])
    }

    /// Resolve exports the whole effect stack of every clip: volume
    /// becomes gain, disabled and default ones disappear, the rest is
    /// summarized in one warning per effect. Groups and audio streams come
    /// from its metadata.
    #[test]
    fn resolve_effects_links_and_channels_are_translated() {
        let resolve_clip = |start: f64, effects: Value, link: u64, source_track: u64| {
            let mut clip = clip_1("c", B_ROLL, 86_400.0 + start, 24.0, true);
            clip["effects"] = effects;
            clip["metadata"] = json!({ "Resolve_OTIO": {
                "Link Group ID": link,
                "Channels": [
                    { "Source Channel ID": 0, "Source Track ID": source_track },
                    { "Source Channel ID": 1, "Source Track ID": source_track },
                ],
            }});
            clip
        };
        let zoom = json!([{
            "Parameter ID": "zoom",
            "Default Parameter Value": 1.0,
            "Parameter Value": 1.5,
        }]);
        let track = |kind: &str, children: Vec<Value>| {
            json!({ "OTIO_SCHEMA": "Track.1", "kind": kind, "children": children })
        };
        let otio = json!({
            "OTIO_SCHEMA": "Timeline.1",
            "global_start_time": rt(0.0, 24.0),
            "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [
                track("Video", vec![
                    resolve_clip(0.0, json!([
                        resolve_effect("Transform", true, json!([])),
                        resolve_effect("Dynamic Zoom", false, zoom.clone()),
                        resolve_effect("Zoom", true, zoom.clone()),
                    ]), 5, 0),
                    resolve_clip(24.0, json!([resolve_effect("Zoom", true, zoom)]), 6, 0),
                ]),
                track("Audio", vec![
                    resolve_clip(0.0, json!([
                        resolve_effect(
                            "Fairlight Clip Volume and Fades",
                            true,
                            volume(3.5, json!({})),
                        ),
                    ]), 5, 1),
                    resolve_clip(24.0, json!([resolve_effect(
                        "Fairlight Clip Volume and Fades",
                        true,
                        volume(0.0, json!({ "0": { "Value": -6.0 }, "12": { "Value": 0.0 } })),
                    )]), 6, 1),
                ]),
            ]},
        });
        let mut probe = probe_from(vec![("/media/b roll.mov", meta(Rational::new(24, 1), 2400))]);
        let imported = project_from_otio(&otio, Path::new("/"), &mut probe, None).unwrap();

        assert_eq!(imported.warnings, [OtioWarning::EffectIgnored { effect: "Zoom".into(), clips: 2 }]);
        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        let (video, audio) = (&tl.tracks[0].clips, &tl.tracks[1].clips);
        assert_eq!(audio[0].effects.gain_db.value_at(0), 3.5);
        let gain = &audio[1].effects.gain_db;
        assert_eq!(gain.value_at(24), -6.0, "keyframe on the first source frame of the clip");
        assert_eq!(gain.value_at(36), 0.0);
        assert_eq!(gain.value_at(30), -3.0);
        assert_eq!(audio[0].audio_stream_index, 1);
        assert_eq!(video[0].linked_group, audio[0].linked_group);
        assert_eq!(video[1].linked_group, audio[1].linked_group);
        assert_ne!(video[0].linked_group, video[1].linked_group);
    }

    fn audio_only_meta() -> MediaMeta {
        MediaMeta {
            duration_frames: 300,
            fps: Rational::new(30, 1),
            width: 0,
            height: 0,
            has_video: false,
            has_audio: true,
            sample_rate: 48_000,
            channels: 2,
            audio_streams: 1,
        }
    }

    /// An audio-only media comes back identical from an export of ours, reads
    /// at any rate from another editor, and on a video track is discarded
    /// with a warning.
    #[test]
    fn audio_only_media_round_trips_and_is_refused_on_video_tracks() {
        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: "/tmp/voice.wav".into(),
            meta: audio_only_meta(),
            content_hash: 42,
            compound: None,
        });
        let fps = Rational::new(25, 1);
        let timeline_id = project.timelines.insert(Timeline {
            name: "Voice".into(),
            fps,
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        });
        let rate = Rational::conform_rate(fps, Rational::new(30, 1));
        project.timelines[timeline_id].tracks[1].clips.push(Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(media),
            30,
            270,
            10,
            rate,
        ));
        let mut split = crate::SplitClip::new(timeline_id, 1, ClipId(1), 77);
        crate::Command::apply(&mut split, &mut project);

        let otio = timeline_to_otio(&project, timeline_id, None);
        let mut probe = probe_from(vec![("/tmp/voice.wav", audio_only_meta())]);
        let imported = project_from_otio(&otio, Path::new("/"), &mut probe, None).unwrap();
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
        let (_, back) = imported.project.timelines.iter().next().unwrap();
        let spans = |t: &Timeline| t.tracks[1].clips.iter().map(span).collect::<Vec<_>>();
        assert_eq!(spans(back), spans(&project.timelines[timeline_id]));

        let wav_clip = |start_samples: f64| {
            json!({
                "OTIO_SCHEMA": "Clip.2",
                "name": "voice",
                "source_range": range(start_samples, 48_000.0, 48_000.0),
                "media_references": { "DEFAULT_MEDIA": {
                    "OTIO_SCHEMA": "ExternalReference.1",
                    "target_url": "file:///tmp/voice.wav",
                }},
                "active_media_reference_key": "DEFAULT_MEDIA",
            })
        };
        let track = |kind: &str| {
            json!({ "OTIO_SCHEMA": "Track.1", "kind": kind, "children": [wav_clip(24_000.0)] })
        };
        let foreign = json!({
            "OTIO_SCHEMA": "Timeline.1",
            "global_start_time": rt(0.0, 25.0),
            "tracks": { "OTIO_SCHEMA": "Stack.1", "children": [track("Video"), track("Audio")] },
        });
        let imported = project_from_otio(&foreign, Path::new("/"), &mut probe, None).unwrap();
        assert_eq!(imported.warnings.len(), 1, "{:?}", imported.warnings);
        assert!(matches!(imported.warnings[0], OtioWarning::AudioOnlyOnVideoTrack { .. }));
        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        assert!(tl.tracks[0].clips.is_empty());
        let clip = &tl.tracks[1].clips[0];
        assert_eq!((clip.timeline_len, clip.source_offset), (25, 13), "1 s from 0.5 s, at 25 fps");
        assert_eq!(clip.rate, rate);
        assert_eq!(tl.resolution, (1920, 1080));
    }

    #[test]
    fn a_file_without_timelines_is_an_error() {
        let mut probe = probe_from(vec![]);
        let not_a_timeline = json!({ "OTIO_SCHEMA": "Clip.2" });
        let result = project_from_otio(&not_a_timeline, Path::new("/"), &mut probe, None);
        assert!(matches!(result, Err(OtioError::Format(_))));
    }

    #[test]
    fn fps_from_float_recognises_ntsc_rates() {
        assert_eq!(Rational::from_fps(25.0), Rational::new(25, 1));
        assert_eq!(Rational::from_fps(29.97), Rational::new(30_000, 1001));
        assert_eq!(Rational::from_fps(30_000.0 / 1001.0), Rational::new(30_000, 1001));
        assert_eq!(Rational::from_fps(23.976), Rational::new(24_000, 1001));
        assert_eq!(Rational::from_fps(59.94), Rational::new(60_000, 1001));
        assert_eq!(Rational::from_fps(12.5), Rational::new(25, 2));
    }
}
