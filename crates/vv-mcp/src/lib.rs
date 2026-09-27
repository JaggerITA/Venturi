//! Model Context Protocol server for Venturi. The tool logic runs on a
//! `vv_session::Session` owned by a host (headless, or the GUI); the MCP
//! transport only sends it `ToolCall`s through a `McpHandle`.

mod dispatch;
mod host;
mod ids;
mod tools;

pub use dispatch::{Dispatch, Pending, dispatch};
pub use host::{
    Disconnected, McpHandle, McpInbox, McpRequest, PendingCalls, channel, run_headless,
};
pub use tools::*;
