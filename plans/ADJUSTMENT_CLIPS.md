# ADJUSTMENT_CLIPS — plan for Resolve-style adjustment clips

An adjustment clip is a clip with no content of its own that sits on a video
track: its effects (filters, transform, crop, opacity, composite mode) apply
to **the composite of every track below it**, for the frames it covers.
Resolve: Effects Library → Toolbox → Effects → "Adjustment Clip".

**Status:** implemented (all steps in §7).

Also closes the "Adjustment Clip" gap on Resolve OTIO import, today left as
an `UnsupportedReference` warning on purpose.

---

## 1. Semantics

Resolve's model, which we copy:

- It only sees what is **below** it on the timeline (lower track index in
  compositing order); tracks above are unaffected.
- Its output **replaces** the stack below: the processed stack, with the
  timeline background (black; transparent inside a compound clip) wherever
  the transform or the crop leave the frame uncovered — verified in
  Resolve, zooming an adjustment clip out shows black. Then:
  - opacity < 100% (and fades) = mix between that result and the original;
  - composite mode other than Normal = that result blended onto the
    original (e.g. Screen for a glow).
- Transform (zoom/position/rotation/flip) moves the whole composite below:
  the typical use is a shared zoom, a shake, a reframe across many clips.
- Inside a compound clip it only affects the nested timeline's tracks below
  it; over empty nested tracks it produces transparency (nothing to adjust).
- Gaps below it are black on the main timeline (opaque clear): the
  adjustment processes black, as in Resolve.

---

## 2. Model (`vv-core`)

- New variant `ClipSource::Adjustment`. A generator like `SolidColor`/`Text`:
  rate 1, `source_offset` 0, unlimited length, no media, no audio. Additive
  serde variant: old projects load unchanged (projects containing it do not
  open in older builds — acceptable, same as when `Text` arrived).
- Nothing new in `EffectStack`: the adjustment uses `transform`, `filters`,
  `blend_mode`, fades. `color`, `title`, `gain_db`, `speed` stay unused.
- Video tracks only, like the other generators.

The exhaustive `match`es on `ClipSource` will point at every place to touch
(≈ 15, list in §5); most just join the `SolidColor | Text` arm.

---

## 3. Rendering (`vv-render`)

The compositor already has what is needed: non-Normal blend modes copy the
stack composed so far into a scratch texture (`backdrop`) and sample it.

- New `LayerContent::Adjustment` (no payload).
- In `render_layers_to_texture_with_clear`, for an adjustment layer:
  1. if it is the first layer, clear first (as for the backdrop today);
  2. `copy_texture_to_texture` output → scratch texture (the same
     `backdrops` pool, returned at the end);
  3. draw it through the `texture_bind_group` / `Fill::Rgba` path with
     `source_size = timeline_size`, so crop and position are in timeline
     pixels, as for `Solid`/`Text`, on the REPLACE pipeline: the shader
     (`adjusted`) fills the uncovered area with the clear color, applies the
     blend mode, and mixes with the original by the opacity.
  The **same copy** serves as both source and backdrop: one copy per
  adjustment layer, never two.
- Cost: one full-frame GPU copy + one pass per adjustment layer. No
  readback, no CPU work. Works unchanged for preview (reduced resolution:
  the copy is at output size, `source_size` keeps the units right), export
  (`render_layers_i420`) and compound clips (transparent clear → the copy is
  premultiplied, which `Fill::Rgba` already un-premultiplies).
- Filters: they already run per pixel in `shade()` for `Fill::Rgba`, so
  Grayscale works as-is. Future spatial filters (blur, sharpen) need
  neighbour sampling, which a texture source allows: adjustment clips are in
  fact the natural home for them.

Tests (`crates/vv-render/src/tests/compositor.rs`): Grayscale adjustment over
a coloured solid → grey; tracks above it untouched; opacity 50% → mix;
crop → only the uncropped region changes; over a transparent stack
(compound case) → stays transparent.

---

## 4. Preview/export plumbing (`vv-app`)

- `frame_provider`:
  - `OwnedContent::Adjustment` and its `as_render`;
  - `build_layer`: `ClipSource::Adjustment` → `OwnedContent::Adjustment`,
    with transform/filters/opacity/blend taken as for the other clips;
  - `clip_source_size`: the timeline's size;
  - `renders_same`: `Adjustment == Adjustment` when the other fields match.
    Correct because stacks are compared position by position: equal layers
    below imply an equal backdrop.
- `render_ahead`, mixer, thumbnails, waveforms: nothing to decode, they
  already skip every non-`Media` source.
- Transitions (Push) on an adjustment clip: **not supported in v1**. In a
  crossing the second side would sample a backdrop that already contains the
  first side's output (effect applied twice). The timeline refuses the drop
  on an adjustment clip's edge (or on a neighbour's edge towards one), and
  rendering ignores any that still arrive (paste attributes, OTIO):
  `Track::crossing_at` skips crossings involving one,
  `Clip::transition_offset_at` returns no offset. Fades stay allowed.

---

## 5. UI

- **Effects panel** (`media_pool_ui::effect_item`, `timeline_ui::Generator`):
  new `Generator::Adjustment`, in its own "Effects" section (Resolve's
  Toolbox), draggable onto a video track like Solid/Text, default length
  like Text. `insert_adjustment_clip` in `main.rs`, same
  shape as `insert_text_clip`.
- **Timeline** (`timeline_ui.rs` ≈4990, ≈5700): label "Adjustment Clip",
  its own default body colour (distinct from generators and media), no
  filmstrip.
- **Properties panel**: Video tab with Transform, Cropping, Composite mode,
  Opacity; Effects tab with filters. Hidden: Title, Color, Audio. Label in
  the header (`properties_panel.rs` ≈1219). An `is_adjustment` in
  `PanelTarget` only if hiding sections requires it.
- **Viewer overlay**: the transform gizmo works on the timeline-sized frame,
  as for Solid/Text — to be checked, no changes expected.
- **Selection follows playhead**: an adjustment on the top track becomes the
  active clip. It is what Resolve does; keep it.
- Paste attributes, copy/paste, split, trim, ripple, link groups: generic
  on `Clip`, only the `ClipSource` matches in `main.rs`/`paste_attributes.rs`
  (offline check → `false`, retime → keep `rate`, cycle check → `true`,
  label).
- Locales: `generator.adjustment` in en/it.

---

## 6. OTIO

- **Import** (`otio/import.rs`): a Resolve adjustment clip is a `Clip.2`
  with a `MissingReference` and a `Resolve_OTIO` effect of `"Type": 74`
  (`"Effect Name": "Effect"`). Verified on `FULL_TIMELINE_EMA.otio`: 25
  adjustment clips, 25 Type-74 effects, no other `MissingReference` clip
  carries one (Text+, Fusion Composition, TypeFlow do not). Detect on the
  Type 74, not on the name (renamable). Its Transform/Cropping/Opacity/
  Composite are already read by `resolve.rs` like any clip; OFX effects on
  it stay "unsupported" warnings.
- **Export** (`otio/export.rs`): mirror the same shape — name
  "Adjustment Clip", `MissingReference`, the Type-74 effect next to the ones
  `resolve::clip_effects` writes — so Resolve recognises it, and the
  `metadata.venturi` round trip covers the rest. To verify by importing the
  export into Resolve.

Tests in `crates/vv-core/src/tests/otio/`: import of a minimal fixture
(Type 74 → `ClipSource::Adjustment`, transform kept); export → import round
trip.

---

## 7. Steps

| # | Step | Depends on |
|---|------|------------|
| 1 | `ClipSource::Adjustment` + all `match` arms compiling (no UI yet) | — |
| 2 | Resolve check of the zoom-out behaviour — done: black | — |
| 3 | `LayerContent::Adjustment` in the compositor + render tests | 1, 2 |
| 4 | `frame_provider` plumbing; preview and export render it | 3 |
| 5 | Effects panel entry, insertion, timeline look, properties panel | 4 |
| 6 | Transitions refused on adjustment clips | 5 |
| 7 | OTIO import (Type 74) and export + tests | 1 (render: 4) |
| 8 | Headless check in the podman container: screenshot with a Grayscale adjustment over two tracks; export and compare a frame | 5 |

Out of scope: new filters. With Grayscale as the only filter today the
feature is useful mostly for shared transforms/crop/opacity; its value
grows with every filter added afterwards (colour correction, blur), which
need no further adjustment-specific work.
