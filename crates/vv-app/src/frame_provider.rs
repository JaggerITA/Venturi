//! Come si procura il frame decodificato di una clip: cache riempita in
//! background (anteprima, `render_ahead.rs`) o decode sincrono (export,
//! `export.rs`). La mappatura clip -> frame sorgente è una sola.

use std::sync::Arc;
use vv_core::{Clip, ClipSource, FrameIdx, MediaId, Project, Rgba, TitleParams, Timeline, Transform};
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
    let media_frame = match &clip.source {
        ClipSource::Media(_) => provider.frame_for(project, clip, frame)?,
        ClipSource::SolidColor | ClipSource::Text => None,
    };
    Ok(build_layer(project, clip, source_frame, transform, opacity, media_frame, timeline_size))
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
    media_frame: Option<Arc<FrameYuv420>>,
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
        ClipSource::Media(_) => media_frame.map(|frame| OwnedLayer::Video {
            frame,
            transform,
            source_size: clip_source_size(project, clip, timeline_size),
            opacity,
            filters,
        }),
    }
}

/// Margine di ritentativo di `extrapolated_frame_for` verso l'inizio del
/// file quando l'ultimo frame "congelato" non arriva mai: `duration_frames`
/// (vedi `probe::probe`) è `durata_secondi * fps` arrotondato, non il
/// conteggio di frame osservato dal decoder, quindi può sovrastimarlo di un
/// frame — quel frame non esiste per il decoder (EOF), e senza ritentativo
/// il freeze non si risolve mai (il worker di `render_ahead` ci sbatte
/// contro all'infinito, mai in cache). Pochi frame bastano per il comune
/// errore di arrotondamento senza mascherare un media davvero rotto.
const EXTRAPOLATION_EOF_RETRY_FRAMES: FrameIdx = 5;

/// Come `provider.frame_for`, ma oltre i bordi reali del media si blocca
/// (freeze) sul frame sorgente più vicino disponibile invece di restituire
/// `None`: usato solo dalle crossing transition, dove "oltre la fine" è la
/// norma (è la clip che presta il suo bordo alla transizione), non un
/// errore da segnalare come farebbe `clip_layer` in export.
fn extrapolated_frame_for(
    project: &Project,
    clip: &Clip,
    timeline_frame: FrameIdx,
    provider: &mut dyn FrameProvider,
) -> Result<Option<Arc<FrameYuv420>>, String> {
    let ClipSource::Media(media_id) = &clip.source else {
        return provider.frame_for(project, clip, timeline_frame);
    };
    let Some(media) = project.media_pool.get(*media_id) else {
        return provider.frame_for(project, clip, timeline_frame);
    };
    let wanted = clip.source_frame_at(timeline_frame);
    let clamped = wanted.clamp(0, (media.meta.duration_frames - 1).max(0));
    let earliest_retry = clamped.saturating_sub(EXTRAPOLATION_EOF_RETRY_FRAMES).max(0);
    let mut probe = clamped;
    loop {
        let held_timeline_frame = clip.timeline_frame_at(probe);
        match provider.frame_for(project, clip, held_timeline_frame)? {
            some @ Some(_) => return Ok(some),
            None if probe > earliest_retry => probe -= 1,
            None => return Ok(None),
        }
    }
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
    let media_frame = match &clip.source {
        ClipSource::Media(_) => extrapolated_frame_for(project, clip, frame, provider)?,
        ClipSource::SolidColor | ClipSource::Text => None,
    };
    Ok(build_layer(project, clip, source_frame, transform, opacity, media_frame, timeline_size))
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
