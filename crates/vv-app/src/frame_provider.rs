//! Come si procura il frame decodificato di una clip: cache riempita in
//! background (anteprima, `render_ahead.rs`) o decode sincrono (export,
//! `export.rs`). La mappatura clip -> frame sorgente è una sola.

use std::sync::Arc;
use vv_core::{Clip, ClipSource, FrameIdx, MediaId, Project, Rgba, TitleParams, Transform};
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
    },
    Solid {
        color: Rgba,
        transform: Transform,
    },
    Text {
        title: TitleParams,
        transform: Transform,
    },
}

impl OwnedLayer {
    pub fn as_render(&self) -> vv_render::Layer<'_> {
        match self {
            OwnedLayer::Video {
                frame,
                transform,
                source_size,
            } => vv_render::Layer::Video {
                frame: as_render_yuv_frame(frame),
                transform: *transform,
                source_size: *source_size,
            },
            OwnedLayer::Solid { color, transform } => vv_render::Layer::Solid {
                color: *color,
                transform: *transform,
            },
            OwnedLayer::Text { title, transform } => vv_render::Layer::Text {
                title,
                transform: *transform,
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
    let transform = clip.effects.transform.value_at(source_frame);
    Ok(match &clip.source {
        ClipSource::SolidColor => Some(OwnedLayer::Solid {
            color: clip
                .effects
                .color
                .as_ref()
                .map_or(Rgba::BLACK, |k| k.value_at(source_frame)),
            transform,
        }),
        ClipSource::Text => clip
            .effects
            .title
            .clone()
            .map(|title| OwnedLayer::Text { title, transform }),
        ClipSource::Media(_) => provider
            .frame_for(project, clip, frame)?
            .map(|frame| OwnedLayer::Video {
                frame,
                transform,
                source_size: clip_source_size(project, clip, timeline_size),
            }),
    })
}
