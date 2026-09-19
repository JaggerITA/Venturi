//! Compositor GPU (wgpu) e rasterizzazione dei titoli, vedi
//! ARCHITECTURE.md § Compositing GPU.

pub mod compositor;
pub mod text;

pub use compositor::{Compositor, Layer, OutputFrame, YuvFrame, fit_output_size};
