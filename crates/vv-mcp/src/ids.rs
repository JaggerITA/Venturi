//! Ids as the agent sees them: decimal strings. Slotmap keys go through
//! their FFI form, whose u64 would not survive a JSON number in JS clients.

use slotmap::{Key, KeyData};
use vv_core::edit::ClipRef;
use vv_core::{ClipId, MarkerId, MediaId, Project, TimelineId, TrackKind};

use crate::ToolError;

pub(crate) fn key_to_string(key: impl Key) -> String {
    key.data().as_ffi().to_string()
}

fn parse_u64(id: &str, what: &str) -> Result<u64, ToolError> {
    id.trim()
        .parse::<u64>()
        .map_err(|_| ToolError(format!("malformed {what} id \"{id}\"")))
}

pub(crate) fn media_id(project: &Project, id: &str) -> Result<MediaId, ToolError> {
    let key: MediaId = KeyData::from_ffi(parse_u64(id, "media")?).into();
    project
        .media_pool
        .contains_key(key)
        .then_some(key)
        .ok_or_else(|| ToolError(format!("unknown media id \"{id}\"")))
}

pub fn timeline_id(project: &Project, id: &str) -> Result<TimelineId, ToolError> {
    let key: TimelineId = KeyData::from_ffi(parse_u64(id, "timeline")?).into();
    project
        .timelines
        .contains_key(key)
        .then_some(key)
        .ok_or_else(|| ToolError(format!("unknown timeline id \"{id}\"")))
}

/// Clips are addressed by id alone: their track can change under the
/// agent's feet.
pub(crate) fn clip_ref(
    project: &Project,
    timeline: TimelineId,
    id: &str,
) -> Result<ClipRef, ToolError> {
    let clip_id = ClipId(parse_u64(id, "clip")?);
    project.timelines[timeline]
        .tracks
        .iter()
        .position(|track| track.clips.iter().any(|c| c.id == clip_id))
        .map(|track_index| (track_index, clip_id))
        .ok_or_else(|| ToolError(format!("no clip \"{id}\" in this timeline")))
}

pub(crate) fn clip_refs(
    project: &Project,
    timeline: TimelineId,
    ids: &[String],
) -> Result<Vec<ClipRef>, ToolError> {
    if ids.is_empty() {
        return Err(ToolError("no clip ids given".into()));
    }
    ids.iter()
        .map(|id| clip_ref(project, timeline, id))
        .collect()
}

/// "V1", "a2"... → track index.
pub(crate) fn track_index(
    project: &Project,
    timeline: TimelineId,
    name: &str,
) -> Result<usize, ToolError> {
    let bad = || {
        ToolError(format!(
            "no track \"{name}\": tracks are named V1, V2, ..., A1, A2, ..."
        ))
    };
    let name = name.trim();
    let kind = match name.chars().next().map(|c| c.to_ascii_uppercase()) {
        Some('V') => TrackKind::Video,
        Some('A') => TrackKind::Audio,
        _ => return Err(bad()),
    };
    let number: usize = name[1..].parse().map_err(|_| bad())?;
    project.timelines[timeline]
        .track_of_kind_numbered(kind, number)
        .ok_or_else(bad)
}

pub(crate) fn marker_id(
    project: &Project,
    timeline: TimelineId,
    id: &str,
) -> Result<MarkerId, ToolError> {
    let marker = MarkerId(parse_u64(id, "marker")?);
    project.timelines[timeline]
        .marker(marker)
        .map(|_| marker)
        .ok_or_else(|| ToolError(format!("no marker \"{id}\" in this timeline")))
}

pub(crate) fn check_revision(
    project: &Project,
    timeline: TimelineId,
    expected: Option<&str>,
) -> Result<(), ToolError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let current = crate::json::timeline_revision(project, timeline);
    if expected.trim() != current {
        return Err(ToolError(format!(
            "the timeline changed since you read it (revision {expected}, now {current}): \
             read it again with get_timeline before editing"
        )));
    }
    Ok(())
}
