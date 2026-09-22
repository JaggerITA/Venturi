//! `source_range` is expressed at the timeline fps, like `source_offset`/
//! `timeline_len` (see `Clip`): exact even for conformed clips, where in
//! the media's fps the start would fall mid-frame. What OTIO cannot
//! represent (linked groups, audio streams, the parts of the transform
//! Resolve does not read) ends up in `metadata.venturi`; the transform and
//! the transitions also travel in Resolve's own namespace (see `resolve`).

use super::OtioError;
use super::resolve::{self, Scale};
use crate::FadeEdge;
use crate::model::{Clip, ClipSource, FrameIdx, Project, Rational, TimelineId, Track, TrackKind};
use serde_json::{Value, json};
use std::path::Path;

pub fn export_otio(project: &Project, timeline: TimelineId, path: &Path) -> Result<(), OtioError> {
    let contents = serde_json::to_string_pretty(&timeline_to_otio(project, timeline))?;
    std::fs::write(path, contents)?;
    Ok(())
}

pub fn timeline_to_otio(project: &Project, timeline_id: TimelineId) -> Value {
    let timeline = &project.timelines[timeline_id];
    let fps = timeline.fps;
    let tracks: Vec<Value> = timeline
        .tracks
        .iter()
        .enumerate()
        .map(|(i, track)| {
            track_to_otio(project, track, timeline.track_label(i), fps, timeline.resolution)
        })
        .collect();

    json!({
        "OTIO_SCHEMA": "Timeline.1",
        "name": timeline.name,
        "global_start_time": rational_time(0, fps),
        "metadata": {
            "venturi": {
                "fps": timeline.fps,
                "resolution": timeline.resolution,
            }
        },
        "tracks": {
            "OTIO_SCHEMA": "Stack.1",
            "name": "tracks",
            "source_range": null,
            "effects": [],
            "markers": [],
            "enabled": true,
            "metadata": {},
            "children": tracks,
        },
    })
}

/// OTIO wants sequential tracks: the holes between clips become `Gap`s.
fn track_to_otio(
    project: &Project,
    track: &Track,
    name: String,
    fps: Rational,
    resolution: (u32, u32),
) -> Value {
    let mut children = Vec::new();
    let mut cursor = 0;
    for clip in &track.clips {
        if clip.timeline_start > cursor {
            children.push(gap(clip.timeline_start - cursor, fps));
        }
        if let Some(transition) = &clip.effects.transition_in {
            children.push(resolve::transition_to_otio(transition, FadeEdge::In, fps));
        }
        children.push(clip_to_otio(project, clip, track.kind, fps, resolution));
        if let Some(transition) = &clip.effects.transition_out {
            children.push(resolve::transition_to_otio(transition, FadeEdge::Out, fps));
        }
        cursor = clip.timeline_end();
    }
    json!({
        "OTIO_SCHEMA": "Track.1",
        "name": name,
        "kind": match track.kind {
            TrackKind::Video => "Video",
            TrackKind::Audio => "Audio",
        },
        "source_range": null,
        "effects": [],
        "markers": [],
        "enabled": !track.muted,
        "metadata": {},
        "children": children,
    })
}

fn clip_to_otio(
    project: &Project,
    clip: &Clip,
    kind: TrackKind,
    fps: Rational,
    resolution: (u32, u32),
) -> Value {
    let (name, media_reference) = match &clip.source {
        ClipSource::Media(media_id) => match project.media_pool.get(*media_id) {
            Some(item) => (
                item.path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                json!({
                    "OTIO_SCHEMA": "ExternalReference.1",
                    "name": "",
                    "target_url": file_url(&item.path),
                    "available_range": time_range(0, item.meta.duration_frames, item.meta.fps),
                    "available_image_bounds": null,
                    "metadata": {},
                }),
            ),
            None => (
                String::new(),
                json!({
                    "OTIO_SCHEMA": "MissingReference.1",
                    "name": "",
                    "available_range": null,
                    "available_image_bounds": null,
                    "metadata": {},
                }),
            ),
        },
        ClipSource::SolidColor => {
            let color = clip.effects.color.as_ref().map(|c| c.default);
            (
                "Colore".to_owned(),
                json!({
                    "OTIO_SCHEMA": "GeneratorReference.1",
                    "name": "",
                    "generator_kind": "SolidColor",
                    "parameters": { "color": color.map(|c| [c.r, c.g, c.b, c.a]) },
                    "available_range": null,
                    "available_image_bounds": null,
                    "metadata": {},
                }),
            )
        }
        ClipSource::Text => {
            let content = clip.effects.title.as_ref().map(|t| t.content.clone());
            (
                content.clone().unwrap_or_default(),
                json!({
                    "OTIO_SCHEMA": "GeneratorReference.1",
                    "name": "",
                    "generator_kind": "Text",
                    "parameters": { "text": content },
                    "available_range": null,
                    "available_image_bounds": null,
                    "metadata": {},
                }),
            )
        }
    };

    let frame = (resolution.0 as f32, resolution.1 as f32);
    let media = match &clip.source {
        ClipSource::Media(id) => project
            .media_pool
            .get(*id)
            .map_or(frame, |m| (m.meta.width as f32, m.meta.height as f32)),
        ClipSource::SolidColor | ClipSource::Text => frame,
    };
    let scale = Scale::new(media, frame);

    json!({
        "OTIO_SCHEMA": "Clip.2",
        "name": name,
        "source_range": time_range(clip.source_offset, clip.timeline_len, fps),
        "effects": resolve::clip_effects(clip, kind, &scale),
        "markers": [],
        "enabled": !clip.disabled,
        "metadata": {
            "venturi": {
                "effects": clip.effects,
                "linked_group": clip.linked_group,
                "audio_stream_index": clip.audio_stream_index,
                "fade_in": clip.fade_in,
                "fade_out": clip.fade_out,
            }
        },
        "media_references": { "DEFAULT_MEDIA": media_reference },
        "active_media_reference_key": "DEFAULT_MEDIA",
    })
}

fn gap(len: FrameIdx, fps: Rational) -> Value {
    json!({
        "OTIO_SCHEMA": "Gap.1",
        "name": "",
        "source_range": time_range(0, len, fps),
        "effects": [],
        "markers": [],
        "enabled": true,
        "metadata": {},
    })
}

pub(super) fn rational_time(value: FrameIdx, fps: Rational) -> Value {
    json!({ "OTIO_SCHEMA": "RationalTime.1", "rate": fps.as_f64(), "value": value as f64 })
}

fn time_range(start: FrameIdx, duration: FrameIdx, fps: Rational) -> Value {
    json!({
        "OTIO_SCHEMA": "TimeRange.1",
        "start_time": rational_time(start, fps),
        "duration": rational_time(duration, fps),
    })
}

fn file_url(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut url = String::from("file://");
    for byte in absolute.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn project() -> (Project, TimelineId, MediaId) {
        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: "/tmp/un video.mp4".into(),
            meta: MediaMeta {
                duration_frames: 1000,
                fps: Rational::new(30_000, 1001),
                width: 1920,
                height: 1080,
                has_video: true,
                has_audio: true,
                sample_rate: 48_000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 1,
            compound: None,
        });
        let timeline = project.timelines.insert(Timeline {
            name: "Timeline 1".into(),
            fps: Rational::new(30, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track::new(TrackKind::Video),
                Track::new(TrackKind::Video),
                Track::new(TrackKind::Audio),
            ],
        });
        (project, timeline, media)
    }

    #[test]
    fn exports_tracks_in_compositing_order_with_gaps_between_clips() {
        let (mut project, timeline_id, media) = project();
        let rate = Rational::conform_rate(Rational::new(30, 1), Rational::new(30_000, 1001));
        let tl = &mut project.timelines[timeline_id];
        tl.tracks[0].clips.push(Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(media),
            100,
            400,
            30,
            rate,
        ));
        let mut color =
            Clip::from_source_range(ClipId(2), ClipSource::SolidColor, 0, 60, 0, Rational::one());
        color.effects.color = Some(Keyframed::constant(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }));
        tl.tracks[1].clips.push(color);
        tl.tracks[2].muted = true;

        let otio = timeline_to_otio(&project, timeline_id);
        let tracks = otio["tracks"]["children"].as_array().unwrap();
        let names: Vec<&str> = tracks.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["V1", "V2", "A1"]);
        assert_eq!(tracks[2]["kind"], "Audio");
        assert_eq!(tracks[2]["enabled"], false, "track mutata");

        let v1 = tracks[0]["children"].as_array().unwrap();
        assert_eq!(v1[0]["OTIO_SCHEMA"], "Gap.1");
        assert_eq!(v1[0]["source_range"]["duration"]["value"], 30.0);
        let clip = &v1[1];
        assert_eq!(clip["name"], "un video.mp4");
        assert_eq!(clip["source_range"]["start_time"]["value"], 100.0);
        assert_eq!(clip["source_range"]["start_time"]["rate"], 30.0);
        assert_eq!(clip["source_range"]["duration"]["value"], 300.0);
        let reference = &clip["media_references"]["DEFAULT_MEDIA"];
        assert_eq!(reference["target_url"], "file:///tmp/un%20video.mp4");
        assert_eq!(reference["available_range"]["duration"]["value"], 1000.0);

        let generator = &tracks[1]["children"][0]["media_references"]["DEFAULT_MEDIA"];
        assert_eq!(generator["generator_kind"], "SolidColor");
        assert_eq!(generator["parameters"]["color"], json!([1.0, 0.0, 0.0, 1.0]));
    }

    /// After a split mid source frame the right half starts where the left
    /// one ends, both on the timeline and in the media.
    #[test]
    fn a_split_conformed_clip_exports_contiguous_source_ranges() {
        let (mut project, timeline_id, media) = project();
        let rate = Rational::conform_rate(Rational::new(30, 1), Rational::new(30_000, 1001));
        project.timelines[timeline_id].tracks[0].clips.push(Clip::from_source_range(
            ClipId(1),
            ClipSource::Media(media),
            0,
            1000,
            0,
            rate,
        ));
        let mut split = crate::SplitClip::new(timeline_id, 0, ClipId(1), 500);
        crate::Command::apply(&mut split, &mut project);

        let otio = timeline_to_otio(&project, timeline_id);
        let v1 = otio["tracks"]["children"][0]["children"].as_array().unwrap();
        assert_eq!(v1.len(), 2, "nessun gap tra le due metà");
        let start = |i: usize| v1[i]["source_range"]["start_time"]["value"].as_f64().unwrap();
        let len = |i: usize| v1[i]["source_range"]["duration"]["value"].as_f64().unwrap();
        assert_eq!(start(0) + len(0), start(1));
        assert_eq!(len(0) + len(1), 1001.0);
    }

    /// Parameter IDs, units and shape copied from a file exported by
    /// Resolve: this is the only thing its importer reads back.
    #[test]
    fn exports_the_transform_as_resolve_effects() {
        let (mut project, timeline_id, media) = project();
        let mut clip =
            Clip::from_source_range(ClipId(1), ClipSource::Media(media), 0, 100, 0, Rational::one());
        let t = &mut clip.effects.transform;
        t.track_mut(TransformParam::ZoomX).default = 1.07;
        t.track_mut(TransformParam::PositionX).default = 96.0;
        t.track_mut(TransformParam::PositionY).default = -54.0;
        t.track_mut(TransformParam::Rotation).default = 7.8;
        t.track_mut(TransformParam::AnchorX).default = 192.0;
        t.track_mut(TransformParam::CropTop).default = 108.0;
        t.track_mut(TransformParam::Opacity).default = 80.0;
        t.flip = [true, false];
        clip.effects.speed = Keyframed::constant(1.705_748_4);
        clip.fade_in = 12;
        project.timelines[timeline_id].tracks[0].clips.push(clip);

        let otio = timeline_to_otio(&project, timeline_id);
        let effects = otio["tracks"]["children"][0]["children"][0]["effects"].as_array().unwrap();
        let named = |name: &str| {
            effects
                .iter()
                .find(|e| e["metadata"]["Resolve_OTIO"]["Effect Name"] == name)
                .unwrap_or_else(|| panic!("nessun effetto {name}"))
                .clone()
        };
        let raw = |effect: &Value, id: &str| {
            effect["metadata"]["Resolve_OTIO"]["Parameters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["Parameter ID"] == id)
                .unwrap_or_else(|| panic!("nessun parametro {id}"))["Parameter Value"]
                .clone()
        };
        // The values are f32 widened to f64: comparing them exactly would
        // only measure the widening.
        let value = |effect: &Value, id: &str| {
            (raw(effect, id).as_f64().unwrap() * 1e6).round() / 1e6
        };

        let transform = named("Transform");
        assert_eq!(transform["effect_name"], "Resolve Effect");
        assert_eq!(value(&transform, "transformationZoomX"), 1.07);
        assert_eq!(value(&transform, "transformationPan"), 0.05, "96 px su 1920");
        assert_eq!(value(&transform, "transformationTilt"), -0.05, "-54 px su 1080");
        assert_eq!(value(&transform, "transformationRotationAngle"), -7.8, "verso opposto");
        let anchor = raw(&transform, "transformationAnchorPoint");
        assert!((anchor[0].as_f64().unwrap() - 0.1).abs() < 1e-6);
        assert_eq!(anchor[1].as_f64().unwrap(), 0.0);
        assert!(
            transform["metadata"]["Resolve_OTIO"]["Parameters"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["Parameter ID"] != "transformationZoomY"),
            "i parametri al default sono omessi, come fa Resolve"
        );
        assert_eq!(raw(&transform, "transformationFlipX"), true);
        assert!(
            transform["metadata"]["Resolve_OTIO"]["Parameters"]
                .as_array()
                .unwrap()
                .iter()
                .all(|p| p["Parameter ID"] != "transformationFlipY"),
            "il flip non attivo è omesso"
        );
        assert_eq!(value(&named("Cropping"), "cropTop"), 0.1);
        assert_eq!(value(&named("Composite"), "opacity"), 80.0);
        assert_eq!(value(&named("Video Faders"), "videoFaderIn"), 12.0);

        // The speed is the one thing Resolve reads from the standard schema.
        let warp = &effects[0];
        assert_eq!(warp["OTIO_SCHEMA"], "LinearTimeWarp.1");
        assert_eq!((warp["time_scalar"].as_f64().unwrap() * 1e6).round() / 1e6, 1.705748);
    }

    #[test]
    fn exports_the_keyframes_on_the_timeline_frames_of_the_clip() {
        let (mut project, timeline_id, media) = project();
        let mut clip =
            Clip::from_source_range(ClipId(1), ClipSource::Media(media), 30, 130, 0, Rational::one());
        clip.effects
            .transform
            .track_mut(TransformParam::ZoomX)
            .upsert(40, 2.0, Interpolation::Linear);
        project.timelines[timeline_id].tracks[0].clips.push(clip);

        let otio = timeline_to_otio(&project, timeline_id);
        let keys = &otio["tracks"]["children"][0]["children"][0]["effects"][0]["metadata"]
            ["Resolve_OTIO"]["Parameters"][0]["Key Frames"];
        assert_eq!(keys["10"]["Value"], 2.0, "frame 40 della sorgente, decimo della clip");
    }

    /// A vertical clip in a horizontal timeline: Resolve measures the pan
    /// on the clip as it is fitted into the frame, so a shift of 700 px on
    /// a 1080x2400 source that lands 486 px wide is far more than 700/1920.
    #[test]
    fn normalizes_the_position_on_the_clip_not_on_the_frame() {
        let mut project = Project::default();
        let media = project.media_pool.insert(MediaItem {
            path: "/tmp/verticale.mp4".into(),
            meta: MediaMeta {
                duration_frames: 1000,
                fps: Rational::new(30, 1),
                width: 1080,
                height: 2400,
                has_video: true,
                has_audio: false,
                sample_rate: 48_000,
                channels: 2,
                audio_streams: 1,
            },
            content_hash: 2,
            compound: None,
        });
        let timeline_id = project.timelines.insert(Timeline {
            name: "Verticale".into(),
            fps: Rational::new(30, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video)],
        });
        let mut clip =
            Clip::from_source_range(ClipId(1), ClipSource::Media(media), 0, 100, 0, Rational::one());
        clip.effects.transform.track_mut(TransformParam::PositionX).default = -700.0;
        project.timelines[timeline_id].tracks[0].clips.push(clip);

        let otio = timeline_to_otio(&project, timeline_id);
        let pan = otio["tracks"]["children"][0]["children"][0]["effects"][0]["metadata"]
            ["Resolve_OTIO"]["Parameters"][0]["Parameter Value"]
            .as_f64()
            .unwrap();
        assert_eq!((pan * 1e4).round() / 1e4, -1.4403, "-700 su 486 px di clip");
        assert_eq!(
            (pan * 1920.0).round(),
            -2765.0,
            "il valore che l'inspector di Resolve mostra in pixel"
        );
    }

    #[test]
    fn exports_a_transition_as_an_item_that_takes_no_time() {
        let (mut project, timeline_id, media) = project();
        let mut clip =
            Clip::from_source_range(ClipId(1), ClipSource::Media(media), 0, 100, 0, Rational::one());
        clip.effects.transition_in = Some(Transition {
            kind: TransitionKind::Push,
            duration: 24,
            direction: PushDirection::Left,
            ease: Ease::InOut,
            curve: 0.5,
        });
        project.timelines[timeline_id].tracks[0].clips.push(clip);

        let otio = timeline_to_otio(&project, timeline_id);
        let children = otio["tracks"]["children"][0]["children"].as_array().unwrap();
        assert_eq!(children.len(), 2);
        let transition = &children[0];
        assert_eq!(transition["OTIO_SCHEMA"], "Transition.1");
        assert_eq!(transition["in_offset"]["value"], 0.0);
        assert_eq!(transition["out_offset"]["value"], 24.0, "entra nella clip che segue");
        assert_eq!(transition["metadata"]["Resolve_OTIO"]["Transition Type"], "Push");
        assert_eq!(transition["metadata"]["venturi"]["transition"]["direction"], "Left");

        // Without the progress curve Resolve holds the transition at 0.
        let curve = &transition["metadata"]["Resolve_OTIO"]["Effects"]["Parameters"][1];
        assert_eq!(curve["Parameter ID"], "transitionCustomCurvesKeyframes");
        let keys = &curve["Key Frames"];
        assert_eq!(keys["0"]["Value"], 0.0);
        assert_eq!(keys["24"]["Value"], 1.0);
        let handle = |key: &Value, name: &str| {
            (key[name][name][0].as_f64().unwrap() * 1e3).round() / 1e3
        };
        assert_eq!(handle(&keys["0"], "OutBez"), 7.2, "0.5 * 0.6 * 24");
        assert_eq!(handle(&keys["24"], "InBez"), -7.2);
    }

    #[test]
    fn keeps_what_otio_cannot_represent_in_metadata() {
        let (mut project, timeline_id, media) = project();
        let mut clip =
            Clip::from_source_range(ClipId(1), ClipSource::Media(media), 0, 10, 0, Rational::one());
        clip.audio_stream_index = 1;
        clip.linked_group = Some(LinkGroupId(7));
        clip.effects.gain_db = Keyframed::constant(-6.0);
        project.timelines[timeline_id].tracks[2].clips.push(clip);

        let otio = timeline_to_otio(&project, timeline_id);
        let meta = &otio["tracks"]["children"][2]["children"][0]["metadata"]["venturi"];
        assert_eq!(meta["audio_stream_index"], 1);
        assert_eq!(meta["linked_group"], 7);
        assert_eq!(meta["effects"]["gain_db"]["default"], -6.0);
    }
}
