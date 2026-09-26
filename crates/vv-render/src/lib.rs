//! GPU compositor (wgpu) and title rasterization, see
//! ARCHITECTURE.md § GPU compositing.

pub mod compositor;
pub mod text;

pub use compositor::{
    Compositor, Layer, LayerContent, OutputFrame, PooledTexture, YuvFrame, fit_output_size,
};
/// Re-exported: whoever owns a texture for `LayerContent::Texture` must use the
/// same wgpu version as the compositor.
pub use wgpu;
