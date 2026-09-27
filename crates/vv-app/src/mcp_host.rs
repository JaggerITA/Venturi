//! MCP in the editor window (attach mode): the agent's calls run on the
//! same `Session` the user is editing, between two frames.

use std::collections::{HashSet, VecDeque};

use vv_mcp::{Dispatch, McpRequest, Pending, PendingCalls, Reply, ToolCall, ToolError, ToolOutput};
use vv_session::JobId;

use super::*;

pub(crate) struct McpHost {
    /// `None` in the tests, which submit calls straight to the channel.
    server: Option<vv_mcp::SocketServer>,
    inbox: vv_mcp::McpInbox,
    pending: PendingCalls,
    /// Calls that change the project, held while the user is dragging:
    /// applied mid-gesture they would end up in the user's undo step.
    held: VecDeque<McpRequest>,
    screenshots: Vec<Reply>,
    /// Jobs the agent started: their outcome does not move the user's view.
    agent_jobs: HashSet<JobId>,
    pub(crate) last_tool: Option<&'static str>,
}

impl McpHost {
    fn new(server: Option<vv_mcp::SocketServer>, inbox: vv_mcp::McpInbox) -> Self {
        Self {
            server,
            inbox,
            pending: PendingCalls::default(),
            held: VecDeque::new(),
            screenshots: Vec::new(),
            agent_jobs: HashSet::new(),
            last_tool: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(inbox: vv_mcp::McpInbox) -> Self {
        Self::new(None, inbox)
    }

    pub(crate) fn clients(&self) -> usize {
        self.server.as_ref().map_or(0, |s| s.clients())
    }

    pub(crate) fn is_agent_job(&self, job: JobId) -> bool {
        self.agent_jobs.contains(&job)
    }
}

fn reply(request: McpRequest, result: vv_mcp::ToolResult) {
    let _ = request.reply.send(result);
}

impl VenturiApp {
    /// Starts or stops serving MCP to follow the setting (or `--mcp`).
    pub(crate) fn apply_mcp_setting(&mut self, ctx: &egui::Context) {
        let wanted = self.settings.mcp_enabled || self.mcp_forced;
        match (wanted, self.mcp.is_some()) {
            (true, false) => {
                let repaint = ctx.clone();
                let (handle, inbox) = vv_mcp::channel(move || repaint.request_repaint());
                match vv_mcp::SocketServer::start(handle) {
                    Ok(server) => self.mcp = Some(McpHost::new(Some(server), inbox)),
                    Err(e) => {
                        self.project_error =
                            Some(t!("settings.mcp_failed", error = e.to_string()).into_owned())
                    }
                }
            }
            (false, true) => self.mcp = None,
            _ => {}
        }
    }

    pub(crate) fn is_agent_job(&self, job: JobId) -> bool {
        self.mcp.as_ref().is_some_and(|mcp| mcp.is_agent_job(job))
    }

    /// Lets the calls waiting on background work see `event` first.
    pub(crate) fn mcp_session_event(&mut self, event: &vv_session::SessionEvent) {
        if let Some(mcp) = &mut self.mcp {
            mcp.pending.resolve(&mut self.session, event);
        }
    }

    /// The user is in the middle of a pointer gesture: a drag on the
    /// timeline, a slider, the viewer's handles.
    fn user_mid_gesture(&self, ctx: &egui::Context) -> bool {
        self.edit_drag_group.is_some()
            || self.timeline_state.gesture_active()
            || ctx.input(|i| i.pointer.any_down())
    }

    /// A dialog is waiting for the user's answer about the project.
    fn modal_open(&self) -> bool {
        self.pending_project_switch.is_some()
            || self.session.otio_awaiting_decision().is_some()
            || self.forced_relink.is_some()
    }

    pub(crate) fn poll_mcp(&mut self, ctx: &egui::Context) {
        let Some(mut mcp) = self.mcp.take() else {
            return;
        };
        mcp.pending.poll();
        let busy = self.user_mid_gesture(ctx);
        if !busy {
            while let Some(request) = mcp.held.pop_front() {
                self.handle_mcp_request(&mut mcp, request, ctx);
            }
        }
        while let Some(request) = mcp.inbox.try_recv() {
            if busy && request.call.mutates() {
                mcp.held.push_back(request);
            } else {
                self.handle_mcp_request(&mut mcp, request, ctx);
            }
        }
        if !mcp.held.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        self.mcp = Some(mcp);
    }

    fn handle_mcp_request(&mut self, mcp: &mut McpHost, request: McpRequest, ctx: &egui::Context) {
        mcp.last_tool = Some(request.call.name());
        if request.call.mutates() && self.modal_open() {
            return reply(
                request,
                Err(ToolError(
                    "the user has a dialog open in Venturi: retry once it is closed".into(),
                )),
            );
        }
        match &request.call {
            ToolCall::GetState => {
                let state = self.gui_state();
                let value = vv_mcp::state_json(&self.session, &state);
                return reply(request, Ok(ToolOutput::json(value)));
            }
            ToolCall::ScreenshotUi => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                mcp.screenshots.push(request.reply);
                return;
            }
            ToolCall::SetActiveTimeline(args) => {
                let result = vv_mcp::parse_timeline_id(&self.session.project, &args.timeline_id)
                    .map(|id| {
                        self.stop_browsing();
                        self.open_timeline(id);
                        self.spawn_render_ahead_if_needed(id);
                        ToolOutput::json(serde_json::json!({ "active_timeline": args.timeline_id }))
                    });
                return reply(request, result);
            }
            ToolCall::NewProject | ToolCall::OpenProject(_)
                if self.session.has_unsaved_changes() =>
            {
                return reply(
                    request,
                    Err(ToolError(
                        "the project open in Venturi has unsaved changes: save it first \
                         (save_project) or ask the user"
                            .into(),
                    )),
                );
            }
            _ => {}
        }
        let replaces_project = matches!(
            request.call,
            ToolCall::NewProject | ToolCall::OpenProject(_)
        );
        let McpRequest { call, reply: to } = request;
        match vv_mcp::dispatch(&mut self.session, call) {
            Dispatch::Handled(result) => {
                if replaces_project && result.is_ok() {
                    self.reset_for_replaced_project();
                }
                let _ = to.send(result);
            }
            Dispatch::Deferred(pending) => {
                if let Pending::Import { job, .. } | Pending::Otio { job, .. } = &pending {
                    mcp.agent_jobs.insert(*job);
                }
                mcp.pending.push(pending, to);
            }
        }
        // The progress window shows the agent's export like the user's.
        if self.export.is_none()
            && let Some(job) = self.session.running_export()
            && let Some(progress) = self.session.export_progress(job)
        {
            self.pause_proxies_for_export();
            self.export = Some(ExportUiState { job, progress });
        }
    }

    fn gui_state(&self) -> vv_mcp::GuiState {
        vv_mcp::GuiState {
            active_timeline: self.timeline_id,
            timeline_stack: self.timeline_stack.clone(),
            playhead: self.timeline_state.playhead,
            selected_clips: self
                .timeline_state
                .selected
                .iter()
                .map(|&(_, id)| id)
                .collect(),
            selected_media: self.media_pool_state.selected.iter().copied().collect(),
            previewed_media: self.browsing_media,
        }
    }

    /// Answers the `screenshot_ui` calls with the frame egui just captured.
    pub(crate) fn mcp_screenshot(&mut self, image: &egui::ColorImage) {
        let Some(mcp) = &mut self.mcp else {
            return;
        };
        if mcp.screenshots.is_empty() {
            return;
        }
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let size = (image.size[0] as u32, image.size[1] as u32);
        let result = vv_mcp::encode_png(&rgba, size).map(|png| ToolOutput {
            value: serde_json::json!({ "width": size.0, "height": size.1 }),
            image_png: Some(png),
        });
        for screenshot in mcp.screenshots.drain(..) {
            let _ = screenshot.send(result.clone());
        }
    }
}

#[cfg(test)]
#[path = "tests/mcp_host.rs"]
mod tests;
