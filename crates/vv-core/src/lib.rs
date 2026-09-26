pub mod command;
pub mod model;
pub mod otio;
pub mod persistence;

pub use command::{
    AddTrack, Command, CommandLabel, CompositeCommand, CompoundPlan, FadeEdge, GroupMark, History,
    InsertClip, KeyframePick, KeyframeTarget, KeyframeValue, LiftDelete, LinkClips, MoveClips,
    MoveKeyframes, RemoveKeyframe, RemoveMedia, RemoveTrack, ResetTransformParams, RippleDeleteGap,
    SetClipAttributes, SetClipColor, SetClipFade, SetClipSpeed, SetClipValue, SetClipsDisabled,
    SetClipsDisplayColor, SetCrossTransition, SetKeyframeInterpolation, SetMediaPath, SetTrackFlag,
    SplitClip, TrackFlag, TrimClip, TrimEdge, UnlinkClip, UpsertKeyframe, compound_clip_commands,
    cut_overlaps, insert_overwriting, make_room_for_ranges, plan_compound_clip, reset_clip_gain,
    set_clip_blend_mode, set_clip_filters, set_clip_flip, set_clip_gain, set_clip_title,
    set_clip_transform_param, set_clip_transition,
};
pub use model::*;
pub use otio::{
    MeasureTitle, OtioError, OtioImport, OtioWarning, TitleMetrics, export_otio, import_otio,
    media_url_count, project_from_otio,
};
pub use persistence::{PersistenceError, load_project, save_project};

#[cfg(test)]
#[path = "tests/lib.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/lib_compound_clip.rs"]
mod compound_clip_tests;

#[cfg(test)]
#[path = "tests/lib_clip_speed.rs"]
mod clip_speed_tests;
