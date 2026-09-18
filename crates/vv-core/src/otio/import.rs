//! Le posizioni arrivano in secondi a un rate qualunque e vengono
//! quantizzate al frame della timeline. Le track si leggono accumulando i
//! secondi e arrotondando inizio e fine di ogni elemento, così non si
//! accumula deriva lungo la track. Un file esportato da VibeVideo torna
//! identico grazie a `metadata.vibevideo`.

use super::OtioError;
use crate::model::{
    Clip, ClipSource, EffectStack, FrameIdx, Interpolation, Keyframed, LinkGroupId, MediaId,
    MediaItem, MediaMeta, Project, Rational, Rgba, Timeline, Track, TrackKind,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Metadati di un media e il suo `content_hash`, o un errore leggibile.
pub type ProbeResult = Result<(MediaMeta, u64), String>;

pub struct OtioImport {
    pub project: Project,
    /// Quel che non è stato importato o è stato approssimato, per l'utente.
    pub warnings: Vec<String>,
}

pub fn import_otio(
    path: &Path,
    mut probe: impl FnMut(&Path) -> ProbeResult,
) -> Result<OtioImport, OtioError> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let base_dir = path.parent().unwrap_or(Path::new("."));
    project_from_otio(&value, base_dir, &mut probe)
}

/// `base_dir` risolve i `target_url` relativi.
pub fn project_from_otio(
    value: &Value,
    base_dir: &Path,
    probe: &mut dyn FnMut(&Path) -> ProbeResult,
) -> Result<OtioImport, OtioError> {
    let mut timelines = Vec::new();
    collect_timelines(value, &mut timelines);
    if timelines.is_empty() {
        return Err(OtioError::Format("nessuna timeline nel file OTIO".into()));
    }
    let mut importer = Importer {
        project: Project::default(),
        warnings: Vec::new(),
        media: HashMap::new(),
        ignored_effects: BTreeMap::new(),
        base_dir,
        probe,
    };
    for timeline in timelines {
        importer.timeline(timeline);
    }
    for (effect, clips) in std::mem::take(&mut importer.ignored_effects) {
        importer.warn(format!("effetto \"{effect}\" ignorato su {clips} clip"));
    }
    Ok(OtioImport {
        project: importer.project,
        warnings: importer.warnings,
    })
}

struct Importer<'a> {
    project: Project,
    warnings: Vec<String>,
    media: HashMap<PathBuf, Option<MediaId>>,
    /// Nome dell'effetto → quante clip lo avevano: un avviso per effetto,
    /// non uno per clip.
    ignored_effects: BTreeMap<String, usize>,
    base_dir: &'a Path,
    probe: &'a mut dyn FnMut(&Path) -> ProbeResult,
}

/// Spazio dei numeri di gruppo collegato nel file: i nostri e quelli di
/// Resolve non vanno mescolati.
#[derive(PartialEq, Eq, Hash)]
enum GroupKey {
    VibeVideo(u64),
    Resolve(u64),
}

/// Una clip letta da un file non nostro, candidata al collegamento
/// automatico video+audio.
struct ForeignClip {
    track: usize,
    index: usize,
}

impl Importer<'_> {
    fn timeline(&mut self, otio: &Value) {
        let vibevideo = &otio["metadata"]["vibevideo"];
        let fps = serde_json::from_value::<Rational>(vibevideo["fps"].clone())
            .ok()
            .or_else(|| otio["global_start_time"]["rate"].as_f64().map(Rational::from_fps))
            .or_else(|| first_item_rate(otio).map(Rational::from_fps))
            .unwrap_or(Rational::new(30, 1));

        let mut groups: HashMap<GroupKey, LinkGroupId> = HashMap::new();
        let mut foreign = Vec::new();
        let mut tracks = Vec::new();
        for otio_track in children(&otio["tracks"]) {
            if schema(otio_track) != "Track" {
                self.warn(format!("{} non supportato a livello di track", schema(otio_track)));
                continue;
            }
            let kind = match otio_track["kind"].as_str() {
                Some("Video") => TrackKind::Video,
                Some("Audio") => TrackKind::Audio,
                other => {
                    self.warn(format!("track di tipo {other:?} ignorata"));
                    continue;
                }
            };
            let mut track = Track::new(kind);
            track.muted = otio_track["enabled"] == false;
            let mut cursor = 0.0;
            for item in children(otio_track) {
                let start = to_frames(cursor, fps);
                let duration = match schema(item) {
                    // Non occupa tempo sulla track: si sovrappone alle vicine.
                    "Transition" => {
                        self.warn("transizione ignorata: taglio netto".into());
                        continue;
                    }
                    "Gap" => item_duration(item),
                    "Clip" => {
                        let (duration, clip) =
                            self.clip(item, kind, fps, start, cursor, &mut groups);
                        if let Some((clip, is_foreign)) = clip {
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
                        self.warn(format!("{other} non supportato: lasciato vuoto"));
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

        let resolution = serde_json::from_value::<(u32, u32)>(vibevideo["resolution"].clone())
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

    /// Durata occupata sulla track (anche se la clip non viene importata)
    /// e la clip, con `true` se non viene da VibeVideo.
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
            self.warn(format!("clip \"{name}\" senza durata: ignorata"));
            return (0.0, None);
        };
        let timeline_len = to_frames(cursor + duration, fps) - timeline_start;
        if item["enabled"] == false {
            self.warn(format!("clip \"{name}\" disattivata: ignorata"));
            return (duration, None);
        }
        if timeline_len < 1 {
            self.warn(format!("clip \"{name}\" più corta di un frame: ignorata"));
            return (duration, None);
        }
        let vibevideo = &item["metadata"]["vibevideo"];
        let mut effects = serde_json::from_value::<EffectStack>(vibevideo["effects"].clone())
            .unwrap_or_default();
        // Secondi nel media all'inizio della clip e fps del media, per
        // tradurre i keyframe degli effetti di altri editor.
        let (source, rate, source_offset, media_start) = match schema(reference) {
            "ExternalReference" => {
                let url = reference["target_url"].as_str().unwrap_or("");
                let Some(media_id) = self.media(url) else {
                    return (duration, None);
                };
                let meta = &self.project.media_pool[media_id].meta;
                if kind == TrackKind::Video && !meta.has_video {
                    self.warn(format!("clip \"{name}\": solo audio su una track video, ignorata"));
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
            "GeneratorReference" if reference["generator_kind"] == "SolidColor" => {
                if effects.color.is_none() {
                    let color = serde_json::from_value::<[f32; 4]>(
                        reference["parameters"]["color"].clone(),
                    )
                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                    effects.color = Some(Keyframed::constant(Rgba {
                        r: color[0],
                        g: color[1],
                        b: color[2],
                        a: color[3],
                    }));
                }
                (ClipSource::SolidColor, Rational::one(), 0, None)
            }
            other => {
                self.warn(format!("clip \"{name}\": riferimento {other} non supportato, ignorata"));
                return (duration, None);
            }
        };

        let resolve = &item["metadata"]["Resolve_OTIO"];
        let keyframe_rate =
            item["source_range"]["start_time"]["rate"].as_f64().unwrap_or(fps.as_f64());
        for effect in children_of(item, "effects") {
            self.effect(effect, &mut effects, media_start, keyframe_rate);
        }

        let group_key = vibevideo["linked_group"]
            .as_u64()
            .map(GroupKey::VibeVideo)
            .or_else(|| resolve["Link Group ID"].as_u64().map(GroupKey::Resolve));
        let is_foreign = group_key.is_none();
        let linked_group = group_key
            .map(|key| *groups.entry(key).or_insert_with(|| self.project.alloc_link_group_id()));
        let audio_stream_index = vibevideo["audio_stream_index"]
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
        };
        (duration, Some((clip, is_foreign)))
    }

    /// Porta in `effects` quel che si sa tradurre di un effetto OTIO; il
    /// resto va in `ignored_effects`, tranne gli effetti spenti o ai valori
    /// di default (Resolve li esporta tutti, per ogni clip).
    /// `media_start` sono i secondi nel media all'inizio della clip e l'fps
    /// del media; `rate` è quello dei keyframe di Resolve, in frame
    /// dall'inizio della clip.
    fn effect(
        &mut self,
        effect: &Value,
        effects: &mut EffectStack,
        media_start: Option<(f64, Rational)>,
        rate: f64,
    ) {
        let resolve = &effect["metadata"]["Resolve_OTIO"];
        if resolve.is_null() {
            let name = effect["effect_name"].as_str().unwrap_or_else(|| schema(effect));
            *self.ignored_effects.entry(name.to_owned()).or_default() += 1;
            return;
        }
        if resolve["Enabled"] == false {
            return;
        }
        let name = resolve["Effect Name"].as_str().unwrap_or("Resolve Effect");
        let mut untranslated = false;
        for parameter in children_of(resolve, "Parameters") {
            if name == "Fairlight Clip Volume and Fades" && parameter["Parameter ID"] == "volume" {
                resolve_volume(parameter, effects, media_start, rate);
            } else if !is_default_parameter(parameter) {
                untranslated = true;
            }
        }
        if untranslated {
            *self.ignored_effects.entry(name.to_owned()).or_default() += 1;
        }
    }

    /// Il media al `target_url`, sondato una volta sola per file.
    fn media(&mut self, url: &str) -> Option<MediaId> {
        let Some(path) = url_to_path(url, self.base_dir) else {
            self.warn(format!("{url}: indirizzo non supportato"));
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
            })),
            Err(e) => {
                self.warn(format!("{}: {e}", path.display()));
                None
            }
        };
        self.media.insert(path, media);
        media
    }

    /// Gli altri editor esportano video e audio dello stesso media come
    /// clip separate: si ricollegano quelle che coincidono in tutto.
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
                ClipSource::SolidColor => None,
            })
    }

    fn warn(&mut self, warning: String) {
        self.warnings.push(warning);
    }
}

fn is_default_parameter(parameter: &Value) -> bool {
    let no_keyframes = parameter["Key Frames"].as_object().is_none_or(|k| k.is_empty());
    no_keyframes && parameter["Parameter Value"] == parameter["Default Parameter Value"]
}

/// Il volume di Resolve è in dB, come `gain_db`; i keyframe diventano
/// keyframe lineari sul frame sorgente corrispondente.
fn resolve_volume(
    parameter: &Value,
    effects: &mut EffectStack,
    media_start: Option<(f64, Rational)>,
    rate: f64,
) {
    if let Some(db) = parameter["Parameter Value"].as_f64() {
        effects.gain_db.default = db as f32;
    }
    let (Some(keyframes), Some((start_secs, media_fps))) =
        (parameter["Key Frames"].as_object(), media_start)
    else {
        return;
    };
    for (frame, keyframe) in keyframes {
        let (Ok(frame), Some(db)) = (frame.parse::<f64>(), keyframe["Value"].as_f64()) else {
            continue;
        };
        let source_frame = ((start_secs + frame / rate) * media_fps.as_f64()).round();
        effects
            .gain_db
            .upsert(source_frame as FrameIdx, db as f32, Interpolation::Linear);
    }
}

/// Lo stream audio del media da cui Resolve prende i canali della clip,
/// se vengono tutti dallo stesso.
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

/// Il nome dello schema senza versione (`"Clip.2"` → `"Clip"`).
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

/// `(inizio, durata)` in secondi.
fn time_range(range: &Value) -> Option<(f64, f64)> {
    Some((seconds(&range["start_time"])?, seconds(&range["duration"])?))
}

/// Durata di un elemento non importato: `source_range`, oppure quella dei
/// figli (in sequenza per una track, la più lunga per uno stack).
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
    use crate::model::ClipId;
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
                .ok_or_else(|| "file non trovato".to_owned())
        }
    }

    fn span(clip: &Clip) -> (FrameIdx, FrameIdx, FrameIdx, Rational) {
        (clip.timeline_start, clip.source_offset, clip.timeline_len, clip.rate)
    }

    /// Export e import di un progetto VibeVideo restituiscono le stesse
    /// clip, anche divise a metà frame sorgente.
    #[test]
    fn a_vibevideo_export_imports_back_unchanged() {
        let media_meta = meta(Rational::new(30_000, 1001), 1000);
        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: "/tmp/a.mp4".into(),
            meta: media_meta.clone(),
            content_hash: 42,
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
        let tl = &mut project.timelines[timeline_id];
        tl.tracks[0].clips.push(video);
        tl.tracks[1].clips.push(color);
        tl.tracks[2].clips.push(audio);
        tl.tracks[2].muted = true;
        let mut split = crate::SplitClip::new(timeline_id, 0, ClipId(1), 500);
        crate::Command::apply(&mut split, &mut project);

        let otio = timeline_to_otio(&project, timeline_id);
        let mut probe = probe_from(vec![("/tmp/a.mp4", media_meta.clone())]);
        let imported = project_from_otio(&otio, Path::new("/"), &mut probe).unwrap();
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
        assert_eq!(back.tracks[0].clips[1].linked_group, None, "metà destra scollegata");
        let color = back.tracks[1].clips[0].effects.color.as_ref().unwrap().default;
        assert_eq!((color.r, color.g, color.b), (0.2, 0.4, 0.6));
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

    /// Sorgente a 24 fps da `start` frame per `duration`.
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
                // Media con timecode di partenza 01:00:00:00 a 24 fps.
                "available_range": range(86_400.0, 2400.0, 24.0),
            },
        })
    }

    /// File di un altro editor: `Clip.1`, tempi nel rate del media con
    /// timecode di partenza, transizioni, clip disattivate e media
    /// mancanti; video e audio dello stesso tratto vanno ricollegati.
    #[test]
    fn a_foreign_file_imports_with_warnings_for_what_is_skipped() {
        let fps = 24_000.0 / 1001.0;
        let otio = json!({
            "OTIO_SCHEMA": "SerializableCollection.1",
            "children": [{
                "OTIO_SCHEMA": "Timeline.1",
                "name": "Da Resolve",
                "global_start_time": rt(86_400.0, fps),
                "tracks": {
                    "OTIO_SCHEMA": "Stack.1",
                    "children": [
                        {
                            "OTIO_SCHEMA": "Track.1",
                            "kind": "Video",
                            "children": [
                                { "OTIO_SCHEMA": "Gap.1", "source_range": range(0.0, 24.0, fps) },
                                clip_1("uno", B_ROLL, 86_448.0, 48.0, true),
                                { "OTIO_SCHEMA": "Transition.1", "in_offset": rt(6.0, fps) },
                                clip_1("spenta", B_ROLL, 86_400.0, 24.0, false),
                                clip_1("persa", "file:///media/manca.mov", 86_400.0, 24.0, true),
                                clip_1("due", "b roll.mov", 86_400.0, 12.0, true),
                            ],
                        },
                        {
                            "OTIO_SCHEMA": "Track.1",
                            "kind": "Audio",
                            "enabled": false,
                            "children": [
                                { "OTIO_SCHEMA": "Gap.1", "source_range": range(0.0, 24.0, fps) },
                                clip_1("uno", B_ROLL, 86_448.0, 48.0, true),
                            ],
                        },
                    ],
                },
            }],
        });
        let mut probe = probe_from(vec![("/media/b roll.mov", meta(Rational::new(24, 1), 2400))]);
        let imported = project_from_otio(&otio, Path::new("/media"), &mut probe).unwrap();

        assert_eq!(imported.warnings.len(), 3, "{:?}", imported.warnings);
        assert!(imported.warnings[0].contains("transizione"));
        assert!(imported.warnings[1].contains("spenta"));
        assert!(imported.warnings[2].contains("manca.mov"));
        assert_eq!(imported.project.media_pool.len(), 1, "stesso file sondato una volta");

        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        assert_eq!(tl.fps, Rational::new(24_000, 1001));
        assert_eq!(tl.resolution, (1280, 720), "dal primo media video");
        let video = &tl.tracks[0].clips;
        assert_eq!(video.len(), 2);
        assert_eq!((video[0].timeline_start, video[0].timeline_len), (24, 48));
        assert_eq!(video[0].source_in(), 48, "dal timecode di partenza del media");
        assert_eq!(video[1].timeline_start, 24 + 48 + 24 + 24, "dopo spenta e persa");
        assert_eq!(video[1].source_in(), 0, "percorso relativo");

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

    /// Resolve esporta tutto lo stack di effetti di ogni clip: il volume
    /// diventa gain, spenti e default spariscono, il resto è riassunto in
    /// un avviso per effetto. Gruppi e stream audio vengono dai suoi
    /// metadati.
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
        let imported = project_from_otio(&otio, Path::new("/"), &mut probe).unwrap();

        assert_eq!(imported.warnings, ["effetto \"Zoom\" ignorato su 2 clip"]);
        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        let (video, audio) = (&tl.tracks[0].clips, &tl.tracks[1].clips);
        assert_eq!(audio[0].effects.gain_db.value_at(0), 3.5);
        let gain = &audio[1].effects.gain_db;
        assert_eq!(gain.value_at(24), -6.0, "keyframe sul primo frame sorgente della clip");
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
        }
    }

    /// Un media solo audio torna identico da un export nostro, si legge a
    /// qualunque rate da un altro editor e su una track video viene
    /// scartato con un avviso.
    #[test]
    fn audio_only_media_round_trips_and_is_refused_on_video_tracks() {
        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: "/tmp/voce.wav".into(),
            meta: audio_only_meta(),
            content_hash: 42,
        });
        let fps = Rational::new(25, 1);
        let timeline_id = project.timelines.insert(Timeline {
            name: "Voce".into(),
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

        let otio = timeline_to_otio(&project, timeline_id);
        let mut probe = probe_from(vec![("/tmp/voce.wav", audio_only_meta())]);
        let imported = project_from_otio(&otio, Path::new("/"), &mut probe).unwrap();
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
        let (_, back) = imported.project.timelines.iter().next().unwrap();
        let spans = |t: &Timeline| t.tracks[1].clips.iter().map(span).collect::<Vec<_>>();
        assert_eq!(spans(back), spans(&project.timelines[timeline_id]));

        let wav_clip = |start_samples: f64| {
            json!({
                "OTIO_SCHEMA": "Clip.2",
                "name": "voce",
                "source_range": range(start_samples, 48_000.0, 48_000.0),
                "media_references": { "DEFAULT_MEDIA": {
                    "OTIO_SCHEMA": "ExternalReference.1",
                    "target_url": "file:///tmp/voce.wav",
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
        let imported = project_from_otio(&foreign, Path::new("/"), &mut probe).unwrap();
        assert_eq!(imported.warnings.len(), 1, "{:?}", imported.warnings);
        assert!(imported.warnings[0].contains("solo audio"));
        let (_, tl) = imported.project.timelines.iter().next().unwrap();
        assert!(tl.tracks[0].clips.is_empty());
        let clip = &tl.tracks[1].clips[0];
        assert_eq!((clip.timeline_len, clip.source_offset), (25, 13), "1 s da 0,5 s, a 25 fps");
        assert_eq!(clip.rate, rate);
        assert_eq!(tl.resolution, (1920, 1080));
    }

    #[test]
    fn a_file_without_timelines_is_an_error() {
        let mut probe = probe_from(vec![]);
        let not_a_timeline = json!({ "OTIO_SCHEMA": "Clip.2" });
        let result = project_from_otio(&not_a_timeline, Path::new("/"), &mut probe);
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
