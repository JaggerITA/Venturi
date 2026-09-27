//! The tools and their arguments. Argument structs derive `JsonSchema`: the
//! MCP layer publishes them as the tools' input schemas.

use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCall {
    GetProject,
    NewProject,
    OpenProject(OpenProjectArgs),
    SaveProject(SaveProjectArgs),
    ImportMedia(ImportMediaArgs),
    CreateTimeline(CreateTimelineArgs),
    Undo,
    Redo,
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
