# REFACTOR_PIPELINE — rules and status of the rendering pipeline

The pipeline refactor is almost done: only the worker pool (§3.4) is left.
This document keeps the invariants and constraints the code must keep
honouring, plus the identifiers (A1…B5, §x) cited in code comments. The
current architecture is described in [ARCHITECTURE.md](../ARCHITECTURE.md);
the old code and the original diagnosis are in the git history.

---

## 1. Starting problems and status

| # | Problem solved | Current solution |
|---|----------------|------------------|
| A1 | Two disconnected eviction mechanisms (positional + capacity LRU) contradicting each other | A single `SharedFrameCache::reconcile` pass (§2) |
| A2 | Budget split between media and segments, not proportional to need | Global byte budget, eviction by distance from the playhead (§2) |
| A3 | Fixed seek threshold, not adapted to the real GOP | Adaptive threshold on the observed GOP (§3.3) |
| A4 | A single serial worker with no priority between "needed now" and prefetch | Fill in order of distance from the playhead; the multi-worker pool is still to do (§3.4) |
| B1 | Clip→source frame mapping duplicated between preview and export | `Clip::source_frame_at` + `FrameProvider` trait |
| B2 | CPU→GPU→CPU compositor round trip on every preview frame | `Compositor::render_layers_to_texture` registered in `egui-wgpu` |
| B3 | Cached frames in RGBA converted on the CPU | YUV420 frames in the cache, conversion in the shader |
| B4 | Rendering limited to one fixed video track and one audio track | N video tracks (composited bottom->top with alpha-over) and N audio tracks (summed) |
| B5 | Per-clip audio instead of a continuous mixer | `vv_audio::Mixer`: sums all tracks and is the playback clock |

---

## 2. Frame cache (Part A)

**A single cache keyed by `(MediaId, source frame)`, a global byte budget,
and one `reconcile()` pass that decides eviction** by priority of distance
from the playhead: for each frame the timeline-space position is computed
(through the window segment containing it), then `|position − playhead|`; a
frame outside every segment has infinite distance.

- **Tier A (window):** drop every frame outside the union of the source
  ranges of its media's current segments. Covers behind the playhead, media
  that left the window, and beyond the lookahead horizon.
- **Tier B (budget):** if over budget, drop the in-window frames
  **farthest** from the playhead.

**⚠️ Tier B cannot use recency LRU.** During the forward fill the frame at
the playhead is the first inserted, so the least recent: an LRU would evict
it first. Recency can only act as a tiebreaker at equal distance.

**Fill order.** The worker decodes in order of distance from the playhead
across all media in the window, up to the budget or until the window is
covered: the frame needed now always arrives before a distant prefetch. The
fill stops when it joins frames already cached, so a small backward scrub
does not re-decode the tail.

### Invariants (covered by the tests in `render_ahead.rs` and `vv-media/src/cache.rs`)
1. **Front at the playhead:** at steady state the buffer starts at the
   current playhead, even when advancing in small steps without a real seek.
2. **Stability with a still playhead:** no cycle invalidates what is already
   correct.
3. **No segment invalidates another:** two segments of the same media in the
   same window do not evict each other, not even with a tight budget.
4. **Coverage under a small budget:** the buffer still starts at the
   playhead.
5. **Backward scrub** (small and large): the buffer reaches the new
   position; a small scrub does not re-decode the tail already cached.
6. **Reused seek:** repositioning an already open media uses `seek_to_time`
   on the existing decoder, never a reopen.

---

## 3. Part A steps

1. ✅ **Fill responsiveness:** the loop rereads the target every N frames and
   interrupts a prefetch that became stale.
2. ✅ **Unified cache + `reconcile()` + global budget + fill by distance** (§2).
3. ✅ **Adaptive seek threshold:** after every real seek the GOP is estimated
   from the landing keyframes; the threshold is about one GOP, with a low
   fallback until there is an observation (erring on the high side is cheap
   because the seek reuses the open decoder).
4. ⏳ **Worker pool with a priority queue:** spread the same fill-by-distance
   order over N threads. Only if one thread cannot keep up; it is the
   riskiest step (concurrency on the shared cache).

---

## Proxy

Low-resolution all-intra copy of each media, generated in the background:
makes every frame reachable with a single decode instead of crossing the
source GOP, so fast scrubbing keeps up. It is not an approximation: the frame
shown is still the exact one at the requested position. Export always uses
the original sources. Details in ARCHITECTURE.md.

---

## 5. Cross-cutting constraints (non-negotiable)

- **Frame accuracy.** Never show an approximate/stale frame to gain
  smoothness: *the frame shown is exactly the timeline frame at the
  playhead*. If a perf proposal requires this trade-off, stop and ask first.
- **One change at a time.** After each step: `cargo build -p vv-app` (not
  just check) + the test suite.
- **Regression tests are the safety net.** They get extended, never
  removed. A test that stops passing after a refactor signals a lost
  invariant: investigate, do not "fix" the test.
- **No special cases per scenario.** `RenderAhead` walks the timeline
  without branches for gap/cut/same media: a single uniform walk.
