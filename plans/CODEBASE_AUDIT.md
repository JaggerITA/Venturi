# CODEBASE_AUDIT — duplicated logic, duplicated data structures, inefficiencies

Audit of 2026-09-24 over the ~31k non-test lines of the five crates. Nothing
was changed by the audit itself; the "Plan" section at the bottom tracks what
has been done since. Items touching `render_ahead` buffering/eviction are
**proposals to discuss before coding**. None of the proposals trades frame
accuracy for speed.

Line numbers refer to the code at the time of the audit and will drift.

---

## 1. Inefficiencies (by impact)

### 1.1 Viewer recomposes and re-uploads textures on every repaint — high
`main.rs` `ui()` → `timeline_video_layers()` + `show_composited()` ran on
**every** `ui()` call, even with playhead, project and frames unchanged: a
mouse move, a hover or the audiometer animation redid the composition and
re-uploaded every layer's YUV planes (`compositor.rs` `plane_texture` →
`write_texture`), ~12 MB per 4K frame.
Fix: skip the composition when what would be drawn is identical to what the
texture already shows (see Plan, step 1).

### 1.2 Every history change clones the whole Project and closes every decoder — high (render_ahead design: discuss first)
- `main.rs` `sync_render_ahead`: on every `generation` change,
  `project.clone()` (deep copy of all timelines and keyframes) →
  `render_ahead.rs` `Command::UpdateProject` does `open.clear()` and
  `open_behind.clear()`.
- While dragging a properties-panel slider, `apply_effect_changes`
  (`properties_panel.rs`) runs one `do_command` **per frame**: one project
  clone per frame, decoders reopened as soon as decoding is needed, GOP
  estimate (`estimated_gop`) lost.
- Proposal: do not clear `open` on `UpdateProject` — `position_decoder`
  already drops a decoder whose `resolved_path` changed, and
  `walk_and_fill`'s `open.retain` already prunes media out of the window.
  For the clone: share an `Arc<Project>` snapshot.

### 1.3 "Buffered" strip recomputed every UI frame with a full cache scan — medium
`main.rs` `buffered_timeline_ranges` → `cached_ranges_of_timeline` →
`SharedFrameCache::cached_ranges` (`cache.rs`): per media it locks the mutex
shared with the worker, scans **all** entries, sorts, compacts, ~60x/s.
Fix: recompute only when a cache generation counter changes, or maintain the
ranges incrementally in `insert`/`reconcile`.

### 1.4 Double copy per frame in the decoder — medium
`decode.rs` `yuv420_from_decoded`: even when the source is already `YUV420P`
(the common 8-bit H.264 case) it goes through `sws_scale` (copy 1 + a new
`frame::Video` allocation) and then `pack_plane` (copy 2). Fix: when
`format == target_format`, compact straight from the decoded frame.
The proxy path (`proxy.rs` `write_frame`) goes the other way round again:
`FrameYuv420` → `fill_plane` into a new ffmpeg frame → scaler.

### 1.5 Synchronous probes on the UI thread — medium
- OTIO import (`project_io.rs` `import_otio_from`, called from
  `poll_pending_dialog`) runs `vv_media::probe` for every media
  synchronously: ~100 ms per video, 50 media = ~5 s of frozen UI.
  `ImportWorker` already does this in parallel.
- `main.rs` `preview_media` opens a full `Decoder` just to check the file
  opens; the render_ahead worker reopens it right after.
- `project_io.rs`: `audio_streams()` per media on project load (legacy
  projects only).

### 1.6 Export audio: whole files decoded, compound mixdowns not cached — medium
- `export.rs` `mix_audio_track` decodes **whole** audio files into RAM at
  48 kHz even when exporting a few seconds of a long file (1 h stereo
  ≈ 1.4 GB per stream).
- `compound_mix_buffer` has no cache: a compound used N times is remixed N
  times, multiplied again for nested compounds.

### 1.7 Minor
- `Timeline::active_video_clips_at` / `active_video_clip_at`: linear `find`
  on already sorted clips (`partition_point` would do), several times per
  frame. `Track::crossing_at`: two linear `find`s per crossing.
- `main.rs` `timeline_video_layers` reads `std::env::var("VV_DEBUG_RENDER_AHEAD")`
  every frame; `render_ahead.rs` already caches it in `debug_enabled()`.
- `render_ahead` calls `proxy_exists` (a filesystem `stat`) per segment every
  50 ms cycle, even when settled.
- `History` undo stack is unbounded.

---

## 2. Duplicated logic

### 2.1 Audio mix construction: 4 copies of the same walk — high
"`audible_tracks` → enabled clips → `ClipSource::Media` → compound or file →
`mix_clip_from`" lives in:
- `vv-audio/src/mixer.rs` `MixSnapshot::from_timeline`
- `mix_buffers.rs` `get_or_compute_compound_at_depth` (hand-rewritten because
  of a borrow conflict)
- `timeline_audio.rs` `sync` (a third walk just to precompute compound
  buffers)
- `export.rs` `collect_audio_streams` + `compound_mix_buffer` (a second
  compound mixdown, uncached)

Proposal: a single `from_timeline` with one callback
`FnMut(&MediaItem, &Clip) -> Lookup` (Ready / NotReady / NoAudio): the
two-`&mut`-closures conflict disappears and the compound mixdown (with its
cache) lives in one place.

### 2.2 Recursive compound walk: 6 implementations, 2 different limits
`MAX_COMPOUND_DEPTH = 16` defined in `render_ahead.rs`, `export.rs`,
`mix_buffers.rs`, `frame_provider.rs`; `MAX_COMPOUND_WALK_DEPTH = 8` in
`main.rs`. The "clip → media → `compound?` → nested timeline" logic is
rewritten in `clipped_media_segments`, `push_borrowed_segment` (restarts at
depth 0), `cached_ranges_of_timeline`, `ensure_compound_waveform`,
`collect_audio_streams`, `nested_timeline_of`.
Proposal: one constant in `vv-core` and a
`Project::resolve_clip_media(clip) -> Real(&MediaItem) | Compound(&Timeline) | None`.

### 2.3 "Visible video tracks, enabled clips, media" filter written by hand
In `render_ahead.rs` (segments), `main.rs` `window_frame_bytes`,
`cached_ranges_of_timeline`, `proxy_timeline_ranges` — with **inconsistent**
filters (see §4).

### 2.4 Decoder open/positioning and ffmpeg boilerplate
- `if is_image { open_image } else { open }` in `export.rs` and
  `render_ahead.rs`; `if is_image { probe_image } else { probe }` in
  `import_worker.rs` and `project_io.rs`.
- `secs = frame / fps.max(1e-9)` + `seek_to_time` three times, while
  `seek_to_time` immediately converts back to a frame index. Proposal:
  `Decoder::open_media(path, is_image)` and `Decoder::seek_to_frame(idx)`.
- "`format::input` → `best(Video)` → index/time_base" 5 times across
  `probe.rs`/`decode.rs`; audio stream enumeration 3 times (`probe`,
  `audio_streams`, `audio_stream_timing`); `threading::Config{Frame,0}` in
  `decode.rs` and `probe.rs`. `MediaMeta` could carry the
  `Vec<AudioStreamInfo>` instead of `probe` and `audio_streams` both scanning.

### 2.5 Timeline UI: fade and transition gestures cloned
`fade_drag_value` ≡ `transition_drag_value` (only the clamp differs, 0 vs 1);
`begin_fade_drag` ≈ `begin_transition_drag`. The doc comment of
`adjacent_clip` sits above `transition_drag_value`.

### 2.6 Model
- `Clip::transition_offset_at`: the `transition_in`/`transition_out` blocks
  are nearly identical.
- Fade ramp written twice: `Clip::fade_multiplier_at` and `mixer.rs`
  `fade_multiplier` → a `fn fade_ramp(pos, len, in, out)` in `vv-core`.
- `WantedRange::timeline_position_of` reimplements `Clip::timeline_frame_at`.
- `chunk_forward_segments_near_to_far` / `chunk_behind_segments_near_to_far`
  share the chunk construction.

### 2.7 Commands
`SetClipsDisabled` and `SetClipsDisplayColor` are the same command on a
different field; a multi-clip `SetClipsValue<T>` (like `SetClipValue<T>`)
would replace both.

### 2.8 Misc
- Four time formatters: `format_timecode` (`timeline_ui.rs`, takes seconds
  and converts back to frames), two same-named `format_duration` with
  different formats (`timeline_ui.rs`, `main.rs`), `format_elapsed`
  (`project_io.rs`).
- ffmpeg `ensure_init()` duplicated in `probe.rs` and `stretch.rs`.
- Two different types both named `ProbeResult` (`import_worker.rs`,
  `otio/import.rs`).
- "Load waveform or mark missing" block twice in `main.rs`
  (`ensure_waveforms_loaded` / `ensure_compound_waveform`).
- YUV→RGB Kr/Kb coefficients in `thumbnail.rs` and again in the shader.
- mtime-in-seconds computation duplicated between `content_fingerprint` and
  the `measured_peak_fps` cache key (`probe.rs`).

---

## 3. Duplicated data structures

| Duplicate | Where | Note |
|---|---|---|
| `MediaSegment` ≡ `WantedRange` | `render_ahead.rs`, `vv-media/src/cache.rs` | Same 5 fields and types, plus a field-by-field conversion in `walk_and_fill` |
| `FadeDragState` ≡ `TransitionDragState` | `timeline_ui.rs` | Identical |
| 8 mutually exclusive `Option<…Drag>` in `TimelineState` | `timeline_ui.rs` | `drag`, `marquee`, `trim`, `fade_drag`, `transition_drag`, `crossing_drag`, `transition_duplicate_drag`, `volume_drag`: an `enum Gesture` would make exclusivity a type invariant |
| `OwnedLayer` ≅ `vv_render::Layer` | `frame_provider.rs`, `compositor.rs` | 4 variants each repeating `transform/opacity/filters/blend`: `struct LayerCommon` + an enum of the content only; `as_render`/`layer_blend` collapse |
| `FrameYuv420` ≅ `YuvFrame` | `decode.rs`, `compositor.rs` | Justified by the dependency direction (documented), but field names differ (`u_width` vs `chroma_width`) |
| Hand-written `ALL`/`id`/`from_id`/`label` enums | `ProxyQuality`, `Language`, `Action`, `VideoCodec`, `AudioCodec`, `TransformParam`… | `strum` candidates; `ALL_FILTER_KINDS`/`ALL_TRANSITION_KINDS` live in `timeline_ui.rs` instead of on the model enums |

---

## 4. Inconsistencies born from the duplication (potential bugs)

1. Different clip/track filters: `active_video_clips_at` excludes disabled
   clips and muted tracks, `active_video_clip_at` does not — "selection
   follows playhead" can select a disabled clip the viewer does not show.
   `cached_ranges_of_timeline` and `proxy_timeline_ranges` do not filter
   muted/disabled, unlike `render_ahead`.
2. OTIO import never uses `probe_image`, unlike the other two import paths:
   images in an OTIO are probed as video.
3. Compound depth limit: 8 in the UI vs 16 elsewhere; `push_borrowed_segment`
   restarts at depth 0.
4. Stale doc: `crossing_borrowed_segments` cites `extrapolated_frame_for`,
   which no longer exists (now `held_timeline_frame`, whose semantics — freeze
   on the edge frame — are the opposite of what the doc describes).

---

## 5. Test-only public API and hot spots

Used only by tests: `impl Lerp for Transform` (superseded by
`TransformTracks`), `decode_audio_track` + `AudioBuffer`,
`Compositor::render_layers`, `render_layers_rgba_transparent` — candidates
for `#[cfg(test)]` or `test_support`.

Longest functions: `show_timeline` 1472 lines, `show_properties_panel` 653,
`VenturiApp::ui` 554. Much of the `timeline_ui` duplication (§2.5, §3) stems
from there.

---

## Plan

| Step | Item | Status |
|---|---|---|
| 1 | §1.1 Skip identical viewer recompositions | done (9c5521a) |
| 2 | §2.1 Unify the audio mix (also fixes §1.6 compound caching) | done (a7c3370) |
| 3 | §1.2 Keep decoders across `UpdateProject` (design agreed 2026-09-24) | done — see below |
| 4 | §4 Fix the inconsistencies | todo |
| 5 | §3 Merge `MediaSegment`/`WantedRange`, `LayerCommon`; `enum Gesture` + split `show_timeline` | todo |

### Step 1 notes
The viewer keeps the layers it last composed (`VenturiApp::viewer_content`)
and compares the new ones by content (`frame_provider::renders_same`): same
frame `Arc`s (held, so an address cannot be reused), same transform,
opacity, filters, blend, color, title, and same `OutputFrame`. Anything that
cannot be compared — a compound clip's freshly composed GPU texture —
always recomposes. Equality is on everything the compositor reads, so a
skipped recomposition shows exactly the pixels a new one would.

### Step 2 notes
One walk builds every mix: `MixSnapshot::from_timeline` takes an
`AudioSource` (`vv_audio::mixer`), which answers `Ready`/`Partial`/`Pending`/
`Missing` per file and may cache compound mixdowns; the compound mixdown
itself (`compound_mixdown`) and its depth limit live only in the mixer.
`MixBufferCache` (preview) and the export's `DecodedAudio` implement it; the
export's first pass (`WantedStreams`) only lists the streams to decode, so
`collect_audio_streams`/`compound_mix_buffer`/`get_or_compute_compound` and
`timeline_audio`'s pre-pass are gone. `vv_core::MAX_COMPOUND_DEPTH` replaces
the per-module 16s (the UI's 8 is left for step 4).

Bug fixed on the way: the preview cached a compound mixdown built on a
*partially* decoded buffer (progressive publication), so its tail stayed
silent until `content_hash` changed. `Partial` still plays but is never
cached.

Pre-existing failures, unrelated (also on HEAD before step 1):
`export_timeline_produces_a_playable_file_matching_the_timeline` and
`ease_in_and_ease_out_are_slow_then_fast_and_viceversa` (`Interpolation::PRESETS`
contains `Hold`, which never reaches 1.0).

### Step 3 notes
`UpdateProject` no longer clears every decoder: `forget_changed_media`
(`render_ahead.rs`) drops the decoders **and the cached frames**
(`SharedFrameCache::remove_media`) only of media gone from the new project
or whose `(path, content_hash)` changed. Eviction/budget policy untouched.
The project clone per history change was left as is (the cost was the
decoders, not the clone).

Bug fixed on the way: the frame cache was never invalidated on project
replace, and `MediaId`s are slotmap keys reused by a new project — opening
another project could show the previous project's frames on the new media
(test `a_replaced_project_never_shows_the_previous_projects_frames`).

Measured on `bbb_sunflower_1080p_60fps_normal.mp4` (GOP 250), 6 s of 60 fps
playback with a zoom edit every 100 ms: decoder opens 19 → 2, each reopen
~75–80 ms of worker time (~1.3 s wasted per 6 s before). No missed frames
in either case on this machine: the 3 s lookahead absorbed it.
