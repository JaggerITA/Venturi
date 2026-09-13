//! Compositor GPU (wgpu), generatori (`generator`) e overlay testo
//! (`text`) — vedi ARCHITECTURE.md § Compositing GPU.
//! Milestone 5: crop + zoom + gain via shader (`compositor`), con un
//! device wgpu indipendente per ora — condividerlo con `eframe`/
//! `egui-wgpu` è un'ottimizzazione futura, non una correttezza (vedi doc
//! del modulo). Milestone 6: generatore SolidColor (`generator`). Overlay
//! testo (`text`) resta uno scheletro.

pub mod compositor;
pub mod generator;
pub mod text;

pub use compositor::Compositor;
pub use generator::solid_color_frame;
