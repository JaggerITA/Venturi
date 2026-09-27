use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use vv_core::{MediaId, MediaItem, Project, Rational, Timeline, TimelineId, Track, TrackKind};
use vv_session::{JobId, Session, SessionEvent};

use crate::ids::{self, key_to_string};
use crate::tools::*;

pub enum Dispatch {
    Handled(ToolResult),
    /// Waits for background work: the host feeds every `SessionEvent` to
    /// `Pending::resolve` until it returns the result.
    Deferred(Pending),
}

pub enum Pending {
    Import { job: JobId, paths: Vec<PathBuf> },
}

impl Pending {
    pub fn resolve(&self, session: &Session, event: &SessionEvent) -> Option<ToolResult> {
        match (self, event) {
            (
                Pending::Import { job, paths },
                SessionEvent::ImportFinished {
                    job: finished,
                    errors,
                    ..
                },
            ) if job == finished => {
                // Files already in the pool are reported too: the agent
                // needs their ids either way.
                let media: Vec<Value> = paths
                    .iter()
                    .filter_map(|path| session.media_with_path(path))
                    .map(|id| media_json(&session.project, id))
                    .collect();
                Some(Ok(ToolOutput::json(
                    json!({ "media": media, "errors": errors }),
                )))
            }
            _ => None,
        }
    }
}

pub fn dispatch(session: &mut Session, call: ToolCall) -> Dispatch {
    if let ToolCall::ImportMedia(args) = call {
        return import_media(session, args);
    }
    Dispatch::Handled(run(session, call))
}

fn run(session: &mut Session, call: ToolCall) -> ToolResult {
    match call {
        ToolCall::GetProject => Ok(ToolOutput::json(project_json(session))),
        ToolCall::NewProject => {
            session.new_project();
            Ok(ToolOutput::json(project_json(session)))
        }
        ToolCall::OpenProject(args) => {
            session
                .open(Path::new(&args.path))
                .map_err(|e| ToolError(format!("cannot open {}: {e}", args.path)))?;
            Ok(ToolOutput::json(project_json(session)))
        }
        ToolCall::SaveProject(args) => {
            let path = match (args.path, session.path()) {
                (Some(path), _) => PathBuf::from(path),
                (None, Some(current)) => current.to_path_buf(),
                (None, None) => {
                    return Err(ToolError("the project has no file yet: pass `path`".into()));
                }
            };
            session
                .save_to(&path)
                .map_err(|e| ToolError(format!("cannot save to {}: {e}", path.display())))?;
            Ok(ToolOutput::json(json!({ "path": path })))
        }
        ToolCall::CreateTimeline(args) => create_timeline(session, args),
        ToolCall::Undo => {
            let position = session.history.position();
            let Some(label) = position
                .checked_sub(1)
                .and_then(|last| session.history.labels().nth(last))
            else {
                return Err(ToolError("nothing to undo".into()));
            };
            session.history.undo(&mut session.project);
            Ok(ToolOutput::json(json!({ "undone": format!("{label:?}") })))
        }
        ToolCall::Redo => {
            let position = session.history.position();
            let Some(label) = session.history.labels().nth(position) else {
                return Err(ToolError("nothing to redo".into()));
            };
            session.history.redo(&mut session.project);
            Ok(ToolOutput::json(json!({ "redone": format!("{label:?}") })))
        }
        ToolCall::ImportMedia(_) => Err(ToolError("import_media is deferred".into())),
    }
}

fn import_media(session: &mut Session, args: ImportMediaArgs) -> Dispatch {
    if args.paths.is_empty() {
        return Dispatch::Handled(Err(ToolError("no paths given".into())));
    }
    let paths: Vec<PathBuf> = args.paths.iter().map(PathBuf::from).collect();
    let job = session.import_media(paths.clone());
    Dispatch::Deferred(Pending::Import { job, paths })
}

fn create_timeline(session: &mut Session, args: CreateTimelineArgs) -> ToolResult {
    let mut fps = Rational::new(25, 1);
    let mut resolution = (1920, 1080);
    if let Some(media) = &args.from_media {
        let meta = &session.project.media_pool[ids::media_id(&session.project, media)?].meta;
        fps = meta.fps;
        if meta.has_video {
            resolution = (meta.width, meta.height);
        }
    }
    if let Some([num, den]) = args.fps {
        if num <= 0 || den <= 0 {
            return Err(ToolError(format!("invalid fps {num}/{den}")));
        }
        fps = Rational::new(num, den);
    }
    if let Some([width, height]) = args.resolution {
        if width == 0 || height == 0 {
            return Err(ToolError(format!("invalid resolution {width}x{height}")));
        }
        resolution = (width, height);
    }
    let id = session.create_timeline(Timeline {
        name: args.name,
        fps,
        resolution,
        tracks: vec![Track::new(TrackKind::Video), Track::new(TrackKind::Audio)],
        markers: Vec::new(),
    });
    Ok(ToolOutput::json(timeline_json(&session.project, id)))
}

fn fps_json(fps: Rational) -> Value {
    json!({ "num": fps.num, "den": fps.den, "value": fps.as_f64() })
}

fn project_json(session: &Session) -> Value {
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

fn media_json(project: &Project, id: MediaId) -> Value {
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

fn timeline_json(project: &Project, id: TimelineId) -> Value {
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

#[cfg(test)]
#[path = "tests/dispatch.rs"]
pub(crate) mod tests;
