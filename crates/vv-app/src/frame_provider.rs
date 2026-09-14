//! Un'unica interfaccia per le due strategie di acquisizione del frame
//! decodificato di una clip Media a una data posizione di timeline:
//! cache asincrona già riempita in background (anteprima, vedi l'`impl
//! FrameProvider for RenderAhead` in `render_ahead.rs`) o streaming
//! sincrono un frame alla volta (export, vedi `StreamingFrameProvider`
//! in `export.rs`). La mappatura clip→frame-sorgente
//! (`vv_core::Clip::source_frame_at`) è la stessa per entrambe — qui
//! cambia solo *come* il frame a quella posizione viene procurato, non
//! *dove* si trova (REFACTOR_PIPELINE.md B1). Prima delle due strategie
//! rifacevano ciascuna la propria mappatura, identiche solo perché
//! coincidono quando `EffectStack::speed == 1`: al primo time-remap
//! reale (milestone 7) sarebbero divergenti senza questo posto unico.

use std::sync::Arc;
use vv_core::{Clip, ClipSource, FrameIdx, MediaId, Project};
use vv_media::FrameRgba;

/// Procura il frame RGBA per una clip Media a una data posizione di
/// timeline. `&mut self` perché l'implementazione per l'export tiene
/// stato (il decoder aperto per la clip attiva) — quella per l'anteprima
/// non ne ha bisogno, ma il trait resta uniforme per le due strategie.
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
    ) -> Result<Option<Arc<FrameRgba>>, String>;
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
