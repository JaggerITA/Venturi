# Venturi — web/tablet frontend: implementation notes

Design document for decoupling editing from egui and enabling a web client
(e.g. a tablet on the local network) that drives a headless backend running
on a desktop PC. Written to be used as a starting point in a separate
chat/session — it does not touch the ongoing work on the native egui
frontend, which stays the "main" client until further notice.

It comes from an exploratory question by the user ("what if the frontend
were web-based? can I mount it on a tablet connected to the backend on a
desktop PC?"), not from an already decided requirement: before implementing,
check that the stage 1 choices (see below) are still the desired ones.

## Why it is feasible without rewriting the backend

The project is already split into data-oriented crates, and only `vv-app` is
tied to egui:

| Crate | What it does | Reusable as-is? |
|---|---|---|
| `vv-core` | data model (`Project`/`Timeline`/`Clip`), command pattern, `History` | yes, 100% |
| `vv-media` | ffmpeg probe/decode, `SharedFrameCache` | yes, 100% |
| `vv-render` | headless GPU compositor (wgpu), text | yes, 100% |
| `vv-audio` | audio track mixer, output through cpal | yes for local monitoring on the PC (see § Audio) |
| `vv-app` | egui/eframe UI, `TimelineAudio`, orchestration (`main.rs`), timeline drawing (`timeline_ui.rs`) | **only partly**: `TimelineAudio`, `History`, `drive_playback`, `ensure_active_clip_matches_playhead`, `sync_selection_to_playhead` are plain Rust not tied to egui and move almost unchanged into a new host; the drawing (`timeline_ui.rs`) and the egui panels in `main.rs` do not, they get rewritten in the web client |

Conclusion: no backend rewrite is needed. What is needed is a new process
that replaces eframe's event loop with a network server, plus a web client
that redoes only the presentation part.

## What does NOT change

- Decode, GPU compositing and encode stay on the PC. The tablet neither
  decodes nor composes anything of its own (except, possibly, the H.264
  video already prepared for the preview — see § Preview channel).
- The data model and the command pattern stay those of `vv-core`: that is
  exactly why this step is feasible at low risk. Every existing `Command`
  (`InsertClip`, `SplitClip`, `MoveClips`, `LinkClips`, `CompositeCommand`,
  ...) becomes an RPC message with almost no changes.

## New component: `vv-server`

A new crate, analogous to `vv-app` but without UI: tokio + axum (or plain
`tungstenite` to stay minimal), owns `Project` + `History` as the single
source of truth, and replaces eframe's `App::ui()` loop with a loop driven
by network messages. It keeps `TimelineAudio`/`drive_playback` alive exactly
as today (at a fixed interval, or better driven by a dedicated timer instead
of egui's repaint cycle).

Items to carry over from `vv-app/src/main.rs` almost unchanged (they are
already plain Rust, covered by the existing unit tests):
`TimelineAudio`, `History`, `group_members`,
`ensure_active_clip_matches_playhead`, `drive_playback`,
`sync_selection_to_playhead`, `split_at_playhead`, `delete_selected`,
`ripple_delete_selected`, `add_media_to_timeline`.

## Control channel (commands + state)

WebSocket, a single endpoint (e.g. `/ws`), JSON messages to begin with
(bincode is a later optimisation, not needed while the project stays on a
LAN with very few clients).

**Client → server**: an application command, mirroring the `vv-core`
`Command`s 1:1:

```json
{ "type": "SplitClip", "track_index": 0, "clip_id": 5, "split_at": 1200 }
{ "type": "RippleDeleteSelected" }
{ "type": "SetSelection", "track_index": 0, "clip_id": 5 }
{ "type": "SetPlayhead", "frame": 1200 }
{ "type": "Undo" }
{ "type": "TogglePlayback" }
```

On the server side, each message translates into a direct call to the
methods already existing on the `VenturiApp` equivalent
(`split_at_playhead`, `ripple_delete_selected`, etc.) or into a
`History::do_command` with the matching `vv-core` `Command` built from the
message fields.

**Server → client**: after every applied command, broadcast the updated
state to all connected clients. To begin with, no diffing: `Project` (clips,
not pixels) is small, a "real" edit (a few hundred clips) stays well under
100KB serialised — simplicity over performance is a non-issue in this range.
Add diffing only if/when it becomes a measured bottleneck, not before.

```json
{
  "type": "ProjectState",
  "project": { "...": "..." },
  "selected": [0, 5],
  "playhead": 1200,
  "playing": false
}
```

### Multiple clients and concurrency

If several tablets/browsers connect at once, decide on a simple rule right
away to avoid conflicting commands: **the server is authoritative and
serial** (a single shared `History`, commands are applied in arrival order,
no application-level locking on the client). No "ownership"/turn concept is
needed yet: given the intended use (one editor at a time moving from the PC
to the tablet, not two people editing together), last-write-wins on message
arrival order is enough. If real collaborative editing is ever needed, it is
a separate project (CRDT/OT), not in scope for this document.

## Video preview channel

The genuinely new and delicate part, to be built in two separate stages.

### Stage A — on-demand request (scrub, pause)

An HTTP endpoint `GET /frame?playhead=1200` that the backend resolves
exactly like the internal viewer does today (compositing the current frame
through `vv-render::Compositor`, SolidColor and Text layers included) and
returns as a single JPEG. The web client shows it in a plain `<img>`. No
streaming concept: one frame per request, the very same composition
pipeline that already runs in `vv-app` today, just exposed over HTTP instead
of drawn onto an `egui::TextureHandle`.

This stage alone already covers "arrange, cut, move clips from the tablet"
with visual feedback for scrubbing — it has the best value/effort ratio and
should be done first.

### Stage B — live playback

During playback, continuous streaming of the composited frames over the
WebSocket channel (binary, not the same JSON channel as the commands — keep
them separate so controls do not queue behind frames). Start from the
simplest possible solution:

- **MVP**: one JPEG frame per binary message, at the timeline frame rate or
  at a reduced rate if bandwidth/CPU must be contained (e.g. 15fps instead of
  25-30 in preview, not in the final export). On a local network bandwidth
  is not the constraint (1080p JPEG ≈ 100-300KB/frame → 3-9MB/s at 30fps,
  well within a decent LAN/WiFi); the real constraint is
  encode+network+decode latency, to be measured before optimising further.
- **If the MVP is not enough** (bandwidth or JPEG encode CPU on the PC
  become a real, measured problem): move to a proper compressed codec
  (H.264) wrapped in small WebSocket frames, decoded in the browser with
  `WebCodecs` (hardware-accelerated, supported on modern Chrome/Safari, so
  on iPad too). Avoid WebRTC unless proven necessary: on a low-latency LAN
  its complexity (SDP/ICE/negotiation) is not needed; it is only needed if
  access from outside the local network is wanted in the future.

## Audio

For the first version: **audio stays local to the PC**, it is not streamed
to the tablet. Reasons: it avoids a whole audio/video sync problem over the
network (jitter, buffering separate from the video), and in the intended use
(desktop PC in the room) whoever is working hears the audio from the PC's
speakers/headphones anyway while watching/controlling from the tablet. To be
revisited only if real use shows that hearing it from the tablet is really
needed (e.g. PC in another room) — at that point: Web Audio API + PCM or Opus
on the same binary channel as the video, with the same WebCodecs vs raw MVP
complexity trade-off.

## Staged plan (recommended order)

1. `vv-server`: control channel (commands + state) over WebSocket, plus the
   HTTP endpoint for the single on-demand frame (Stage A above). Minimal web
   client: draws the timeline from `ProjectState` (canvas or SVG), sends
   commands, shows the current frame as an `<img>` refreshed on every
   scrub/selection/command. Result: cutting, moving and ripple-deleting from
   the tablet, with static visual feedback.
2. Live playback streaming (Stage B, JPEG-over-WS MVP). Result: the tablet
   can also press play and watch playback, not just edit while stopped.
3. (Only if necessary, measured) streaming optimisation (WebCodecs/H.264)
   and/or audio to the tablet.

Do not start stage 3 before verifying that stage 2 is really insufficient
in real use: that is where complexity rises faster than added value.

## Open decisions (to settle in the separate chat)

- Web client framework: vanilla TS + canvas is closest to the "no needless
  overhead" style of the rest of the project; React/Svelte are fine if UI
  development speed is preferred at the cost of a few extra dependencies.
  Neither changes the reasoning above.
- Server library: `axum` (more common, good WebSocket support) vs
  `tungstenite` directly on `tokio` (more minimal). `axum` is recommended
  for less infrastructure code to maintain by hand.
- Name/path of the `vv-server` binary and whether it should share the same
  Cargo workspace as `vv-app` (recommended: yes, same workspace, new member
  `crates/vv-server`, so it reuses `vv-core`/`vv-media`/`vv-render` as path
  dependencies without duplication).
- Whether and when minimal authentication will be needed (even just a static
  token in the query string) before exposing the WebSocket beyond
  `localhost` — not necessary while it stays bound to a trusted LAN
  interface, but to be decided explicitly before opening the port beyond
  `127.0.0.1`.

## Testing

Consistent with the approach already used in the rest of the project (pure
functions extracted on purpose to be testable, no decoder/encoder mocks):
the message→`Command` translation logic lends itself to pure tests without
network (given a JSON message, check which `Command`/method is invoked); the
actual WebSocket loop is tested with an in-process test client (e.g.
`tokio-tungstenite` against a server started on a local port in the test),
following the same "real I/O, not mocks" principle already used for ffmpeg
in the `vv-media` tests.
