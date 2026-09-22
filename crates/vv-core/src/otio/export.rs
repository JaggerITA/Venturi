//! `source_range` is expressed at the timeline fps, like `source_offset`/
//! `timeline_len` (see `Clip`): exact even for conformed clips, where in
//! the media's fps the start would fall mid-frame. What OTIO cannot
//! represent (effects, linked groups, audio streams) ends up in
//! `metadata.venturi`.

use super::OtioError;
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
        .map(|(i, track)| track_to_otio(project, track, timeline.track_label(i), fps))
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
fn track_to_otio(project: &Project, track: &Track, name: String, fps: Rational) -> Value {
    let mut children = Vec::new();
    let mut cursor = 0;
    for clip in &track.clips {
        if clip.timeline_start > cursor {
            children.push(gap(clip.timeline_start - cursor, fps));
        }
        children.push(clip_to_otio(project, clip, fps));
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

fn clip_to_otio(project: &Project, clip: &Clip, fps: Rational) -> Value {
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

    json!({
        "OTIO_SCHEMA": "Clip.2",
        "name": name,
        "source_range": time_range(clip.source_offset, clip.timeline_len, fps),
        "effects": [],
        "markers": [],
        "enabled": !clip.disabled,
        "metadata": {
            "venturi": {
                "effects": clip.effects,
                "linked_group": clip.linked_group,
                "audio_stream_index": clip.audio_stream_index,
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

fn rational_time(value: FrameIdx, fps: Rational) -> Value {
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
