//! What the agent reads: projects, timelines and clips as JSON. Frames are
//! integers of the timeline (or, for `source_*`, of the media).

use serde_json::{Value, json};
use vv_core::{Clip, ClipSource, MediaId, MediaItem, Project, Rational, TimelineId, TrackKind};
use vv_session::Session;

use crate::ids::key_to_string;

/// What the user has in front of them in the editor.
pub struct GuiState {
    pub active_timeline: Option<TimelineId>,
    /// From the root timeline down to the compound clip being edited.
    pub timeline_stack: Vec<TimelineId>,
    pub playhead: vv_core::FrameIdx,
    pub selected_clips: Vec<vv_core::ClipId>,
    pub selected_media: Vec<MediaId>,
    /// Media previewed from the media pool instead of the timeline.
    pub previewed_media: Option<MediaId>,
}

pub fn state_json(session: &Session, state: &GuiState) -> Value {
    let project = &session.project;
    json!({
        "active_timeline": state.active_timeline.map(|id| timeline_json(project, id)),
        "timeline_stack": state.timeline_stack.iter().map(|&id| key_to_string(id)).collect::<Vec<_>>(),
        "playhead": state.playhead,
        "selected_clips": state.selected_clips.iter().map(|id| id.0.to_string()).collect::<Vec<_>>(),
        "selected_media": state.selected_media.iter().map(|&id| key_to_string(id)).collect::<Vec<_>>(),
        "previewed_media": state.previewed_media.map(key_to_string),
        "project_path": session.path(),
        "unsaved": session.has_unsaved_changes(),
    })
}

pub(crate) fn fps_json(fps: Rational) -> Value {
    json!({ "num": fps.num, "den": fps.den, "value": fps.as_f64() })
}

pub(crate) fn project_json(session: &Session) -> Value {
    let project = &session.project;
    json!({
        "path": session.path(),
        "unsaved": session.has_unsaved_changes(),
        "media": project.media_pool.keys().map(|id| media_json(project, id)).collect::<Vec<_>>(),
        "timelines": project.timelines.keys().map(|id| timeline_json(project, id)).collect::<Vec<_>>(),
        "folders": project.folders.iter().map(|(id, folder)| json!({
            "id": key_to_string(id),
            "name": folder.name,
            "parent": folder.parent.map(key_to_string),
        })).collect::<Vec<_>>(),
    })
}

fn media_kind(item: &MediaItem) -> &'static str {
    if item.compound.is_some() {
        "timeline"
    } else if item.meta.is_image() {
        "image"
    } else if item.meta.has_video {
        "video"
    } else {
        "audio"
    }
}

pub(crate) fn media_json(project: &Project, id: MediaId) -> Value {
    let item = &project.media_pool[id];
    let meta = &item.meta;
    let mut value = json!({
        "id": key_to_string(id),
        "name": vv_session::file_label(&item.path),
        "kind": media_kind(item),
        "fps": fps_json(meta.fps),
        "duration_frames": meta.duration_frames,
        "duration_secs": meta.duration_frames as f64 / meta.fps.as_f64().max(1e-9),
        "audio_streams": if meta.has_audio { meta.audio_stream_count() } else { 0 },
        "folder": item.folder.map(key_to_string),
    });
    match item.compound {
        Some(timeline) => value["timeline_id"] = json!(key_to_string(timeline)),
        None => {
            value["path"] = json!(item.path);
            value["offline"] = json!(!item.path.exists());
        }
    }
    if meta.has_video {
        value["resolution"] = json!([meta.width, meta.height]);
    }
    value
}

pub(crate) fn timeline_json(project: &Project, id: TimelineId) -> Value {
    let timeline = &project.timelines[id];
    let length = timeline.total_frames();
    json!({
        "id": key_to_string(id),
        "name": timeline.name,
        "fps": fps_json(timeline.fps),
        "resolution": [timeline.resolution.0, timeline.resolution.1],
        "length_frames": length,
        "length_secs": length as f64 / timeline.fps.as_f64().max(1e-9),
        "video_tracks": timeline.tracks_of_kind(TrackKind::Video).count(),
        "audio_tracks": timeline.tracks_of_kind(TrackKind::Audio).count(),
    })
}

pub(crate) fn track_name(project: &Project, timeline: TimelineId, track_index: usize) -> String {
    let timeline = &project.timelines[timeline];
    let prefix = match timeline.tracks[track_index].kind {
        TrackKind::Video => "V",
        TrackKind::Audio => "A",
    };
    format!("{prefix}{}", timeline.track_number(track_index))
}

/// Tracks top to bottom as the agent addresses them, with their clips.
pub(crate) fn timeline_detail_json(project: &Project, id: TimelineId) -> Value {
    let timeline = &project.timelines[id];
    let mut value = timeline_json(project, id);
    value["tracks"] = timeline
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            json!({
                "name": track_name(project, id, index),
                "kind": match track.kind { TrackKind::Video => "video", TrackKind::Audio => "audio" },
                "muted": track.muted,
                "solo": track.solo,
                "locked": track.locked,
                "clips": track.clips.iter().map(|clip| clip_json(project, id, index, clip)).collect::<Vec<_>>(),
            })
        })
        .collect();
    value["markers"] = timeline
        .markers
        .iter()
        .map(|m| {
            json!({
                "id": m.id.0.to_string(),
                "start": m.start,
                "duration": m.duration,
                "note": m.note,
            })
        })
        .collect();
    value
}

pub(crate) fn clip_json(
    project: &Project,
    timeline: TimelineId,
    track_index: usize,
    clip: &Clip,
) -> Value {
    let mut value = json!({
        "id": clip.id.0.to_string(),
        "track": track_name(project, timeline, track_index),
        "start": clip.timeline_start,
        "end": clip.timeline_end(),
        "length": clip.timeline_len,
        "link_group": clip.linked_group.map(|g| g.0.to_string()),
        "disabled": clip.disabled,
        "fade_in": clip.fade_in,
        "fade_out": clip.fade_out,
        "effects": effect_names(clip),
    });
    value["source"] = match &clip.source {
        ClipSource::Media(media_id) => {
            let name = project
                .media_pool
                .get(*media_id)
                .map(|item| vv_session::file_label(&item.path));
            json!({ "type": "media", "media_id": key_to_string(*media_id), "name": name })
        }
        ClipSource::SolidColor => json!({ "type": "solid_color" }),
        ClipSource::Text => json!({
            "type": "title",
            "text": clip.effects.title.as_ref().map(|t| t.content.clone()),
        }),
        ClipSource::Adjustment => json!({ "type": "adjustment" }),
    };
    if let ClipSource::Media(_) = clip.source {
        value["source_in"] = json!(clip.source_in());
        value["source_out"] = json!(clip.source_out());
        if project.timelines[timeline].tracks[track_index].kind == TrackKind::Audio {
            value["audio_stream"] = json!(clip.audio_stream_index);
        }
    }
    if clip.speed != Rational::one() {
        value["speed"] = json!(clip.speed.as_f64());
    }
    value
}

/// The effects that differ from a fresh clip; `get_clip` has their values.
fn effect_names(clip: &Clip) -> Vec<&'static str> {
    let effects = &clip.effects;
    let mut names = Vec::new();
    if !effects.transform.is_pristine() {
        names.push("transform");
    }
    if !effects.gain_db.is_constant() || effects.gain_db.default != 0.0 {
        names.push("gain");
    }
    if !effects.filters.is_empty() {
        names.push("filters");
    }
    if effects.transition_in.is_some() {
        names.push("transition_in");
    }
    if effects.transition_out.is_some() {
        names.push("transition_out");
    }
    if effects.blend_mode != Default::default() {
        names.push("blend_mode");
    }
    names
}

/// Everything about a clip, effects in full.
pub(crate) fn clip_detail_json(
    project: &Project,
    timeline: TimelineId,
    track_index: usize,
    clip: &Clip,
) -> Value {
    let mut value = clip_json(project, timeline, track_index, clip);
    value["effects"] = serde_json::to_value(&clip.effects).unwrap_or(Value::Null);
    value
}
