//! Un'unica interfaccia per le due strategie di acquisizione del frame
//! decodificato di una clip Media a una data posizione di timeline:
//! cache asincrona già riempita in background (anteprima, vedi l'`impl
//! FrameProvider for RenderAhead` in `render_ahead.rs`) o streaming
//! sincrono un frame alla volta (export, vedi `StreamingFrameProvider`
//! in `export.rs`). La mappatura clip→frame-sorgente
//! (`vv_core::Clip::source_frame_at`) è la stessa per entrambe — qui
//! cambia solo *come* il frame a quella posizione viene procurato, non
//! *dove* si trova (REFACTOR_PIPELINE.md B1): il time-remap (milestone 7)
//! andrà cambiato in un posto solo.

use std::sync::Arc;
use vv_core::{Clip, ClipSource, FrameIdx, MediaId, Project};
use vv_media::FrameYuv420;

/// Procura il frame YUV420 decodificato (REFACTOR_PIPELINE.md B3: la
/// conversione a RGB avviene nello shader del compositor)
/// per una clip Media a una data posizione di timeline. `&mut self`
/// perché l'implementazione per l'export tiene stato (il decoder aperto
/// per la clip attiva) — quella per l'anteprima non ne ha bisogno, ma
/// il trait resta uniforme per le due strategie.
pub trait FrameProvider {
    /// `Err` solo per un fallimento reale (media non trovato, errore di
    /// decodifica) — mai per "non disponibile ora", che è `Ok(None)`:
    /// per la cache dell'anteprima significa "non ancora bufferizzato"
    /// (il chiamante mostra l'ultimo frame già disegnato, non uno nero),
    /// per lo streaming dell'export "oltre la fine reale del file" (il
    /// chiamante mostra un frame nero). Collassare le due cose in un
    /// solo `None` avrebbe trasformato un vero errore di decodifica
    /// durante l'export in un silenzioso frame nero — la stessa
    /// disciplina di "mai un frame approssimativo" vale anche per gli
    /// errori, non solo per i frame.
    fn frame_for(
        &mut self,
        project: &Project,
        clip: &Clip,
        timeline_frame: FrameIdx,
    ) -> Result<Option<Arc<FrameYuv420>>, String>;
}

/// `(media_id, frame_sorgente)` per `clip` alla posizione di timeline
/// `timeline_frame`, oppure `None` se `clip` non è una clip Media
/// (`SolidColor`) — helper condiviso dalle implementazioni di
/// `FrameProvider`, che devono gestire il caso `SolidColor` a parte
/// (non è "frame non disponibile", è "nessun frame da decodificare, il
/// colore è calcolato altrove").
pub fn media_source_frame(clip: &Clip, timeline_frame: FrameIdx) -> Option<(MediaId, FrameIdx)> {
    let ClipSource::Media(media_id) = &clip.source else {
        return None;
    };
    Some((*media_id, clip.source_frame_at(timeline_frame)))
}

/// Risoluzione *nativa* del media di `clip` — le unità in cui il crop del
/// `Transform` è espresso, che non sono quelle del frame decodificato
/// quando si sta usando un proxy. Una clip SolidColor è grande quanto la
/// timeline; `(1, 1)` per un media sparito dal pool.
pub fn clip_source_size(project: &Project, clip: &Clip, timeline_size: (u32, u32)) -> (u32, u32) {
    match &clip.source {
        ClipSource::Media(id) => project
            .media_pool
            .get(*id)
            .map(|m| (m.meta.width, m.meta.height))
            .unwrap_or((1, 1)),
        ClipSource::SolidColor => timeline_size,
    }
}

/// `vv_render::YuvFrame` in prestito da un `vv_media::FrameYuv420` — il
/// compositor (vv-render) non dipende da vv-media (stessa convenzione
/// già in uso per il resto della sua API, prende piani di byte grezzi,
/// non un tipo di vv-media), quindi entrambi i chiamanti di
/// `FrameProvider` (anteprima in `main.rs`, export in `export.rs`)
/// passano di qui prima di chiamare `Compositor::render_frame`/
/// `render_frame_to_texture`.
pub fn as_render_yuv_frame(frame: &FrameYuv420) -> vv_render::YuvFrame<'_> {
    vv_render::YuvFrame {
        y: &frame.y,
        width: frame.width,
        height: frame.height,
        u: &frame.u,
        v: &frame.v,
        chroma_width: frame.u_width,
        chroma_height: frame.u_height,
        matrix: match frame.matrix {
            vv_media::ColorMatrix::Bt601 => vv_render::ColorMatrix::Bt601,
            vv_media::ColorMatrix::Bt709 => vv_render::ColorMatrix::Bt709,
            vv_media::ColorMatrix::Bt2020 => vv_render::ColorMatrix::Bt2020,
        },
        full_range: frame.full_range,
    }
}
