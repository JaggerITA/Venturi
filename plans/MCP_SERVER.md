# MCP_SERVER — Model Context Protocol server (Vikunja #13)

**Status:** done on branch `mcp_server` (based on `session_layer`), not merged.

An MCP server that lets an agent drive Venturi: import media, build
timelines, cut, add titles/transitions, save, export, plus visual feedback
(a composited frame as PNG, a screenshot of the whole UI). Two modes:
- **headless**: `vv-app mcp [project.vv]`, MCP over stdio, no window;
- **attach**: the GUI the user is working in, reached through
  `vv-app mcp --attach` (a stdio ↔ local socket bridge).

Use cases driving the tool set: AI-driven editing end to end, automatic
removal of silences / bad takes (the agent transcribes on its own and
uses `get_audio_levels` + `delete_ranges`), inserting synthetic media the
agent generates as files.

Decisions (agreed with the user):
- **Protocol in its own crate `vv-mcp`, hosts elsewhere.** `vv-mcp` holds
  `rmcp`/`tokio`/`schemars`, the tool definitions, the transports and the
  `--attach` bridge; it never sees `VenturiApp`. It exposes a
  `McpHost` trait and the tool dispatcher over `vv_session::Session`.
  Two hosts: headless (a bare `Session`, no egui) and the GUI
  (`VenturiApp`, adds deferral during gestures and `screenshot_ui`). Both
  run the same `Session` code, so behaviour matches by construction. The
  heavy macro dependencies stay out of `vv-app`'s incremental builds.
- **Transport**: stdio. Attach goes through a bridge process piping stdio to
  a Unix socket (named pipe on Windows) owned by the GUI; no network port.
  The client config is the same command in both modes.
- **Library**: `rmcp` (official Rust SDK). Its tokio runtime runs on a
  dedicated thread, started only when MCP is enabled.
- **Tools are semantic**, not a 1:1 mirror of the `vv-core` `Command`s.
- **Units**: integer frames of the target timeline (or of the media, for
  media-space parameters) everywhere; outputs also report fps and seconds.
- **Audio analysis**: `get_audio_levels` (per-frame RMS/peak), thresholds
  and durations are the agent's call. No built-in silence detection, no
  transcription.
- **One tool call = one undo step** (`History::begin_group`/`end_group`):
  in attach mode the user can undo whatever the agent did, step by step.
- **Off by default in the GUI**: enabled from Settings or `--mcp`.

---

## 1. Prerequisite: session layer

The UI-free editing layer (`vv_core::edit`, including `delete_ranges`) and
the `Session` core (project/history/path, import/OTIO/relink/export jobs,
waker, `tick() -> Vec<SessionEvent>`) are planned in `SESSION_LAYER.md`.
This plan assumes its steps 1-4 are done.

## 2. Hosts

- **`vv-mcp` API**: `ToolCall`/`ToolResult` types, and
  `dispatch(session: &mut Session, call) -> Handled(result) |
  Deferred(PendingMcp)` holding the tool logic. Hosts wrap it.
- **Request channel**: `McpRequest { call: ToolCall,
  reply: oneshot::Sender<ToolResult> }` over a std `mpsc` channel; the
  `McpHost` trait is the sending side. The owner of the `Session` drains
  it:
  - GUI: at the top of `ui()`, after `session.tick()`; the listener thread calls
    `ctx.request_repaint()` on every incoming request.
  - headless (`vv-mcp` or a thin `vv-app mcp` entry): `recv_timeout`
    loop woken by the session waker, `Session::tick()` on every iteration.
- **GUI host**: `VenturiApp::handle_mcp` checks the GUI-only conditions,
  then calls `vv_mcp::dispatch(&mut self.session, call)`; `get_state`
  and `screenshot_ui` are answered by the GUI host itself. Deferred calls
  sit in a list re-checked on every tick, for:
  - an import/OTIO import still probing (resolved by its `SessionEvent`);
  - a screenshot waiting for egui's `Event::Screenshot`;
  - attach mode, the user mid-gesture (`edit_drag_group.is_some()` or a
    timeline gesture active): applying now would merge the agent's edit
    into the user's undo step;
  - a modal in the way (`pending_project_switch`, unsaved-changes dialog):
    the call fails with a "busy" error instead of waiting forever.
- After an edit tool, the dispatcher calls
  `Session::sync_timeline_media(timeline)` so the timeline's pool entry
  follows (the GUI does it only for the open timeline).
- Heavy work never runs on the owner thread: `render_frame`,
  `get_audio_levels` and `export` take a project snapshot (the same
  `project.clone()` export already does) and reply from a worker thread.
- **Headless mode**: `vv-app mcp [project]` parsed before eframe starts
  (today `args().nth(1)` is the startup media path; `mcp` becomes a
  subcommand). No eframe, no `VenturiApp`: a `Session` plus
  `Compositor::new_headless()` for `render_frame`; no audio device, no
  proxy/thumbnail/waveform workers (render/export use the originals, like
  export already does).
- Tests: `vv-mcp/src/tests/` drive `dispatch` on a bare `Session`; the GUI
  deferral rules are tested in `vv-app/src/tests/mcp_host.rs` on
  `VenturiApp::default()`. No MCP transport involved.

## 3. MCP server (`crates/vv-mcp`)

- `rmcp` server with `#[tool]` handlers that only translate arguments,
  send an `McpRequest` and await the reply; schemas via `schemars`.
- Same server code for both transports: stdio (headless) and one socket
  connection (attach). The tokio runtime lives on its own thread.
- **Ids**: strings. `MediaId`/`TimelineId`/`FolderId` via
  `KeyData::as_ffi` (u64 would overflow JS-safe integers),
  `ClipId`/`MarkerId`/`LinkGroupId` from their `u64`. Clips are addressed
  by `(timeline_id, clip_id)`; the track index is looked up, since it can
  change under the agent's feet in attach mode.
- **Errors**: MCP tool errors (`isError`) with a plain English message:
  unknown id, locked track, overlap in insert mode, range outside media.
- Tool descriptions state units and undo semantics explicitly.

### Tool set v1

State
- `get_project` — media pool (id, path, kind, fps, duration, resolution,
  audio streams, folder, offline), timelines (id, name, fps, resolution,
  length), project path, unsaved flag.
- `get_timeline(timeline_id)` — tracks (kind, flags) and clips (id, source,
  timeline start/end, source in/out, speed, link group, disabled),
  transitions, markers. Effects summarised, not dumped.
- `get_clip(timeline_id, clip_id)` — full attributes (transform, opacity,
  gain, fades, filters, keyframed params with their keyframes).
- `get_state` — active timeline, playhead, selection (attach mode: lets
  the user point at something and say "this").

Project
- `new_project`, `open_project(path)`, `save_project(path?)`
  (headless: never prompts; attach: refuses if the user has a modal open).
- `import_media(paths, folder?)` → media ids and metadata once probed.
- `import_otio(path)` → timeline id + warnings.

Timeline
- `create_timeline(name, fps?, resolution? | from_media?)`,
  `set_active_timeline`, `add_track(kind)`, `set_track(flags)`.

Edit
- `insert_clip(media_id, timeline_id, at, source_in?, source_out?,
  video_track?, audio_track?, mode)`
- `split(timeline_id, frame, clip_ids?)`
- `delete_clips(clip_ids, ripple)`, `delete_ranges(ranges, ripple, tracks?)`
- `move_clips(moves)`, `trim_clip(clip_id, edge, frame)`
- `set_clip_properties(clip_ids, {transform, opacity, gain, fades, speed,
  blend_mode, filters, color, disabled})` — static values; keyframe tools
  are v2.
- `add_title(text, style…, at, duration, track)`,
  `add_solid_color`, `add_adjustment_clip`
- `set_transition(clip_id, edge, kind, duration | none)`
- `link_clips`, `unlink_clips`
- `add_marker`, `edit_marker`, `delete_marker`
- `undo`, `redo`

Visual feedback
- `render_frame(timeline_id, frame, max_width = 960)` → PNG image content.
  Exact decode of that frame through the export path (not the viewer's
  render_ahead cache): frame accuracy is non-negotiable. Needs a
  single-frame entry point extracted from `export.rs`
  (`decode_video_frame` + compose to RGBA).
- `screenshot_ui()` → PNG of the whole window
  (`ViewportCommand::Screenshot`, reply deferred to the `Event::Screenshot`
  of the next frame). Attach mode only; headless returns an error.

Analysis
- `get_audio_levels(source, range, window = 1)` — `source` is
  `{media_id, stream}` (media frames) or `{timeline_id}` (the timeline mix,
  via export's `mix_audio_track`). Returns per-window RMS and peak in dBFS,
  from decoded samples, not from the waveform cache (100 peaks/s, peak
  only). Output capped (e.g. 20 000 windows): past it, an error asking for a
  larger `window` or a shorter range.

Export
- `export(timeline_id, path, range?, scale?, codec?, audio?)` → job id;
  `export_status(job_id)` → progress/done/error; `cancel_export(job_id)`.
  Reuses `export_timeline` on its thread; in attach mode it is the same
  `ExportUiState` the dialog uses, so the progress window shows.

Deferred to v2: keyframe editing, compound clips, paste attributes, clip
speed ramps, OTIO export, playback control.

## 4. GUI attach

- Settings → "Integrations": "Enable MCP server" (off by default), plus
  `--mcp` on the command line. i18n strings `en` + `it`.
- Listener thread on `$XDG_RUNTIME_DIR/venturi/mcp-<pid>.sock`, mode 0600,
  removed on exit (stale sockets of dead pids cleaned at startup). Each
  connection gets its own rmcp session on the shared runtime; all of them
  feed the same request channel (serial, last-write-wins).
- `vv-app mcp --attach [--pid N]`: pipes stdio to the socket. Without
  `--pid`, the only running instance; with several, an error listing them.
- Toolbar indicator while a client is connected (icon + tooltip with the
  last tool called); hidden otherwise. Trade-dress rules apply.
- History labels stay the natural ones (`SplitClips`, `InsertClips`, …).
- Windows: named pipe instead of the socket (together with #15). Flatpak
  (#1): socket under `$XDG_RUNTIME_DIR/app/<app-id>/`, bridge through
  `flatpak run`.

## 5. Docs

`docs/MCP.md`: setup (`claude mcp add venturi -- vv-app mcp`, the
`--attach` variant), tool reference with units and undo semantics, a worked
"remove silences" example. Link from `docs/BUILDING.md`.

---

## Steps

| # | Step | Commit |
|---|------|--------|
| 1 | `SESSION_LAYER.md` steps 1-4 (edit layer, `vv-session`, jobs + events) | see that plan |
| 2 | `vv-mcp` crate: `ToolCall`/`ToolResult`, `dispatch` over `Session`, headless `vv-app mcp` loop; dispatch tests | 555241f |
| 3 | rmcp over stdio; state, project, timeline and edit tools; manual test with Claude Code | 5e774af, db8d234 |
| 4 | `render_frame` (single-frame export path), `get_audio_levels`, export tools | 0d2e53c |
| 5 | GUI attach: setting/flag, socket listener, `--attach` bridge, gesture deferral, indicator, `screenshot_ui` | 4175889, bbb4ad3, ad835ff, 7407afd |
| 6 | `docs/MCP.md` | ebbfc94 |

Each step builds and passes `timeout 300 cargo test` on its own. The risk
sits in step 1 (the session refactor); steps 2-6 only add code.

## Step 2 notes

- No `McpHost` trait: a concrete channel (`vv_mcp::channel(notify)` →
  `McpHandle` for the transport, `McpInbox` for the owner of the
  `Session`) serves both hosts; `notify` is the GUI's repaint request.
  The inbox's session waker holds the sender weakly, so dropping the last
  handle still ends `run_headless`.
- `run_headless` lives in vv-mcp; the `vv-app mcp` subcommand that starts it
  comes with the stdio transport in step 3 (without a transport nobody could
  talk to it).
- `Session` is not `Send` (`History` holds `Box<dyn Command>`): the host
  owns it on its thread, the tokio runtime of step 3 goes on another one.
- First tools, enough to exercise immediate and deferred calls:
  `get_project`, `new_project`, `open_project`, `save_project`,
  `import_media` (answers when the import job ends, listing media already
  in the pool too), `create_timeline`, `undo`, `redo`.
- `undo`/`redo` do not resync timeline pool entries (which timeline a step
  touched is unknown); only matters once compound clips are exposed (v2).

## Step 3 notes

- `vv-app mcp [project.vvproj]` serves 28 tools over stdio (rmcp 3.4.1,
  tokio current-thread runtime on its own thread). Claude Code:
  `claude mcp add venturi -- /path/to/vv-app mcp`.
- Tracks are addressed by the names the user sees (`V1`, `A2`), clips by id
  alone (the track is looked up). Every editing tool validates all its
  arguments before touching the project and runs as one undo step; if it
  still fails midway, what it applied is undone.
- `vv_core::edit` gained `move_clips`, `trim_range`/`grown_range`/`trim_clip`
  (moved out of `timeline_ui`, which now uses them) and `split_clips`
  returns the right halves.
- Not done from the v1 list, left for later: `set_active_timeline` (a GUI
  notion, goes with `get_state` in step 5); the `folder` argument of
  `import_media`; `insert_clip` insert (ripple) mode — only overwrite, a
  ripple insert needs a new command; `set_transition`; speed, blend mode and
  filters in `set_clip_properties`.
- Tested end to end: a scripted JSON-RPC client over the real binary's
  stdio, an in-process rmcp client test, and Claude Code itself
  (`claude-local.sh -p` with `--mcp-config`) importing, building a
  timeline, ripple-deleting a second, adding a title and saving — the saved
  project reopened as expected.

## Step 4 notes

- `render_frame` decodes through the export's `StreamingFrameProvider` and
  composites with `Compositor::render_layers` (same shaders, read back as
  RGBA instead of I420). A test checks it returns exactly the asked frame
  (luma-numbered frames, conformed source offset). One headless compositor
  is created on first use and shared.
- `get_audio_levels`: `vv_session::analysis`. A media stream is decoded
  only up to the end of the range, resampled to 48 kHz; the timeline source
  uses the export mix (`mix_audio_track`, which still decodes whole files:
  audit §1.6). Levels floored at -120 dBFS; max 20 000 windows.
- Heavy calls run on a thread and come back as `Pending::Worker`, which the
  host polls after every tick; the worker wakes the host through the
  session waker.
- `export`/`export_status`/`cancel_export` by job id. `Session::export`
  now refuses a second concurrent export (before, it silently replaced the
  running one, whose end was then never reported); a finished export that
  `tick` has not collected yet is no longer cancellable.
- Agent test (plain `claude -p` with `--mcp-config`): found the two pauses
  of a test file with `get_audio_levels`, removed them with one
  `delete_ranges` ripple call, checked with render_frame and the timeline
  levels, exported; ffmpeg confirms 3.6 s and no silence left.

## Step 5 notes

- Settings > Integrations ("Let AI agents work in this window (MCP)", off
  by default) or `--mcp` for one run. The socket is
  `$XDG_RUNTIME_DIR/venturi/mcp-<pid>.sock` (fallback
  `$TMPDIR/venturi-$USER`), dir 0700, socket 0600; sockets nobody answers on
  are removed when an editor starts or a bridge looks for one. One tokio
  current-thread runtime per editor serves every connection.
- `vv-app mcp --attach [--pid N]` pipes stdio to it. It must not use
  `io::copy`: between two descriptors it may `splice`, and replies stalled
  when stdout was a pipe to `podman exec` (clients timed out).
- Editor host (`vv-app/src/mcp_host.rs`): calls that change the project wait
  while the user has the pointer down / a timeline gesture / an open edit
  group, so they never merge into the user's undo step; they are refused
  while the unsaved-changes, OTIO-merge or forced-relink dialog waits;
  `new_project`/`open_project` are refused over unsaved changes. Agent jobs
  (imports, OTIO) do not move the user's view or warnings; the agent's
  export shows in the progress window. `get_state`, `screenshot_ui` (next
  `Event::Screenshot`) and `set_active_timeline` are answered by the
  editor; headless they return an error.
- `Session::finish_otio_import` now reports through `OtioImported` /
  `OtioCancelled`, so the editor and an agent waiting on the same import
  both get the outcome.
- Every MCP edit names its undo step after the tool (a ripple
  `delete_ranges` was listed as its first command, "SplitClips").
- Toolbar: an accent "● MCP" while a client is connected, tooltip with the
  last tool called.
- Tested: `tests/mcp_host.rs` (8 tests), socket tests, and Claude Code on the
  host driving the editor in the podman container through
  `podman exec -i venturi-session vv-app mcp --attach`: import, timeline
  shown with set_active_timeline, silences removed, screenshot_ui (the
  indicator visible), undo/redo.
- Not done: Windows named pipe (#15), Flatpak socket path (#1).

## Open points

- Socket naming with several GUI instances: `--pid` is the minimal answer;
  revisit if it proves clumsy.
- Whether `import_media` in attach mode should also switch the viewer to the
  imported media (today's `preview_media` after import): probably not,
  the agent should not steal the viewer.
- Response size of `get_timeline` on large projects: add paging only if it
  turns out to be a problem.
