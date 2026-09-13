//! Compositor GPU (wgpu) e overlay testo. Condivide `wgpu::Device`/`Queue`
//! con `eframe` (stesso backend, vedi ARCHITECTURE.md): il device va
//! ottenuto da `eframe::CreationContext::wgpu_render_state` e passato qui,
//! non creato una seconda volta.
//!
//! Scheletro per la milestone 5 (trasformazioni: crop, zoom, gain, poi
//! keyframe). Ancora da implementare.

pub mod compositor;
pub mod text;
