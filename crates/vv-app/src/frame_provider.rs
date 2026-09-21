//! Come si procura il frame decodificato di una clip: cache riempita in
//! background (anteprima, `render_ahead.rs`) o decode sincrono (export,
//! `export.rs`). La mappatura clip -> frame sorgente è una sola.

use std::sync::Arc;
use vv_core::{Clip, ClipSource, ColorMatrix, FrameIdx, MediaId, Project, Rgba, TitleParams, Timeline, Transform};
use vv_media::FrameYuv420;

/// `&mut self`: l'export tiene aperti i decoder.
pub trait FrameProvider {
    /// `Ok(None)` = non disponibile ora (anteprima: non ancora in cache;
    /// export: oltre la fine del file). Un errore di decode resta `Err`, così
    /// l'export non lo trasforma in un frame nero silenzioso.
    fn frame_for(
        &mut self,
        project: &Project,
        clip: &Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<FrameYuv420>>, String>;

    /// Il frame composto di `nested` (la timeline annidata di una compound
    /// clip) a `local_frame`, come texture GPU. `None` dall'implementazione
    /// di default: chi non sa comporre su GPU ricade su `frame_for`, cioè
    /// sul frame composto e riportato in CPU. Chi la implementa compone
    /// ricorsivamente con `track_layers_at`, passando sé stesso.
    fn compound_texture(
        &mut self,
        _project: &Project,
        _nested: &Timeline,
        _local_frame: FrameIdx,
    ) -> Result<Option<vv_render::wgpu::Texture>, String> {
        Ok(None)
    }
}

/// La timeline annidata di `clip`, se è una compound clip.
fn nested_timeline_of<'a>(project: &'a Project, clip: &Clip) -> Option<&'a Timeline> {
    let ClipSource::Media(media_id) = &clip.source else {
        return None;
    };
    let nested_id = project.media_pool.get(*media_id)?.compound?;
    project.timelines.get(nested_id)
}

/// `(media, frame sorgente)` di `clip` a `timeline_frame`; `None` se non è
/// una clip Media.
pub fn media_source_frame(clip: &Clip, timeline_frame: FrameIdx) -> Option<(MediaId, FrameIdx)> {
    let ClipSource::Media(media_id) = &clip.source else {
        return None;
    };
    Some((*media_id, clip.source_frame_at(timeline_frame)))
}

/// Risoluzione *nativa* del media, l'unità del crop anche quando si decoda
/// il proxy. Generatori: quella della timeline; `(1, 1)` se il media manca.
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

/// Converte un buffer RGBA8 denso (`width`x`height`, come da
/// `Compositor::render_layers_rgba_transparent`) in un `FrameYuv420`
/// BT.709 limited con l'alpha portato per intero, non sottocampionato, in
/// `FrameYuv420::alpha` — la conversione inversa di `yuv_to_rgb` in
/// `transform.wgsl`. Usata solo per il frame composto della timeline
/// annidata di una compound clip: un giro RGB->YUV->RGB in più rispetto a
/// un video reale, la stessa perdita di risoluzione croma che il 4:2:0 ha
/// già ovunque (non tocca l'accuratezza del *frame*, solo la sua croma).
pub fn rgba_to_yuv420_with_alpha(rgba: &[u8], width: u32, height: u32) -> FrameYuv420 {
    const KR: f32 = 0.2126;
    const KB: f32 = 0.0722;
    const KG: f32 = 1.0 - KR - KB;
    let (w, h) = (width as usize, height as usize);
    let luma = |r: f32, g: f32, b: f32| KR * r + KG * g + KB * b;
    let sample = |x: usize, y: usize| -> (f32, f32, f32) {
        let i = (y * w + x) * 4;
        (rgba[i] as f32 / 255.0, rgba[i + 1] as f32 / 255.0, rgba[i + 2] as f32 / 255.0)
    };

    let mut y_plane = vec![0u8; w * h];
    let mut alpha = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let (r, g, b) = sample(x, y);
            y_plane[i] = (16.0 + luma(r, g, b) * 219.0).round().clamp(0.0, 255.0) as u8;
            alpha[i] = rgba[i * 4 + 3];
        }
    }

    // Croma 4:2:0: media del blocco 2x2 corrispondente, come farebbe un
    // encoder reale invece di prendere un solo campione ad angolo.
    let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut u_sum, mut v_sum, mut n) = (0.0f32, 0.0f32, 0.0f32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let (x, y) = (cx * 2 + dx, cy * 2 + dy);
                    if x >= w || y >= h {
                        continue;
                    }
                    let (r, g, b) = sample(x, y);
                    let yn = luma(r, g, b);
                    u_sum += (b - yn) / (2.0 * (1.0 - KB));
                    v_sum += (r - yn) / (2.0 * (1.0 - KR));
                    n += 1.0;
                }
            }
            let n = n.max(1.0);
            u_plane[cy * cw + cx] = (128.0 + (u_sum / n) * 224.0).round().clamp(0.0, 255.0) as u8;
            v_plane[cy * cw + cx] = (128.0 + (v_sum / n) * 224.0).round().clamp(0.0, 255.0) as u8;
        }
    }

    FrameYuv420 {
        width,
        height,
        y: y_plane,
        u: u_plane,
        v: v_plane,
        u_width: cw as u32,
        u_height: ch as u32,
        matrix: ColorMatrix::Bt709,
        full_range: false,
        alpha: Some(alpha),
    }
}

/// vv-render non dipende da vv-media: prende piani di byte grezzi.
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

/// Un layer di compositing che possiede ciò che `vv_render::Layer` presta.
pub enum OwnedLayer {
    Video {
        frame: Arc<FrameYuv420>,
        transform: Transform,
        /// Risoluzione nativa del media (non del proxy): le unità del crop.
        source_size: (u32, u32),
        opacity: f32,
        /// Solo i filtri attivi di `EffectStack::filters`, nel loro ordine.
        filters: Vec<vv_core::FilterKind>,
    },
    /// Compound clip composta su GPU (vedi `FrameProvider::compound_texture`).
    Texture {
        texture: vv_render::wgpu::Texture,
        transform: Transform,
        /// Risoluzione della timeline annidata: le unità del crop, come
        /// `source_size` di `Video` (la texture può essere più piccola).
        source_size: (u32, u32),
        opacity: f32,
        filters: Vec<vv_core::FilterKind>,
    },
    Solid {
        color: Rgba,
        transform: Transform,
        opacity: f32,
        filters: Vec<vv_core::FilterKind>,
    },
    Text {
        title: TitleParams,
        transform: Transform,
        opacity: f32,
        filters: Vec<vv_core::FilterKind>,
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
            } => vv_render::Layer::Video {
                frame: as_render_yuv_frame(frame),
                transform: *transform,
                source_size: *source_size,
                opacity: *opacity,
                filters: filters.as_slice(),
            },
            OwnedLayer::Texture {
                texture,
                transform,
                source_size,
                opacity,
                filters,
            } => vv_render::Layer::Texture {
                texture,
                transform: *transform,
                source_size: *source_size,
                opacity: *opacity,
                filters: filters.as_slice(),
            },
            OwnedLayer::Solid { color, transform, opacity, filters } => vv_render::Layer::Solid {
                color: *color,
                transform: *transform,
                opacity: *opacity,
                filters: filters.as_slice(),
            },
            OwnedLayer::Text { title, transform, opacity, filters } => vv_render::Layer::Text {
                title,
                transform: *transform,
                opacity: *opacity,
                filters: filters.as_slice(),
            },
        }
    }
}

/// Limite alla profondità di nesting delle compound clip: una compound
/// clip che contenesse sé stessa (`Project::would_create_compound_cycle` lo
/// impedisce all'inserimento) manderebbe la composizione in ricorsione
/// infinita. Rete di sicurezza, non un limite di progetto.
pub const MAX_COMPOUND_DEPTH: u32 = 16;

/// Aggiunge a un provider qualunque la composizione su GPU delle compound
/// clip: la timeline annidata diventa una texture che resta sulla scheda,
/// invece di un frame composto, riportato in CPU e riconvertito a YUV.
/// `inner` procura i media veri, che il resto della pipeline decodifica
/// come sempre (anche quelli dentro le timeline annidate).
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
    ) -> Result<Option<vv_render::wgpu::Texture>, String> {
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
        // Sfondo trasparente: dove `nested` non ha nulla da mostrare deve
        // restare visibile quel che sta sotto nella timeline esterna. E la
        // texture non torna nel pool, perché la teniamo noi fino al pass.
        Ok(Some(self.compositor.render_layers_to_owned_texture_transparent(
            &render_layers,
            vv_render::OutputFrame::exact(width, height),
        )))
    }
}

/// I layer di `nested` a `local_frame`, o `None` se una clip che copre quel
/// frame non ha ancora il suo contenuto (un media in decoding): un frame
/// composto a metà sarebbe peggio di nessun frame. Nessuna clip attiva è un
/// caso valido, non un "non pronto": dà un frame trasparente.
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

/// Il layer di `clip` al frame di timeline `frame`, condiviso da anteprima
/// ed export. `Ok(None)`: frame del media non disponibile.
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

/// Cosa mostra una clip a `timeline_frame`: niente se non è una clip Media,
/// la texture composta se è una compound clip e il provider sa comporla su
/// GPU, altrimenti il frame decodificato (o composto e riportato in CPU).
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

/// Il contenuto già procurato di una clip, vedi `clip_content`.
enum ClipContent {
    None,
    Yuv(Arc<FrameYuv420>),
    Texture(vv_render::wgpu::Texture),
}

/// La parte comune a `clip_layer` e al lato di una crossing transition
/// (`crossing_side_layer`): dato il frame sorgente, il transform già
/// calcolato e il frame media già decodificato (se `ClipSource::Media`),
/// assembla l'`OwnedLayer` giusto per il tipo di sorgente della clip.
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
        }),
        ClipSource::Text => clip
            .effects
            .title
            .clone()
            .map(|title| OwnedLayer::Text { title, transform, opacity, filters }),
        ClipSource::Media(_) => match content {
            ClipContent::None => None,
            ClipContent::Yuv(frame) => Some(OwnedLayer::Video {
                frame,
                transform,
                source_size: clip_source_size(project, clip, timeline_size),
                opacity,
                filters,
            }),
            ClipContent::Texture(texture) => Some(OwnedLayer::Texture {
                texture,
                transform,
                source_size: clip_source_size(project, clip, timeline_size),
                opacity,
                filters,
            }),
        },
    }
}

/// Il frame di timeline da cui prendere il contenuto: `timeline_frame`, o
/// quello del frame sorgente più vicino se cade oltre i bordi reali del
/// media (freeze invece di niente). Usato solo dalle crossing transition,
/// dove "oltre la fine" è la norma — è la clip che presta il suo bordo alla
/// transizione — non un errore da segnalare come fa `clip_layer` in export.
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

/// Il lato di una crossing transition per una singola clip: come
/// `clip_layer`, ma il frame sorgente si blocca ai bordi del media invece
/// di sparire, e l'offset di posizione è quello di `CrossTransition`
/// invece di `Clip::transition_offset_at` (che riguarda solo i bordi
/// singoli, `transition_in`/`transition_out`).
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

/// I layer di una crossing transition attiva a `frame`: coda di `left`,
/// poi testa di `right` (in quest'ordine, `right` sopra — per un push non
/// importa, le due zone visibili non si sovrappongono mai davvero).
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
    // Lo zoom di ciascuna clip al proprio frame sorgente: serve a
    // `offsets` per liberare davvero lo schermo anche se una delle due (o
    // entrambe) è zoomata — vedi `push_clearance`. Ricalcolato qui e di
    // nuovo dentro `crossing_side_layer`: costa pochissimo (`Keyframed`),
    // non vale la pena infilarlo come parametro in più posti.
    let left_zoom = left.effects.transform.value_at(left.source_frame_at(frame)).zoom;
    let right_zoom = right.effects.transform.value_at(right.source_frame_at(frame)).zoom;
    let (left_offset, right_offset) = crossing.offsets(progress, frame_size, left_zoom, right_zoom);
    let mut layers = Vec::with_capacity(2);
    layers.extend(crossing_side_layer(project, left, frame, timeline_size, provider, left_offset)?);
    layers.extend(crossing_side_layer(project, right, frame, timeline_size, provider, right_offset)?);
    Ok(layers)
}

/// I layer di `clip` (sulla track `track_index`) al frame di timeline
/// `frame`: uno solo nel caso normale, ma due se `frame` cade nella
/// finestra di una crossing transition valida che coinvolge questa clip —
/// l'altra metà della coppia si aggiunge da sé, spinta secondo la
/// transizione condivisa. Punto d'ingresso da preferire a `clip_layer`
/// ovunque si componga una track intera (anteprima ed export), non solo
/// una clip isolata.
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

    /// Bug segnalato dall'utente: una compound clip è una timeline come le
    /// altre, quindi le zone dove la sua timeline annidata non ha nulla da
    /// mostrare devono restare trasparenti — non nere, o coprirebbero quel
    /// che c'è sotto nella timeline che la contiene.
    #[test]
    fn an_empty_area_of_a_compound_clip_shows_the_layer_below_it() {
        let mut project = Project::default();
        // Timeline annidata: un rosso che copre solo la metà sinistra.
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

        // Timeline esterna: un blu sotto, la compound clip sopra.
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

    /// Finché un media dentro la timeline annidata non è pronto, la
    /// compound clip non produce un layer: meglio nessun frame che un
    /// frame composto a metà.
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

    /// Riproduce il bug segnalato dall'utente: la clip destra della crossing
    /// zoomata 2x deve restare fuori schermo per l'intera prima metà della
    /// finestra, non comparire di scatto per poi solo "pannare" — vedi
    /// `push_clearance` in vv-core per la derivazione del fattore 1.5.
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

        // Inizio finestra: la sinistra è ancora del tutto a posto, la destra
        // zoomata deve sparire oltre `frame_size.0`, non fermarsi a `frame_size.0`.
        let layers = track_layers_at(&project, &timeline, 0, &left, 90, timeline.resolution, &mut provider).unwrap();
        assert_eq!(position_of(&layers[0]), [0.0, 0.0]);
        assert_eq!(position_of(&layers[1]), [-1.5 * frame_size.0, 0.0]);

        // Quasi a fine finestra (109, l'ultimo frame prima che la finestra
        // [90, 110) si chiuda): la sinistra (zoom 1x) è quasi tutta fuori
        // con la clearance "intera", la destra (zoom 2x) è quasi del tutto
        // a posto.
        let layers = track_layers_at(&project, &timeline, 0, &left, 109, timeline.resolution, &mut provider).unwrap();
        let progress = 19.0 / 20.0;
        assert_eq!(position_of(&layers[0]), [progress * frame_size.0, 0.0]);
        assert_eq!(position_of(&layers[1]), [-(1.0 - progress) * 1.5 * frame_size.0, 0.0]);
    }
}
