//! Model Context Protocol server for Venturi. The tool logic runs on a
//! `vv_session::Session` owned by a host (headless, or the GUI); the MCP
//! transport only sends it `ToolCall`s through a `McpHandle`.

mod dispatch;
mod edit_tools;
mod host;
mod ids;
mod json;
mod media_tools;
mod server;
mod socket;
mod tools;

pub use dispatch::{Dispatch, Pending, dispatch};
pub use host::{
    Disconnected, McpHandle, McpInbox, McpRequest, PendingCalls, channel, run_headless,
};
pub use json::{GuiState, state_json};
pub use media_tools::encode_png;
pub use server::{VenturiServer, serve_headless_stdio, serve_stdio};
pub use socket::{SocketServer, bridge_stdio, socket_dir};
pub use tools::*;
