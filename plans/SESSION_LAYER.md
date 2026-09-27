# SESSION_LAYER — separate the application core from the egui shell

**Status:** approved 2026-09-27, in progress on branch `session_layer`. Prerequisite of `MCP_SERVER.md`.

## Problem

`vv-core` already is a clean model (`Project`, `Command`, `History`), and
most heavy modules of `vv-app` are already egui-free (`export`,
`import_worker`, `relink_job`, `forced_relink`, `frame_provider`, the
waveform/proxy/thumbnail workers). What is missing is the layer between
them: the *document lifecycle* lives as fields and methods of `VenturiApp`,
next to textures, dialogs and gesture state:

- project + history + path + saved generation + `unsaved_media`;
- the running jobs (`pending_import`, `import_queue`, `pending_otio_import`,
  `relink_job`, `export`) and their pollers, which take an `egui::Context`
  only to request a repaint;
- editing operations that read selection/playhead instead of taking
  explicit targets (`split_at_playhead`, `delete_selected`,
  `ripple_delete_selected`, `add_media_to_timeline`, …);
- model fix-ups done by the UI flow after edits
  (`sync_root_timeline_media` → `Project::sync_compound_meta`; the
  overlap cutting after bulk moves, `cut_remaining_overlaps`).

Any non-GUI driver (MCP, a future CLI export or scripting) has to either
run the whole `VenturiApp` or duplicate this logic.

## Why not "MVC"

egui is immediate mode: the view reads the model every frame, there are
no observers to wire up, and `History::generation()` already is the change
signal. Views applying `Command`s directly (`timeline_ui` has ~20
`do_command` sites) are fine: commands *are* the controller boundary. The
split that pays off is **session core vs. UI shell**, not M/V/C.

## Target shape

New crate `vv-session` (depends on `vv-core`, `vv-media`, `vv-render`,
`vv-audio`; no egui, no eframe, no rust-i18n):

```rust
pub struct Session {
    pub project: Project,
    pub history: History,
    path: Option<PathBuf>,
    saved_generation: u64,
    unsaved_media: bool,
    jobs: Jobs,            // import queue, OTIO import, relink, export
    waker: Arc<dyn Fn() + Send + Sync>,
}

impl Session {
    pub fn new(waker) -> Self;
    pub fn apply(&mut self, cmd: Box<dyn Command>);
    pub fn begin_group / end_group;
    pub fn new_project / open(path) / save(path?) / is_dirty();
    pub fn import_media(paths, folder) -> JobId;
    pub fn import_otio(path) -> JobId;
    pub fn relink(...) -> JobId;
    pub fn export(timeline, settings) -> JobId;
    pub fn cancel(JobId);
    pub fn tick(&mut self) -> Vec<SessionEvent>;
}

pub enum SessionEvent {
    ImportProgress { job, done, total },
    MediaImported { job, media: Vec<MediaId>, warnings: Vec<Warning> },
    OtioReady { job, needs_merge_decision: bool, warnings },
    RelinkDone { job, outcome },
    ExportProgress { job, .. }, ExportFinished { job, result },
}
```

- **Waker**: workers call it when there is something to poll. The GUI
  passes `ctx.request_repaint`, headless a channel notify. This replaces
  the `&egui::Context` parameters of the pollers.
- **Errors/warnings** become typed enums; the app translates them with
  `t!()` (today `export.rs` has 4 `t!()` calls, the only i18n outside UI
  code).
- **Fix-ups**: `sync_compound_meta` runs in `Session::tick` keyed on
  `history.generation()` (as today), so it also covers the timeline
  gestures that apply commands straight through `History`. Overlap
  cutting belongs to the operation, so it moves into `edit::move_clips`.
- **Stays in `vv-app`**: selection, playhead, active timeline and
  timeline stack, viewer (`render_ahead`, compositor textures, browsing),
  `timeline_audio`, proxy/thumbnail/waveform workers and caches (they react
  to `MediaImported`), all dialogs. Dialogs that need a user decision (OTIO
  merge, unsaved changes, forced relink) are UI over a session state.
- **Editing ops**: `vv_core::edit` (model-only, no ffmpeg) with explicit
  targets, as in the MCP plan. They receive `&mut Project, &mut History`
  and apply their commands; the caller owns the undo group (a
  "return commands" API does not work for `delete_ranges`: lift and ripple
  depend on the state after the split).

`VenturiApp` becomes `{ session: Session, ui state… }`.

## Steps

Each step is a pure move/refactor, builds, and passes
`timeout 300 cargo test` (`tests/main.rs`, `timeline_ui_gestures.rs`) with
no behaviour change.

| # | Step | Commit |
|---|------|--------|
| 1 | `vv_core::edit`: split/delete/ripple/insert with explicit targets + `delete_ranges`; `VenturiApp` methods become wrappers; tests in `vv-core/src/tests/edit.rs` | |
| 2 | Create `vv-session`; move the egui-free modules (`export`, `frame_provider`, `import_worker`, `relink_job`, `forced_relink` logic, `worker`, `index_media_by_filename`); typed export errors | |
| 3 | `Session` with project/history/path/saved state; `VenturiApp` holds it (mechanical `self.project` → `self.session.project`); fix-ups into `tick` | |
| 4 | Jobs + waker + `tick() -> Vec<SessionEvent>`: import, OTIO import, relink, export; the app's pollers become event handlers | |
| 5 | Open/save/new project into `Session` (the unsaved-changes dialog stays UI) | |
| 6 | Session tests in `vv-session/src/tests/` driving import → edit → save → export without egui | |

Step 3 is the noisiest diff (mechanical renames); step 4 the riskiest
(ordering of the pollers inside `ui()`).

## Relation to other plans

- `CODEBASE_AUDIT.md` 5d (split `show_timeline`) is independent; do it
  before or after, not interleaved.
- `MCP_SERVER.md` starts after step 4 (step 5-6 can overlap): the headless
  MCP host runs on a bare `Session`, no `VenturiApp`.

## Decisions on the former open points

- Proxy/waveform/thumbnail workers stay in the app: thumbnails are egui
  textures, waveforms only feed the timeline drawing, proxies follow user
  settings; headless needs none of them. The export → proxy pause becomes a
  reaction to the export events.
- Crate name: `vv-session`.
- `project` and `history` are public fields (keeps disjoint borrows such as
  `show_timeline(&mut s.project, &mut s.history, …)`); path, saved state,
  jobs are private, since the dirty-flag invariants live there.
