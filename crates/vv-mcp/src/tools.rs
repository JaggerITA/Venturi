//! The tools and their arguments. Argument structs derive `JsonSchema`: the
//! MCP layer publishes them as the tools' input schemas.

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    GetProject,
    GetTimeline(TimelineArgs),
    GetClip(ClipArgs),
    NewProject,
    OpenProject(OpenProjectArgs),
    SaveProject(SaveProjectArgs),
    ImportMedia(ImportMediaArgs),
    ImportOtio(ImportOtioArgs),
    CreateTimeline(CreateTimelineArgs),
    AddTrack(AddTrackArgs),
    SetTrack(SetTrackArgs),
    InsertClip(InsertClipArgs),
    Split(SplitArgs),
    DeleteClips(DeleteClipsArgs),
    DeleteRanges(DeleteRangesArgs),
    MoveClips(MoveClipsArgs),
    TrimClip(TrimClipArgs),
    SetClipProperties(SetClipPropertiesArgs),
    AddTitle(AddTitleArgs),
    AddSolidColor(AddSolidColorArgs),
    AddAdjustmentClip(AddAdjustmentClipArgs),
    LinkClips(ClipsArgs),
    UnlinkClips(ClipsArgs),
    AddMarker(AddMarkerArgs),
    EditMarker(EditMarkerArgs),
    DeleteMarker(MarkerArgs),
    Undo,
    Redo,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct TimelineArgs {
    pub timeline_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ClipArgs {
    pub timeline_id: String,
    pub clip_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ClipsArgs {
    pub timeline_id: String,
    pub clip_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ImportOtioArgs {
    /// Path of an `.otio` file.
    pub path: String,
    /// When some of its media have the same file name as media already in
    /// the pool: `true` points the clips at the existing media, `false`
    /// (default) imports them again into a new folder.
    #[serde(default)]
    pub reuse_existing_media: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrackKindArg {
    Video,
    Audio,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct AddTrackArgs {
    pub timeline_id: String,
    pub kind: TrackKindArg,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct SetTrackArgs {
    pub timeline_id: String,
    /// Track name as in `get_timeline`: "V1", "A2", ...
    pub track: String,
    #[serde(default)]
    pub muted: Option<bool>,
    #[serde(default)]
    pub solo: Option<bool>,
    /// Locked tracks are left alone by every edit tool.
    #[serde(default)]
    pub locked: Option<bool>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct InsertClipArgs {
    pub timeline_id: String,
    pub media_id: String,
    /// Timeline frame where the clip starts. What is already there is
    /// overwritten (shortened, split or removed).
    pub at: i64,
    /// First media frame used (media frames, not timeline frames). Default 0.
    #[serde(default)]
    pub source_in: Option<i64>,
    /// Media frame after the last one used. Default: the end of the media
    /// (5 s for an image).
    #[serde(default)]
    pub source_out: Option<i64>,
    /// Video track name ("V1", ...). Default: the first unlocked one, created
    /// if there is none.
    #[serde(default)]
    pub video_track: Option<String>,
    /// Audio track for the first audio stream ("A1", ...); the other streams
    /// go on the following unlocked audio tracks, created as needed.
    #[serde(default)]
    pub audio_track: Option<String>,
    /// Put the media's video on the timeline. Default true.
    #[serde(default = "yes")]
    pub video: bool,
    /// Put the media's audio streams on the timeline. Default true.
    #[serde(default = "yes")]
    pub audio: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct SplitArgs {
    pub timeline_id: String,
    /// Timeline frame of the cut: the right halves start here.
    pub frame: i64,
    /// Only these clips; default every clip of the unlocked tracks crossing
    /// `frame`. Linked clips are not added automatically.
    #[serde(default)]
    pub clip_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct DeleteClipsArgs {
    pub timeline_id: String,
    pub clip_ids: Vec<String>,
    /// Close the gaps, shifting everything after them on every unlocked
    /// track (their linked clips are deleted too). Default false.
    #[serde(default)]
    pub ripple: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct DeleteRangesArgs {
    pub timeline_id: String,
    /// `[start, end)` timeline frame ranges; clips crossing an edge are cut
    /// there. Overlapping or touching ranges are merged.
    pub ranges: Vec<[i64; 2]>,
    /// Close the gaps on every unlocked track, keeping audio and video in
    /// sync. Default false.
    #[serde(default)]
    pub ripple: bool,
    /// Without `ripple`: only these tracks ("V1", "A1", ...). Default all
    /// unlocked tracks.
    #[serde(default)]
    pub tracks: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ClipMove {
    pub clip_id: String,
    /// New timeline start frame.
    pub start: i64,
    /// Destination track, of the same kind; default the current one.
    #[serde(default)]
    pub track: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct MoveClipsArgs {
    pub timeline_id: String,
    /// The moved clips overwrite what is at their destination. Linked clips
    /// are not moved along: list them too.
    pub moves: Vec<ClipMove>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EdgeArg {
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct TrimClipArgs {
    pub timeline_id: String,
    pub clip_id: String,
    pub edge: EdgeArg,
    /// New timeline frame of that edge (`end` is exclusive). Growing over
    /// a neighbour overwrites it; the media's length is the limit.
    pub frame: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct SetClipPropertiesArgs {
    pub timeline_id: String,
    pub clip_ids: Vec<String>,
    /// Percent, 0-100.
    #[serde(default)]
    pub opacity: Option<f32>,
    /// `[x, y]` displacement in timeline pixels from the center, Y up.
    #[serde(default)]
    pub position: Option<[f32; 2]>,
    /// `[x, y]` magnification, 1 = original size.
    #[serde(default)]
    pub scale: Option<[f32; 2]>,
    /// Degrees, clockwise.
    #[serde(default)]
    pub rotation: Option<f32>,
    /// Audio gain in dB, 0 = unchanged.
    #[serde(default)]
    pub gain_db: Option<f32>,
    /// A disabled clip is neither seen nor heard.
    #[serde(default)]
    pub disabled: Option<bool>,
    /// Fade in length, in timeline frames.
    #[serde(default)]
    pub fade_in: Option<i64>,
    /// Fade out length, in timeline frames.
    #[serde(default)]
    pub fade_out: Option<i64>,
    /// `[r, g, b, a]` in 0-1: the color of a solid color clip.
    #[serde(default)]
    pub color: Option<[f32; 4]>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct AddTitleArgs {
    pub timeline_id: String,
    pub text: String,
    /// Timeline start frame; what is there on the track is overwritten.
    pub at: i64,
    /// In timeline frames. Default 5 s.
    #[serde(default)]
    pub duration: Option<i64>,
    /// Video track ("V1", ...). Default: the first unlocked one.
    #[serde(default)]
    pub track: Option<String>,
    /// Font size in timeline pixels.
    #[serde(default)]
    pub size: Option<f32>,
    /// `[r, g, b, a]` in 0-1.
    #[serde(default)]
    pub color: Option<[f32; 4]>,
    /// `[x, y]` from the center of the frame in timeline pixels, Y up.
    #[serde(default)]
    pub position: Option<[f32; 2]>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct AddSolidColorArgs {
    pub timeline_id: String,
    /// Timeline start frame; what is there on the track is overwritten.
    pub at: i64,
    /// In timeline frames. Default 5 s.
    #[serde(default)]
    pub duration: Option<i64>,
    /// Video track ("V1", ...). Default: the first unlocked one.
    #[serde(default)]
    pub track: Option<String>,
    /// `[r, g, b, a]` in 0-1.
    #[serde(default)]
    pub color: Option<[f32; 4]>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct AddAdjustmentClipArgs {
    pub timeline_id: String,
    /// Timeline start frame; what is there on the track is overwritten.
    pub at: i64,
    /// In timeline frames. Default 5 s.
    #[serde(default)]
    pub duration: Option<i64>,
    /// Video track ("V1", ...). Default: the first unlocked one.
    #[serde(default)]
    pub track: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct AddMarkerArgs {
    pub timeline_id: String,
    /// Timeline frame.
    pub at: i64,
    /// In timeline frames; 0 (default) marks a single frame.
    #[serde(default)]
    pub duration: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct EditMarkerArgs {
    pub timeline_id: String,
    pub marker_id: String,
    #[serde(default)]
    pub at: Option<i64>,
    #[serde(default)]
    pub duration: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct MarkerArgs {
    pub timeline_id: String,
    pub marker_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct OpenProjectArgs {
    /// Path of a `.vvproj` file.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct SaveProjectArgs {
    /// Where to save; omitted, the project's current file.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct ImportMediaArgs {
    /// Video, audio or image files. Files already in the media pool are
    /// skipped.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct CreateTimelineArgs {
    pub name: String,
    /// Takes fps and resolution from this media (its video, if any).
    #[serde(default)]
    pub from_media: Option<String>,
    /// Frames per second as `[numerator, denominator]`, e.g. `[30000, 1001]`.
    /// Default 25, or the media's with `from_media`.
    #[serde(default)]
    pub fps: Option<[i32; 2]>,
    /// `[width, height]` in pixels. Default 1920x1080, or the media's with
    /// `from_media`.
    #[serde(default)]
    pub resolution: Option<[u32; 2]>,
}

/// What a tool returns: JSON for the agent, plus an image for the visual
/// tools.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub value: serde_json::Value,
    pub image_png: Option<Vec<u8>>,
}

impl ToolOutput {
    pub fn json(value: serde_json::Value) -> Self {
        Self {
            value,
            image_png: None,
        }
    }
}

/// A failed call, reported to the agent as a tool error (not a protocol
/// error) with this message.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type ToolResult = Result<ToolOutput, ToolError>;
