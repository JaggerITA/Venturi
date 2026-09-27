//! Ids as the agent sees them: decimal strings. Slotmap keys go through
//! their FFI form, whose u64 would not survive a JSON number in JS clients.

use slotmap::{Key, KeyData};
use vv_core::MediaId;

use crate::ToolError;

pub(crate) fn key_to_string(key: impl Key) -> String {
    key.data().as_ffi().to_string()
}

fn parse_key<K: From<KeyData>>(id: &str, what: &str) -> Result<K, ToolError> {
    id.trim()
        .parse::<u64>()
        .map(|ffi| KeyData::from_ffi(ffi).into())
        .map_err(|_| ToolError(format!("malformed {what} id \"{id}\"")))
}

pub(crate) fn media_id(project: &vv_core::Project, id: &str) -> Result<MediaId, ToolError> {
    let key = parse_key(id, "media")?;
    project
        .media_pool
        .contains_key(key)
        .then_some(key)
        .ok_or_else(|| ToolError(format!("unknown media id \"{id}\"")))
}
