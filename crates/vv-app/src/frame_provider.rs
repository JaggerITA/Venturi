//! How the decoded frame of a clip is obtained: a cache filled in the
//! background (preview, `render_ahead.rs`) or synchronous decoding (export,
//! `export.rs`). The clip -> source frame mapping is a single one.

use std::sync::Arc;
use vv_core::{Clip, ClipSource, FrameIdx, MediaId, Project, Rgba, TitleParams, Timeline, Transform};
use vv_media::FrameYuv420;

/// `&mut self`: the export keeps the decoders open.
pub trait FrameProvider {
    /// `Ok(None)` = not available now (preview: not cached yet;
    /// export: past the end of the file). A decode error stays `Err`, so
    /// the export does not turn it into a silent black frame.
    fn frame_for(
        &mut self,
        project: &Project,
        clip: &Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<FrameYuv420>>, String>;

    /// The composed frame of `nested` (the nested timeline of a compound
    /// clip) at `local_frame`, as a GPU texture. `None` from the default
    /// implementation: whoever cannot compose on the GPU falls back on
    /// `frame_for`, i.e. on the frame composed and brought back to the CPU.
    /// Whoever implements it composes recursively with `track_layers_at`, passing itself.
    fn compound_texture(
        &mut self,
        _project: &Project,
        _nested: &Timeline,
        _local_frame: FrameIdx,
    ) -> Result<Option<vv_render::PooledTexture>, String> {
        Ok(None)
    }
}

/// The nested timeline of `clip`, if it is a compound clip.
fn nested_timeline_of<'a>(project: &'a Project, clip: &Clip) -> Option<&'a Timeline> {
    let ClipSource::Media(media_id) = &clip.source else {
        return None;
    };
    let nested_id = project.media_pool.get(*media_id)?.compound?;
    project.timelines.get(nested_id)
}

/// `(media, source frame)` of `clip` at `timeline_frame`; `None` if it is not
/// a Media clip.
pub fn media_source_frame(clip: &Clip, timeline_frame: FrameIdx) -> Option<(MediaId, FrameIdx)> {
    let ClipSource::Media(media_id) = &clip.source else {
        return None;
    };
    Some((*media_id, clip.source_frame_at(timeline_frame)))
}

/// *Native* resolution of the media, the unit of the crop even when decoding
/// the proxy. Generators: the timeline's; `(1, 1)` if the media is missing.
pub fn clip_source_size(project: &Project, clip: &Clip, timeline_size: (u32, u32)) -> (u32, u32) {
    match &clip.source {
        ClipSource::Media(id) => project
            .media_pool
            .get(*id)
            .map(|m| (m.meta.width, m.meta.height))
            .unwrap_or((1, 1)),
        ClipSource::SolidColor | ClipSource::Text => timeline_size,
    }
}

/// vv-render does not depend on vv-media: it takes raw byte planes.
pub fn as_render_yuv_frame(frame: &FrameYuv420) -> vv_render::YuvFrame<'_> {
    vv_render::YuvFrame {
        y: &frame.y,
        width: frame.width,
        height: frame.height,
        u: &frame.u,
        v: &frame.v,
        chroma_width: frame.u_width,
        chroma_height: frame.u_height,
        matrix: frame.matrix,
        full_range: frame.full_range,
        alpha: frame.alpha.as_deref().unwrap_or(&[255]),
    }
}

/// A compositing layer that owns what `vv_render::Layer` borrows.
pub enum OwnedLayer {
    Video {
        frame: Arc<FrameYuv420>,
        transform: Transform,
        /// Native resolution of the media (not of the proxy): the units of the crop.
        source_size: (u32, u32),
        opacity: f32,
        /// Only the active filters of `EffectStack::filters`, in their order.
        filters: Vec<vv_core::FilterKind>,
        blend: vv_core::BlendMode,
    },
    /// Compound clip composed on the GPU (see `FrameProvider::compound_texture`).
    Texture {
        texture: vv_render::PooledTexture,
        transform: Transform,
        /// Resolution of the nested timeline: the units of the crop, like
        /// `Video`'s `source_size` (the texture can be smaller).
        source_size: (u32, u32),
        opacity: f32,
        filters: Vec<vv_core::FilterKind>,
        blend: vv_core::BlendMode,
    },
    Solid {
        color: Rgba,
        transform: Transform,
        opacity: f32,
        filters: Vec<vv_core::FilterKind>,
        blend: vv_core::BlendMode,
    },
    Text {
        title: TitleParams,
        transform: Transform,
        opacity: f32,
        filters: Vec<vv_core::FilterKind>,
        blend: vv_core::BlendMode,
    },
}

impl OwnedLayer {
    pub fn as_render(&self) -> vv_render::Layer<'_> {
        match self {
            OwnedLayer::Video {
                frame,
                transform,
                source_size,
                opacity,
                filters,
                blend,
            } => vv_render::Layer::Video {
                frame: as_render_yuv_frame(frame),
                transform: *transform,
                source_size: *source_size,
                opacity: *opacity,
                filters: filters.as_slice(),
                blend: *blend,
            },
            OwnedLayer::Texture {
                texture,
                transform,
                source_size,
                opacity,
                filters,
                blend,
            } => vv_render::Layer::Texture {
                texture,
                transform: *transform,
                source_size: *source_size,
                opacity: *opacity,
                filters: filters.as_slice(),
                blend: *blend,
            },
            OwnedLayer::Solid { color, transform, opacity, filters, blend } => vv_render::Layer::Solid {
                color: *color,
                transform: *transform,
                opacity: *opacity,
                filters: filters.as_slice(),
                blend: *blend,
            },
            OwnedLayer::Text { title, transform, opacity, filters, blend } => vv_render::Layer::Text {
                title,
                transform: *transform,
                opacity: *opacity,
                filters: filters.as_slice(),
                blend: *blend,
            },
        }
    }
}

/// Limit on the nesting depth of compound clips: a compound clip
/// containing itself (`Project::would_create_compound_cycle` prevents it
/// on insertion) would send the composition into infinite recursion.
/// A safety net, not a design limit.
pub const MAX_COMPOUND_DEPTH: u32 = 16;

/// Adds to any provider the GPU composition of compound clips: the nested
/// timeline becomes a texture that stays on the card, instead of a composed
/// frame brought back to the CPU and converted to YUV again.
/// `inner` provides the real media, which the rest of the pipeline decodes
/// as always (including those inside the nested timelines).
pub struct GpuCompounds<'a> {
    inner: &'a mut dyn FrameProvider,
    compositor: &'a vv_render::Compositor,
    depth: u32,
}

impl<'a> GpuCompounds<'a> {
    pub fn new(inner: &'a mut dyn FrameProvider, compositor: &'a vv_render::Compositor) -> Self {
        Self {
            inner,
            compositor,
            depth: 0,
        }
    }
}

impl FrameProvider for GpuCompounds<'_> {
    fn frame_for(
        &mut self,
        project: &Project,
        clip: &Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<FrameYuv420>>, String> {
        self.inner.frame_for(project, clip, timeline_frame)
    }

    fn compound_texture(
        &mut self,
        project: &Project,
        nested: &Timeline,
        local_frame: FrameIdx,
    ) -> Result<Option<vv_render::PooledTexture>, String> {
        if self.depth >= MAX_COMPOUND_DEPTH {
            return Ok(None);
        }
        self.depth += 1;
        let layers = nested_layers(project, nested, local_frame, self);
        self.depth -= 1;
        let Some(layers) = layers? else {
            return Ok(None);
        };
        let render_layers: Vec<vv_render::Layer> = layers.iter().map(OwnedLayer::as_render).collect();
        let (width, height) = nested.resolution;
        // Transparent background: where `nested` has nothing to show, what is
        // below in the outer timeline must stay visible. And the texture does
        // not go back to the pool, because we hold it until the pass.
        Ok(Some(self.compositor.render_layers_to_owned_texture_transparent(
            &render_layers,
            vv_render::OutputFrame::exact(width, height),
        )))
    }
}

/// The layers of `nested` at `local_frame`, or `None` if a clip covering that
/// frame does not have its content yet (a media being decoded): a half-composed
/// frame would be worse than no frame. No active clip is a valid case,
/// not a "not ready": it gives a transparent frame.
fn nested_layers(
    project: &Project,
    nested: &Timeline,
    local_frame: FrameIdx,
    provider: &mut dyn FrameProvider,
) -> Result<Option<Vec<OwnedLayer>>, String> {
    let mut layers = Vec::new();
    for (track_index, clip) in nested.active_video_clips_at(local_frame) {
        let clip_layers = track_layers_at(
            project,
            nested,
            track_index,
            clip,
            local_frame,
            nested.resolution,
            provider,
        )?;
        if clip_layers.is_empty() {
            return Ok(None);
        }
        layers.extend(clip_layers);
    }
    Ok(Some(layers))
}

/// The layer of `clip` at timeline frame `frame`, shared by preview
/// and export. `Ok(None)`: media frame not available.
pub fn clip_layer(
    project: &Project,
    clip: &Clip,
    frame: FrameIdx,
    timeline_size: (u32, u32),
    provider: &mut dyn FrameProvider,
) -> Result<Option<OwnedLayer>, String> {
    let source_frame = clip.source_frame_at(frame);
    let mut transform = clip.effects.transform.value_at(source_frame);
    let push = clip.transition_offset_at(frame, (timeline_size.0 as f32, timeline_size.1 as f32), transform.zoom);
    transform.position[0] += push[0];
    transform.position[1] += push[1];
    let opacity = clip.fade_multiplier_at(frame);
    let content = clip_content(project, clip, frame, provider)?;
    Ok(build_layer(project, clip, source_frame, transform, opacity, content, timeline_size))
}

/// What a clip shows at `timeline_frame`: nothing if it is not a Media clip,
/// the composed texture if it is a compound clip and the provider can compose
/// it on the GPU, otherwise the decoded frame (or composed and brought back to the CPU).
fn clip_content(
    project: &Project,
    clip: &Clip,
    timeline_frame: FrameIdx,
    provider: &mut dyn FrameProvider,
) -> Result<ClipContent, String> {
    if !matches!(clip.source, ClipSource::Media(_)) {
        return Ok(ClipContent::None);
    }
    if let Some(nested) = nested_timeline_of(project, clip)
        && let Some(texture) = provider.compound_texture(project, nested, clip.source_frame_at(timeline_frame))?
    {
        return Ok(ClipContent::Texture(texture));
    }
    Ok(provider
        .frame_for(project, clip, timeline_frame)?
        .map_or(ClipContent::None, ClipContent::Yuv))
}

/// The already obtained content of a clip, see `clip_content`.
enum ClipContent {
    None,
    Yuv(Arc<FrameYuv420>),
    Texture(vv_render::PooledTexture),
}

/// The part common to `clip_layer` and to the side of a crossing transition
/// (`crossing_side_layer`): given the source frame, the already computed
/// transform and the already decoded media frame (if `ClipSource::Media`),
/// it assembles the right `OwnedLayer` for the clip's source kind.
fn build_layer(
    project: &Project,
    clip: &Clip,
    source_frame: FrameIdx,
    transform: Transform,
    opacity: f32,
    content: ClipContent,
    timeline_size: (u32, u32),
) -> Option<OwnedLayer> {
    let filters: Vec<vv_core::FilterKind> = clip
        .effects
        .filters
        .iter()
        .filter(|f| f.enabled)
        .map(|f| f.kind)
        .collect();
    let blend = clip.effects.blend_mode;
    // The clip opacity is multiplied by the one already carried by the
    // fades.
    let opacity = opacity * (transform.opacity / 100.0).clamp(0.0, 1.0);
    match &clip.source {
        ClipSource::SolidColor => Some(OwnedLayer::Solid {
            color: clip
                .effects
                .color
                .as_ref()
                .map_or(Rgba::BLACK, |k| k.value_at(source_frame)),
            transform,
            opacity,
            filters,
            blend,
        }),
        ClipSource::Text => clip
            .effects
            .title
            .clone()
            .map(|title| OwnedLayer::Text { title, transform, opacity, filters, blend }),
        ClipSource::Media(_) => match content {
            ClipContent::None => None,
            ClipContent::Yuv(frame) => Some(OwnedLayer::Video {
                frame,
                transform,
                source_size: clip_source_size(project, clip, timeline_size),
                opacity,
                filters,
                blend,
            }),
            ClipContent::Texture(texture) => Some(OwnedLayer::Texture {
                texture,
                transform,
                source_size: clip_source_size(project, clip, timeline_size),
                opacity,
                filters,
                blend,
            }),
        },
    }
}

/// The timeline frame to take the content from: `timeline_frame`, or that of
/// the nearest source frame if it falls past the real edges of the media
/// (freeze instead of nothing). Used only by the crossing transitions,
/// where "past the end" is the norm — it is the clip lending its edge to the
/// transition — not an error to report as `clip_layer` does on export.
fn held_timeline_frame(project: &Project, clip: &Clip, timeline_frame: FrameIdx) -> FrameIdx {
    let ClipSource::Media(media_id) = &clip.source else {
        return timeline_frame;
    };
    let Some(media) = project.media_pool.get(*media_id) else {
        return timeline_frame;
    };
    let wanted = clip.source_frame_at(timeline_frame);
    let clamped = wanted.clamp(0, (media.meta.duration_frames - 1).max(0));
    clip.timeline_frame_at(clamped)
}

/// One side of a crossing transition for a single clip: like `clip_layer`,
/// but the source frame clamps to the edges of the media instead of
/// disappearing, and the position offset is `CrossTransition`'s instead of
/// `Clip::transition_offset_at` (which concerns only the single edges,
/// `transition_in`/`transition_out`).
fn crossing_side_layer(
    project: &Project,
    clip: &Clip,
    frame: FrameIdx,
    timeline_size: (u32, u32),
    provider: &mut dyn FrameProvider,
    extra_offset: [f32; 2],
) -> Result<Option<OwnedLayer>, String> {
    let source_frame = clip.source_frame_at(frame);
    let mut transform = clip.effects.transform.value_at(source_frame);
    transform.position[0] += extra_offset[0];
    transform.position[1] += extra_offset[1];
    let opacity = clip.fade_multiplier_at(frame);
    let content = clip_content(project, clip, held_timeline_frame(project, clip, frame), provider)?;
    Ok(build_layer(project, clip, source_frame, transform, opacity, content, timeline_size))
}

/// The layers of an active crossing transition at `frame`: tail of `left`,
/// then head of `right` (in that order, `right` on top — for a push it does
/// not matter, the two visible areas never really overlap).
fn crossing_layers(
    project: &Project,
    left: &Clip,
    right: &Clip,
    crossing: &vv_core::CrossTransition,
    frame: FrameIdx,
    timeline_size: (u32, u32),
    provider: &mut dyn FrameProvider,
) -> Result<Vec<OwnedLayer>, String> {
    let frame_size = (timeline_size.0 as f32, timeline_size.1 as f32);
    let progress = crossing.eased_progress_at(frame, left, right);
    // The zoom of each clip at its own source frame: `offsets` needs it to
    // really clear the screen even if one of the two (or both) is zoomed
    // — see `push_clearance`. Recomputed here and again inside
    // `crossing_side_layer`: it costs very little (`Keyframed`), not worth
    // threading through as an extra parameter in several places.
    let left_zoom = left.effects.transform.value_at(left.source_frame_at(frame)).zoom;
    let right_zoom = right.effects.transform.value_at(right.source_frame_at(frame)).zoom;
    let (left_offset, right_offset) = crossing.offsets(progress, frame_size, left_zoom, right_zoom);
    let mut layers = Vec::with_capacity(2);
    layers.extend(crossing_side_layer(project, left, frame, timeline_size, provider, left_offset)?);
    layers.extend(crossing_side_layer(project, right, frame, timeline_size, provider, right_offset)?);
    Ok(layers)
}

/// The layers of `clip` (on track `track_index`) at timeline frame
/// `frame`: only one in the normal case, but two if `frame` falls in the
/// window of a valid crossing transition involving this clip —
/// the other half of the pair adds itself, pushed according to the
/// shared transition. The entry point to prefer over `clip_layer`
/// wherever a whole track is composed (preview and export), not just
/// an isolated clip.
pub fn track_layers_at(
    project: &Project,
    timeline: &Timeline,
    track_index: usize,
    clip: &Clip,
    frame: FrameIdx,
    timeline_size: (u32, u32),
    provider: &mut dyn FrameProvider,
) -> Result<Vec<OwnedLayer>, String> {
    let track = &timeline.tracks[track_index];
    if let Some((left, right, crossing)) = track.crossing_at(frame)
        && (left.id == clip.id || right.id == clip.id)
    {
        return crossing_layers(project, left, right, crossing, frame, timeline_size, provider);
    }
    Ok(clip_layer(project, clip, frame, timeline_size, provider)?.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vv_core::{ClipId, CrossTransition, Ease, PushDirection, Rational, Track, TrackKind, Transform, TransformTracks, Transition, TransitionKind};

    struct NoMediaProvider;
    impl FrameProvider for NoMediaProvider {
        fn frame_for(&mut self, _: &Project, _: &Clip, _: FrameIdx) -> Result<Option<Arc<FrameYuv420>>, String> {
            Ok(None)
        }
    }

    fn solid_clip(id: u64, start: FrameIdx, len: FrameIdx, zoom: [f32; 2]) -> Clip {
        let mut clip = Clip::from_source_range(ClipId(id), ClipSource::SolidColor, 0, len, start, Rational::one());
        clip.effects.transform = TransformTracks::constant(Transform {
            zoom,
            ..Default::default()
        });
        clip
    }

    fn position_of(layer: &OwnedLayer) -> [f32; 2] {
        match layer {
            OwnedLayer::Solid { transform, .. } => transform.position,
            _ => panic!("expected a Solid layer"),
        }
    }

    /// Bug reported by the user: a compound clip is a timeline like any
    /// other, so the areas where its nested timeline has nothing to show
    /// must stay transparent — not black, or they would cover what is
    /// below in the timeline containing it.
    #[test]
    fn an_empty_area_of_a_compound_clip_shows_the_layer_below_it() {
        let mut project = Project::default();
        // Nested timeline: a red covering only the left half.
        let mut red = solid_clip(1, 0, 10, [1.0, 1.0]);
        red.effects.color = Some(vv_core::Keyframed::constant(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }));
        red.effects.transform = TransformTracks::constant(Transform {
            crop: [0.0, 0.0, 2.0, 0.0],
            ..Default::default()
        });
        let nested_id = project.timelines.insert(Timeline {
            name: "Nested".into(),
            fps: Rational::new(25, 1),
            resolution: (4, 4),
            tracks: vec![video_track(vec![red])],
        });
        let compound_media = project.media_pool.insert(compound_media_item(nested_id, (4, 4), 10));

        // Outer timeline: a blue below, the compound clip above.
        let mut blue = solid_clip(2, 0, 10, [1.0, 1.0]);
        blue.effects.color = Some(vv_core::Keyframed::constant(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }));
        let compound_clip = Clip::from_source_range(
            ClipId(3),
            ClipSource::Media(compound_media),
            0,
            10,
            0,
            Rational::one(),
        );
        let outer = Timeline {
            name: "Outer".into(),
            fps: Rational::new(25, 1),
            resolution: (4, 4),
            tracks: vec![video_track(vec![blue]), video_track(vec![compound_clip])],
        };

        let compositor = vv_render::Compositor::new_headless();
        let mut inner = NoMediaProvider;
        let mut provider = GpuCompounds::new(&mut inner, &compositor);
        let mut layers = Vec::new();
        for (track_index, clip) in outer.active_video_clips_at(0) {
            layers.extend(
                track_layers_at(&project, &outer, track_index, clip, 0, outer.resolution, &mut provider).unwrap(),
            );
        }
        assert_eq!(layers.len(), 2, "il blu e la compound clip");

        let render_layers: Vec<vv_render::Layer> = layers.iter().map(OwnedLayer::as_render).collect();
        let out = compositor.render_layers_rgba_transparent(&render_layers, vv_render::OutputFrame::exact(4, 4));
        let px = |x: usize, y: usize| &out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(px(0, 1), &[255, 0, 0, 255], "sinistra: il rosso della timeline annidata");
        assert_eq!(px(3, 1), &[0, 0, 255, 255], "destra: vuota nella annidata, si vede il blu sotto");
    }

    /// Until a media inside the nested timeline is ready, the compound clip
    /// produces no layer: better no frame than a half-composed
    /// frame.
    #[test]
    fn a_compound_clip_has_no_layer_until_its_nested_media_is_ready() {
        let mut project = Project::default();
        let missing_media = project.media_pool.insert(vv_core::MediaItem {
            path: "a.mp4".into(),
            meta: vv_core::MediaMeta {
                duration_frames: 10,
                fps: Rational::new(25, 1),
                width: 4,
                height: 4,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: None,
        });
        let nested_id = project.timelines.insert(Timeline {
            name: "Nested".into(),
            fps: Rational::new(25, 1),
            resolution: (4, 4),
            tracks: vec![video_track(vec![Clip::from_source_range(
                ClipId(1),
                ClipSource::Media(missing_media),
                0,
                10,
                0,
                Rational::one(),
            )])],
        });
        let compound_media = project.media_pool.insert(compound_media_item(nested_id, (4, 4), 10));
        let clip = Clip::from_source_range(ClipId(2), ClipSource::Media(compound_media), 0, 10, 0, Rational::one());
        let outer = Timeline {
            name: "Outer".into(),
            fps: Rational::new(25, 1),
            resolution: (4, 4),
            tracks: vec![video_track(vec![clip.clone()])],
        };

        let compositor = vv_render::Compositor::new_headless();
        let mut inner = NoMediaProvider;
        let mut provider = GpuCompounds::new(&mut inner, &compositor);
        let layers = track_layers_at(&project, &outer, 0, &clip, 0, outer.resolution, &mut provider).unwrap();

        assert!(layers.is_empty(), "il media annidato non è in cache: niente layer");
    }

    fn video_track(clips: Vec<Clip>) -> Track {
        Track {
            kind: TrackKind::Video,
            clips,
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }
    }

    fn compound_media_item(nested: vv_core::TimelineId, size: (u32, u32), duration: FrameIdx) -> vv_core::MediaItem {
        vv_core::MediaItem {
            path: "Compound Clip 1".into(),
            meta: vv_core::MediaMeta {
                duration_frames: duration,
                fps: Rational::new(25, 1),
                width: size.0,
                height: size.1,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 1,
            compound: Some(nested),
        }
    }

    /// Reproduces the bug reported by the user: the right clip of the crossing,
    /// zoomed 2x, must stay off screen for the whole first half of the
    /// window, not pop in and then merely "pan" — see
    /// `push_clearance` in vv-core for the derivation of the 1.5 factor.
    #[test]
    fn crossing_offsets_clear_a_zoomed_clip_fully_off_screen() {
        let left = solid_clip(1, 0, 100, [1.0, 1.0]);
        let right = solid_clip(2, 100, 100, [2.0, 2.0]);
        let track = Track {
            kind: TrackKind::Video,
            clips: vec![left.clone(), right.clone()],
            muted: false,
            solo: false,
            locked: false,
            crossings: vec![CrossTransition {
                left_clip: ClipId(1),
                right_clip: ClipId(2),
                transition: Transition {
                    kind: TransitionKind::Push,
                    duration: 20,
                    direction: PushDirection::Right,
                    ease: Ease::None,
                    curve: 0.0,
                },
            }],
        };
        let timeline = Timeline {
            name: "t".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![track],
        };
        let project = Project::default();
        let mut provider = NoMediaProvider;
        let frame_size = (1920.0, 1080.0);

        // Start of the window: the left one is still entirely in place, the zoomed
        // right one must vanish past `frame_size.0`, not stop at `frame_size.0`.
        let layers = track_layers_at(&project, &timeline, 0, &left, 90, timeline.resolution, &mut provider).unwrap();
        assert_eq!(position_of(&layers[0]), [0.0, 0.0]);
        assert_eq!(position_of(&layers[1]), [-1.5 * frame_size.0, 0.0]);

        // Near the end of the window (109, the last frame before the window
        // [90, 110) closes): the left one (zoom 1x) is almost entirely out
        // with the "full" clearance, the right one (zoom 2x) is almost entirely
        // in place.
        let layers = track_layers_at(&project, &timeline, 0, &left, 109, timeline.resolution, &mut provider).unwrap();
        let progress = 19.0 / 20.0;
        assert_eq!(position_of(&layers[0]), [progress * frame_size.0, 0.0]);
        assert_eq!(position_of(&layers[1]), [-(1.0 - progress) * 1.5 * frame_size.0, 0.0]);
    }
}
