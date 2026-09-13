//! Compositor GPU (wgpu) e overlay testo (vedi ARCHITECTURE.md § Compositing
//! GPU). Milestone 5: crop + zoom via shader (`compositor`), con un device
//! wgpu indipendente per ora — condividerlo con `eframe`/`egui-wgpu` è
//! un'ottimizzazione futura, non una correttezza (vedi doc del modulo).
//! Overlay testo (`text`) resta uno scheletro per la milestone 6.

pub mod compositor;
pub mod text;

pub use compositor::Compositor;
