//! Compositor GPU (wgpu) e rasterizzazione dei titoli, vedi
//! ARCHITECTURE.md § Compositing GPU.

pub mod compositor;
pub mod text;

pub use compositor::{Compositor, Layer, OutputFrame, PooledTexture, YuvFrame, fit_output_size};
/// Riesportato: chi possiede una texture per `Layer::Texture` deve usare
/// la stessa versione di wgpu del compositor.
pub use wgpu;
