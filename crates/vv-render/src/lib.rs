//! Compositor GPU (wgpu), generatori (`generator`) e overlay testo
//! (`text`) — vedi ARCHITECTURE.md § Compositing GPU.
//! Crop + zoom via shader (`compositor`), input in YUV420 planare con
//! conversione a RGB nello shader (REFACTOR_PIPELINE.md B3) —
//! `Compositor::new` può condividere il device wgpu di `eframe`/`egui-wgpu`
//! per un path di rendering zero-copy (B2), oppure usare un device
//! indipendente (`Compositor::new_headless`, per l'export e i test).
//! Generatore SolidColor (`generator`). Overlay testo (`text`) non ancora
//! implementato.

pub mod compositor;
pub mod generator;
pub mod text;

pub use compositor::{ColorMatrix, Compositor, YuvFrame};
pub use generator::solid_color_frame;
