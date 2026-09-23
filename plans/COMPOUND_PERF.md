# COMPOUND_PERF — plan for rendering compound clips

Diagnosis and plan born from a reported problem: on a 1080@60 timeline, a
set of clips that renders in real time at normal speed (and holds >1x) can
no longer keep up with 1x playback as soon as it is grouped into a compound
clip.

Complements [REFACTOR_PIPELINE.md](REFACTOR_PIPELINE.md) (§2 cache, B2/B3
compositing); the current architecture is in [ARCHITECTURE.md](../ARCHITECTURE.md).

---

## 1. Where the cost is, today

For **every** compound clip frame, `render_ahead::compose_frame_at`:

1. uploads the YUV planes of the nested layers to the worker's **headless**
   device — a second wgpu device, separate from the egui one used by the UI
   (`main.rs`, `Compositor::new_headless` in the worker vs `Compositor::new`
   with `wgpu_render_state`);
2. render pass → RGBA texture;
3. `Compositor::read_rgba_texture`: `copy_texture_to_buffer` of 8.3 MB +
   `map_read` with `poll(wait_indefinitely)` → **a synchronous GPU stall on
   every frame**, no pipelining. It also allocates a new readback buffer on
   every call (the i420 path pools it instead);
4. row de-padding: another 8.3 MB of memcpy;
5. `frame_provider::rgba_to_yuv420_with_alpha`: **a scalar CPU loop** over
   2.07M px (luma) + 518k 2x2 blocks with divisions (chroma) + alpha plane.
   Single-threaded. On its own it blows the 16.6 ms budget at 60 fps;
6. cache: 3.1 MB YUV + 2.07 MB of alpha → a compound frame weighs ~1.6x a
   normal frame, so it **shrinks the lookahead of every other media** for
   the same budget;
7. UI thread: re-upload of 4 planes to the egui device + `yuv_to_rgb` in the
   shader.

In short: **two full RGB↔YUV round trips and a blocking readback** that
ungrouped clips do not pay, all serialised with decoding in the same worker
thread.

---

## 2. Why flattening is not enough

Idea discarded (but not entirely): expand the compound into its nested
layers with composed transforms and render everything in a single pass, so
that grouping costs nothing.

It is not always valid:

- **Filters.** They are per layer, inside each layer's fragment shader:
  there is no "after the last layer" stage on which to apply the group's
  filters without a rasterised intermediate (which is the compound clip
  itself). Applying them to each nested layer is correct only if the filter
  *distributes* over `over`: true for linear, pointwise functions
  (`Grayscale`, the only one today, is a colour matrix), false for any
  spatial filter (`blur(A over B) != blur(A) over blur(B)`) and any
  non-linearity (gamma, contrast, clamped saturation, curves, chroma key).
  It would work by accident as long as there is only one filter.
- **Group opacity.** With `opacity < 1` and ≥2 overlapping nested layers:
  rasterised fades the group (the lower layer stays covered), flattened
  fades each layer on its own and **the lower layer shows through the upper
  one**. Different images, the second is the wrong one.
- **`Transform` is not closed under composition.** `T_group ∘ T_nested`
  must fit in the fields of `vv_core::Transform` (crop, zoom[2], position,
  rotation, anchor, flip): outer rotation ∘ inner anisotropic zoom ∘ inner
  rotation gives a general 2x2 matrix, not representable. Transform and
  shader would need generalising to a `mat3x3`. On top of that `crop` is in
  native pixels of the single layer's media, while the outer crop is in
  pixels of the nested timeline: different spaces to reconcile.
- **Clipping to the nested canvas.** Whatever leaves the nested timeline's
  edges disappears today; flattened there is nothing left to clip, and a
  layer moved past the margin becomes visible in the outer timeline. It
  would need a per-layer rect clip derived from the group transform. Same
  story for `fit_factors`.

It remains usable as an optional optimisation when the group really
"distributes", but the gain is **a single render pass**: not worth the edge
cases. The real cost is elsewhere (§1).

---

## 3. The plan: shared device, compound as a sub-pass

### 3.1 `Layer::Texture` in the compositor

A variant of `vv_render::Layer` that takes a ready `wgpu::Texture` instead
of the YUV planes. The current bind group layout declares
`TextureSampleType::Float { filterable: true }`, D2: an `Rgba8Unorm` view
**already satisfies it**. So: bind the RGBA texture to slot 0, placeholders
on 1/2/5, a new `Fill::Rgba`, and in `transform.wgsl` a branch that samples
as `vec4` and skips `yuv_to_rgb`. No second pipeline, no second bind group
layout.

`OUTPUT_FORMAT` is `Rgba8Unorm` (not sRGB): no implicit gamma conversion on
sampling.

### 3.2 Premultiplication — there is a latent bug to fix here

The pipeline uses `BlendState::ALPHA_BLENDING`: on a transparent clear the
result is **premultiplied** (`rgb = a·C`, `alpha = a`).
`rgba_to_yuv420_with_alpha` however treats that `rgb` as straight, and the
shader multiplies it by alpha again → **α² on the semi-transparent edges of
a compound clip**, already today. To be confirmed with a test.

In the new path: un-premultiply in the shader (`rgb / max(a, eps)`), which
keeps a single pipeline, or a second blend state
`PREMULTIPLIED_ALPHA_BLENDING` for texture layers only.

### 3.3 The texture pool recycles the output

`render_layers_to_texture_with_clear` does
`give_back(&mut pool.outputs, [output_texture.clone()])` and returns the
same texture: **it is recycled**, the next render of the same size draws
over it. If someone keeps it → silent corruption, not a crash. It needs a
variant that does not put the output back in the pool, or a return to the
pool through the handle's `Drop`.

### 3.4 Device shared with the worker

`RenderAhead::spawn` receives `cc.wgpu_render_state` and builds
`Compositor::new(device, queue)` instead of `new_headless()`.
`wgpu::Device`/`Queue` are `Send + Sync` and already behind `Arc`. Two
`Compositor`s on the same device are fine (the `pool` is one `Mutex` per
instance). Fall back to `new_headless` when there is no render state (tests,
export).

### 3.5 The compound stops being a cached media

This is the part that simplifies the code. Once compositing is on the same
device and costs one pass, there is no reason to *materialise and cache* the
composed frame:

- the worker goes back to **decoding only**, including inside nested
  timelines (the part of `clipped_media_segments` that walks the nesting to
  keep the real media warm stays identical);
- when composing the outer frame, a compound clip produces a
  `Layer::Texture` rendered on the fly into a **transient texture from the
  pool**, released right after. Recursive for nesting.

Gone from `render_ahead.rs`: `compose_compound_segments`,
`MAX_COMPOUND_PASSES`, `CacheOnlyProvider`, `compose_frame_at`, and the
whole second `(real, compound)` channel that today runs through
`collect_media_segments` / `crossing_borrowed_segments` / `walk_and_fill`.
The "layer not ready yet → retry next round" logic goes, and so does the
invalidation through `content_hash` / `Project::touch_compound` for composed
frames: there is nothing stale left to invalidate.

Cost per outer frame: 1 render pass + 1 transient texture per compound
instance. At 1080p60 it is noise. **Zero persistent VRAM, zero readback,
zero conversion.**

Why not cache the composed textures: 1080p RGBA8 = 8.3 MB/frame, a 3 s
lookahead at 60 fps would be 1.5 GB of VRAM. (Note: even today in RAM it is
5.2 MB/frame → 930 MB, kept in check only by eviction.)

Downside: it recomposes on every repaint even with a still playhead, and
there is no reuse when the same compound frame is needed twice (both sides
of a crossing transition that borrows its edge). Mitigated with a tiny LRU —
about ten textures, keyed by `(media_id, local frame, resolution)` — not with
a lookahead-horizon cache.

### 3.6 Export

`export.rs` composes the compound synchronously and wants a `FrameYuv420`
because `FrameProvider::frame_for` returns that. Generalising the return
type (e.g. `enum ProvidedFrame { Yuv, Texture }`) would bring the same gain
to export, which pays the same readback + conversion per compound frame. It
is offline: not urgent, goes to the back of the queue.

---

## 4. Work order and status

1. ~~`Layer::Texture` + `Fill::Rgba` + un-premultiply in the shader (§3.1,
   §3.2)~~ — done.
2. ~~Variant of `render_layers_to_texture` that does not recycle the output
   (§3.3)~~ — done, and it became `PooledTexture`: the intermediate goes back
   to its pool when the layer using it is dropped.
3. ~~Device shared with the worker (§3.4)~~ — **no longer needed**: with
   §3.5 the worker composes nothing, so it needs no compositor.
4. ~~Compound as a sub-pass (§3.5)~~ — done: `frame_provider::GpuCompounds`
   composes the nested timeline on the fly; the compound channel,
   `compose_compound_segments`, `CacheOnlyProvider` and `compose_frame_at`
   are gone from `render_ahead.rs`.
5. ~~Export on the same path (§3.6)~~ — done: same `GpuCompounds`, with the
   two GPU stages on a shared compositor (the texture is born in decode and
   sampled in composition). `rgba_to_yuv420_with_alpha` went away with its
   last caller.

Still to measure on real footage: whether 1080@60 now holds playback, and
whether the variance on the UI thread justifies the short ring of §5.

**Do not touch `FrameYuv420::alpha`.** It had been left without producers
when compound clips stopped going through the CPU, and looked dead — but it
served a case that *did not work at all*: the scaler converted everything to
YUV420P, so an imported PNG with transparency arrived opaque. Now the
decoder fills it (YUVA420P when the source format has real alpha), and all
the plumbing up to the shader was already in place.

**Known gap:** a *video* with alpha (WebM VP9, ProRes 4444) that gets a
proxy loses it, because the proxy is H.264. Images get no proxy
(`project_io.rs`), so PNGs are safe; videos with alpha would need to skip
proxy generation, which means a `has_alpha` in `MediaMeta`.

---

## 5. Why not cache the composite instead of the media

Recurring question: the per-media cache (`SharedFrameCache`, keyed by
`(MediaId, source frame)`) was decided when there was no compositing; do
compound clips change the assessment? Would it be faster, or cleaner, to
cache the last stage of the pipeline — the composited timeline frame?

**No.** The premise does not hold: compounds are not slow because
compositing is expensive, but because *that* path does a blocking readback +
CPU conversion + a second device (§1). Remove that, and composing a nested
timeline is one render pass.

Reasons the per-media cache stays the right choice:

- **Identity stability.** A media frame is invalidated only by the file
  changing or by the proxy toggle. A composited frame depends on the whole
  project state at that frame (transform, keyframes, opacity, filters,
  transitions, track order/mute/solo), recursively on the nested timelines,
  and on the viewer resolution (`OutputFrame::scaled` composes at the
  resolution of the decoded frame, not the timeline's). A **ripple edit**
  shifts the index of every downstream frame by N: the whole composite
  cache dies, the media cache does not notice. Knowing *which* frames to
  invalidate is real dependency tracking; the conservative answer is "all".
- **You cache what is expensive to recompute and not randomly accessed.**
  Decoding is both — seek, GOP, transit: the whole `render_ahead.rs`
  machinery (`BEHIND_CHUNK_FRAMES`, adaptive seek threshold,
  `TRANSIT_SAFETY_CAP_FRAMES`) exists only for that. Compositing is
  stateless and random access.
- **Deduplication.** A media frame serves several timeline frames
  (`rate < 1`) and several clips (duplicated clip, same media in two places,
  reused compound). The composite deduplicates nothing.
- **Density.** 1080p composite = 3.1 MB I420 / 8.3 MB RGBA, media frame =
  3.1 MB: the composite wins only with many active layers. On a single-track
  timeline it is equal or worse.
- **The dominant interaction is editing, not playback.** A cache whose hit
  rate collapses on every edit behaves worst exactly when it hurts most: you
  move a keyframe and re-decode, instead of recomposing from cache.

What is true in the intuition:

- The double `(real, compound)` channel in `render_ahead.rs` exists
  **precisely because** the cache is indexed by media and a compound is not
  a media. A per-timeline-frame cache would remove the special case — but
  §3.5 gets the same cleanup without taking on invalidation, because the
  compound stops being a cached entity.
- A different, valid argument: **variance on the UI thread**. Today all
  compositing happens inside the egui repaint, and the playback clock cannot
  absorb its spikes. A **short ring** of pre-composited textures (0.25–0.5 s,
  15–30 frames, 125–250 MB at 1080p), filled by a worker and thrown away
  entirely on every project change, would reduce the UI thread to a blit.
  Trivial invalidation: the horizon is so short that redoing it is cheap. It
  is a **tier 2 on top of** the media cache, not instead of it, and it is
  independent of compounds: to be evaluated **after** §3, measuring whether
  the variance is a real problem.

What would really change the assessment: truly expensive compositing —
many-layer stacks, heavy spatial filters (blur), 4K multi-stream, effects
with temporal dependency. Even then the answer is not "replace" but a
**tier 3**: an on-disk render cache, opt-in, per section, keyed by a content
hash of the clips' effective state — of which `content_hash` /
`Project::touch_compound` are already the seed.

---

## 6. Quick wins, if a result is needed before the plan

Independent and compatible with the plan:

- RGBA→YUV+alpha conversion **on the GPU**, extending `rgba_to_i420.wgsl` to
  write the alpha plane too: removes the dominant cost (§1.5) and cuts the
  readback from 8.3 to 5.2 MB;
- pool the readback buffer of the RGBA path as the i420 one already does;
- asynchronous readback (submit N frames, map later) instead of blocking per
  frame;
- `rayon` on the conversion loop, if it stays on the CPU;
- compound compositing on a thread of its own, so decoding does not stall.

---

## 7. Constraints not to lose sight of

- **One pass per layer** (`LoadOp::Load` in a separate pass for each): with
  nesting the passes multiply. Independent of this work, but it is the next
  ceiling.
- `OutputFrame::scaled`: the preview composes at the resolution of the
  decoded frame, not the timeline's. The compound texture must be rendered
  at that same resolution, or scaling must be added.
- Concurrent submits on the same queue from UI and worker: wgpu serialises
  internally, and with §3.5 the worker barely touches the GPU any more.
