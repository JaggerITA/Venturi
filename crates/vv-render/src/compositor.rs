//! GPU compositing: planar YUV420 input converted to RGB in the shader,
//! layers composed in alpha-over from bottom to top.
//!
//! - `render_layers` / `render_layers_i420`: readback in RGBA or I420
//!   (export).
//! - `render_layers_to_texture`: stays on the GPU, for the preview that
//!   registers the texture in egui-wgpu. Requires `Compositor::new` on the
//!   same device as egui.

use std::sync::{Arc, Mutex};
use vv_core::{BlendMode, ColorMatrix, Transform};
use wgpu::util::DeviceExt;

const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const BLACK: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};
const TRANSPARENT: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
};
const SOLID_PLACEHOLDER: YuvFrame<'static> = YuvFrame {
    y: &[0],
    width: 1,
    height: 1,
    u: &[128],
    v: &[128],
    chroma_width: 1,
    chroma_height: 1,
    matrix: ColorMatrix::Bt601,
    full_range: true,
    alpha: OPAQUE,
};
/// Placeholder for `YuvFrame::alpha` when the layer carries no real
/// per-pixel coverage: a single byte, sampled everywhere (`ClampToEdge`) —
/// zero cost for the common case (video/solid/text, always opaque).
const OPAQUE: &[u8] = &[255];
/// Format of the three input planes (Y/U/V): a single 8-bit channel, read
/// as `.r` in the shader.
const PLANE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Matrix selector for the shader: must stay aligned with
/// `kr_kb` in `transform.wgsl`.
fn shader_matrix_id(matrix: ColorMatrix) -> f32 {
    match matrix {
        ColorMatrix::Bt601 => 0.0,
        ColorMatrix::Bt709 => 1.0,
        ColorMatrix::Bt2020 => 2.0,
    }
}

/// 8-bit YUV420 frame with the color metadata. Dense planes, no padding.
pub struct YuvFrame<'a> {
    pub y: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub u: &'a [u8],
    pub v: &'a [u8],
    /// Dimensions of the U/V planes (4:2:0 subsampled, typically
    /// `(width+1)/2` x `(height+1)/2` but not recomputed here: the
    /// caller passes the real dimensions allocated by the decoder).
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub matrix: ColorMatrix,
    /// `true` = JPEG/full range (0-255), `false` = MPEG/limited range.
    pub full_range: bool,
    /// Per-pixel coverage, not subsampled: a single byte (`&[255]`,
    /// see `SOLID_PLACEHOLDER`) for "opaque everywhere" (a decoded file
    /// has no alpha channel), otherwise `width`x`height` bytes like Y — see
    /// `FrameYuv420::alpha`, where it comes from when it is not the placeholder.
    pub alpha: &'a [u8],
}

/// A layer of the stack. `Solid` and `Text` are treated as sources as large
/// as the timeline: same transform/crop.
pub enum Layer<'a> {
    Video {
        frame: YuvFrame<'a>,
        transform: Transform,
        /// *Native* resolution of the media, in which the `Transform`'s crop
        /// in pixels is expressed: not that of `frame`, which can be a
        /// reduced-resolution proxy.
        source_size: (u32, u32),
        /// Alpha multiplier of the whole layer (clip fades):
        /// 1.0 = no attenuation.
        opacity: f32,
        /// Active filters of the clip (`EffectStack::filters`), in the order
        /// they must be applied: vv-render does not know what each one means,
        /// only the id of the shader corresponding to it (`filter_shader_id`).
        filters: &'a [vv_core::FilterKind],
        /// How the layer composes onto those below.
        blend: BlendMode,
    },
    /// A frame already composed and resident on the GPU: the nested timeline of
    /// a compound clip, which becomes a layer again in the outer timeline without
    /// going through the CPU. Premultiplied RGBA — see `Fill::Rgba`.
    Texture {
        texture: &'a wgpu::Texture,
        transform: Transform,
        /// As in `Video`: the units of the crop, which may not be the
        /// dimensions of `texture` (reduced-resolution preview).
        source_size: (u32, u32),
        opacity: f32,
        filters: &'a [vv_core::FilterKind],
        blend: BlendMode,
    },
    Solid {
        color: vv_core::Rgba,
        transform: Transform,
        opacity: f32,
        filters: &'a [vv_core::FilterKind],
        blend: BlendMode,
    },
    /// Title: rasterized at the output resolution (see `text`), then
    /// treated like a `Solid` as large as the timeline.
    Text {
        title: &'a vv_core::TitleParams,
        transform: Transform,
        opacity: f32,
        filters: &'a [vv_core::FilterKind],
        blend: BlendMode,
    },
}

/// How many filters per layer the uniform can carry (see `filters` in
/// `TransformUniform`): past that, the excess filters are ignored. Generous
/// for real use, avoids a dynamically sized buffer for the shader.
const MAX_LAYER_FILTERS: usize = 8;

/// Shader id of each `FilterKind`; 0 is reserved for "empty slot".
fn filter_shader_id(kind: vv_core::FilterKind) -> f32 {
    match kind {
        vv_core::FilterKind::Grayscale => 1.0,
    }
}

/// Shader id of each `BlendMode`, aligned with the `switch` of
/// `blend_channel` in `transform.wgsl`; 0 = Normal, the only one using
/// the pipeline's alpha blending instead of reading the backdrop.
fn blend_shader_id(mode: BlendMode) -> f32 {
    match mode {
        BlendMode::Normal => 0.0,
        BlendMode::Add => 1.0,
        BlendMode::Multiply => 2.0,
        BlendMode::Screen => 3.0,
        BlendMode::Overlay => 4.0,
        BlendMode::Darken => 5.0,
        BlendMode::Lighten => 6.0,
        BlendMode::ColorDodge => 7.0,
        BlendMode::ColorBurn => 8.0,
        BlendMode::HardLight => 9.0,
        BlendMode::SoftLight => 10.0,
        BlendMode::Difference => 11.0,
        BlendMode::Exclusion => 12.0,
        BlendMode::Subtract => 13.0,
        BlendMode::Divide => 14.0,
    }
}

/// What colors a layer: the Y/U/V planes, a solid color, or a solid
/// color with the coverage taken from the Y plane.
#[derive(Clone, Copy)]
enum Fill {
    Video,
    Solid(vv_core::Rgba),
    Mask(vv_core::Rgba),
    /// Already composed RGBA texture, with the color premultiplied by the alpha
    /// (it is the result of an `ALPHA_BLENDING` onto a transparent clear): the
    /// shader divides it out before putting it back into alpha-over.
    Rgba,
}

/// Pixel resolution of the produced texture and the logical one of the
/// timeline, in which position and anchor are expressed. They coincide
/// on export; the preview composes at the resolution of the decoded frame.
#[derive(Debug, Clone, Copy)]
pub struct OutputFrame {
    pub width: u32,
    pub height: u32,
    pub timeline_size: (u32, u32),
}

impl OutputFrame {
    /// Output at the timeline resolution.
    pub fn exact(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            timeline_size: (width, height),
        }
    }

    /// Output at a resolution different from the timeline's, with the
    /// same aspect ratio.
    pub fn scaled(width: u32, height: u32, timeline_size: (u32, u32)) -> Self {
        Self {
            width,
            height,
            timeline_size,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct TransformUniform {
    crop: [f32; 4],
    zoom_pos: [f32; 4],
    fit_rot: [f32; 4],
    anchor_flip: [f32; 4],
    color: [f32; 4],
    solid: [f32; 4],
    /// x: opacity of the whole layer. y: id of the compositing method
    /// (`blend_shader_id`). z/w unused.
    extra: [f32; 4],
    /// Shader ids of the active filters, in order of application (see
    /// `filter_shader_id`); 0 = empty slot. `MAX_LAYER_FILTERS` in two vec4s
    /// for the uniform alignment.
    filters: [[f32; 4]; MAX_LAYER_FILTERS / 4],
}

impl TransformUniform {
    fn new(
        t: &Transform,
        matrix: ColorMatrix,
        full_range: bool,
        fit: [f32; 2],
        output: OutputFrame,
        source_size: (u32, u32),
        fill: Fill,
        opacity: f32,
        filters: &[vv_core::FilterKind],
        blend: BlendMode,
    ) -> Self {
        let (mode, solid) = match fill {
            Fill::Video => (0.0, None),
            Fill::Solid(c) => (1.0, Some(c)),
            Fill::Mask(c) => (2.0, Some(c)),
            Fill::Rgba => (3.0, None),
        };
        // The `Transform` is in pixels — of the timeline for position and anchor,
        // of the media for the crop; the shader works in normalized
        // coordinates.
        let (frame_w, frame_h) = (
            output.timeline_size.0.max(1) as f32,
            output.timeline_size.1.max(1) as f32,
        );
        let (source_w, source_h) = (source_size.0.max(1) as f32, source_size.1.max(1) as f32);
        Self {
            // From the per-side cuts to the rectangle the shader samples.
            crop: [
                t.crop[0] / source_w,
                t.crop[1] / source_h,
                1.0 - t.crop[2] / source_w,
                1.0 - t.crop[3] / source_h,
            ],
            // The model's Y axis points up (as in an NLE), the uv one
            // points down.
            zoom_pos: [
                t.zoom[0],
                t.zoom[1],
                t.position[0] / frame_w,
                -t.position[1] / frame_h,
            ],
            fit_rot: [
                fit[0],
                fit[1],
                t.rotation.to_radians(),
                // The softness follows the crop: media pixels, and on the
                // shorter axis, so it stays isotropic.
                t.crop_softness / source_w.min(source_h),
            ],
            anchor_flip: [
                t.anchor[0] / frame_w,
                -t.anchor[1] / frame_h,
                if t.flip[0] { 1.0 } else { 0.0 },
                if t.flip[1] { 1.0 } else { 0.0 },
            ],
            color: [
                shader_matrix_id(matrix),
                if full_range { 1.0 } else { 0.0 },
                output.width as f32 / output.height.max(1) as f32,
                mode,
            ],
            solid: solid.map_or([0.0; 4], |c| {
                [
                    c.r.clamp(0.0, 1.0),
                    c.g.clamp(0.0, 1.0),
                    c.b.clamp(0.0, 1.0),
                    c.a.clamp(0.0, 1.0),
                ]
            }),
            extra: [opacity.clamp(0.0, 1.0), blend_shader_id(blend), 0.0, 0.0],
            filters: {
                let mut ids = [0.0f32; MAX_LAYER_FILTERS];
                for (slot, kind) in ids.iter_mut().zip(filters.iter().take(MAX_LAYER_FILTERS)) {
                    *slot = filter_shader_id(*kind);
                }
                [[ids[0], ids[1], ids[2], ids[3]], [ids[4], ids[5], ids[6], ids[7]]]
            },
        }
    }
}

pub struct Compositor {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::RenderPipeline,
    /// Like `pipeline`, but in REPLACE: used by the compositing methods
    /// other than Normal (see `blend_shader_id`).
    blend_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    i420_pipeline: wgpu::ComputePipeline,
    /// Textures reused from one frame to the next, by size: allocating new
    /// ones on every frame costs more than the drawing itself.
    pool: Mutex<TexturePool>,
    /// A separate pool for the intermediates (see `PooledTexture`): they go back
    /// there when whoever uses them lets them go, not at the end of the render.
    scratch: Arc<Mutex<Vec<wgpu::Texture>>>,
}

#[derive(Default)]
struct TexturePool {
    planes: Vec<wgpu::Texture>,
    outputs: Vec<wgpu::Texture>,
    i420: Option<I420Buffers>,
}

/// Buffer of the I420 conversion, for the size of the last frame.
struct I420Buffers {
    size: wgpu::BufferAddress,
    storage: wgpu::Buffer,
    params: wgpu::Buffer,
    readback: wgpu::Buffer,
}

/// Whether the output texture goes straight back into the frame pool or comes out as
/// a `PooledTexture`, which puts it back when whoever uses it lets it go.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recycle {
    Immediately,
    OnDrop,
}

/// An intermediate texture (the composed frame of a nested timeline) that
/// returns to the pool by itself: while someone holds it as a layer no other
/// render draws over it, and when they let it go it is available
/// again, without reallocating 8 MB on every frame.
pub struct PooledTexture {
    texture: Option<wgpu::Texture>,
    pool: Arc<Mutex<Vec<wgpu::Texture>>>,
}

impl std::ops::Deref for PooledTexture {
    type Target = wgpu::Texture;

    fn deref(&self) -> &wgpu::Texture {
        self.texture.as_ref().expect("la texture c'è fino al Drop")
    }
}

impl Drop for PooledTexture {
    fn drop(&mut self) {
        if let Some(texture) = self.texture.take() {
            give_back(&mut self.pool.lock().unwrap(), [texture]);
        }
    }
}

/// Past that, the textures of no longer used sizes are let go.
const MAX_POOLED: usize = 32;

fn take_sized(pool: &mut Vec<wgpu::Texture>, width: u32, height: u32) -> Option<wgpu::Texture> {
    let i = pool
        .iter()
        .position(|t| t.width() == width && t.height() == height)?;
    Some(pool.swap_remove(i))
}

fn give_back(pool: &mut Vec<wgpu::Texture>, textures: impl IntoIterator<Item = wgpu::Texture>) {
    pool.extend(textures);
    let excess = pool.len().saturating_sub(MAX_POOLED);
    pool.drain(..excess);
}

impl Compositor {
    pub fn new(device: Arc<wgpu::Device>, queue: Arc<wgpu::Queue>) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vv-render transform shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/transform.wgsl").into()),
        });

        // Three input textures (Y/U/V, bindings 0-2) instead of a single
        // RGBA one: the YUV→RGB conversion happens in the shader
        // (plans/REFACTOR_PIPELINE.md B3), only the raw planes arrive here.
        let plane_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vv-render transform bind group layout"),
            entries: &[
                plane_entry(0), // Y
                plane_entry(1), // U
                plane_entry(2), // V
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                plane_entry(5), // Alpha (per-pixel coverage, see YuvFrame::alpha)
                plane_entry(6), // Backdrop (see `backdrop_tex` in the shader)
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vv-render transform pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let transform_pipeline = |label, blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: OUTPUT_FORMAT,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        // Normal: not REPLACE, because the uncovered areas (letterbox) come out with
        // alpha 0 and must show the layer below. The other compositing
        // methods read the layer below themselves (`backdrop_tex`) and
        // write the already composed result.
        let pipeline = transform_pipeline(
            "vv-render transform pipeline",
            Some(wgpu::BlendState::ALPHA_BLENDING),
        );
        let blend_pipeline = transform_pipeline("vv-render blend pipeline", Some(wgpu::BlendState::REPLACE));

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vv-render transform sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let i420_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vv-render rgba->i420 shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/rgba_to_i420.wgsl").into()),
        });
        let i420_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("vv-render rgba->i420 pipeline"),
            layout: None,
            module: &i420_shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        Self {
            device,
            queue,
            pipeline,
            blend_pipeline,
            bind_group_layout,
            sampler,
            i420_pipeline,
            pool: Mutex::default(),
            scratch: Arc::default(),
        }
    }

    /// Creates an independent wgpu device (headless, no surface) to
    /// use the compositor outside an eframe/egui-wgpu context — useful
    /// for the app today and for the tests.
    pub fn new_headless() -> Self {
        let (device, queue) = pollster::block_on(async {
            let instance = wgpu::Instance::default();
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions::default())
                .await
                .expect("nessun adapter wgpu disponibile");
            adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("vv-render headless device"),
                    ..Default::default()
                })
                .await
                .expect("richiesta device wgpu fallita")
        });
        Self::new(Arc::new(device), Arc::new(queue))
    }

    /// Like `render_layers`, but dense I420 BT.709 limited: doing the conversion on
    /// the GPU saves doing it on the CPU and halves the readback.
    pub fn render_layers_i420(&self, layers: &[Layer], output: OutputFrame) -> Vec<u8> {
        const WORKGROUP: u32 = 256;
        const MAX_GROUPS_PER_DIM: u32 = 65535;

        let output_texture = self.render_layers_to_texture(layers, output);
        let (w, h) = (output.width, output.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let len = (w * h + 2 * cw * ch) as usize;
        let total_words = len.div_ceil(4) as u32;
        let groups = total_words.div_ceil(WORKGROUP);
        let groups_x = groups.min(MAX_GROUPS_PER_DIM);
        let groups_y = groups.div_ceil(groups_x);
        let params: [u32; 8] = [w, h, cw, ch, groups_x * WORKGROUP, total_words, 0, 0];

        let buffer_size = total_words as wgpu::BufferAddress * 4;
        let mut pool = self.pool.lock().unwrap();
        if pool.i420.as_ref().is_none_or(|b| b.size != buffer_size) {
            pool.i420 = Some(I420Buffers {
                size: buffer_size,
                storage: self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vv-render i420 storage"),
                    size: buffer_size,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                params: self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vv-render i420 params"),
                    size: std::mem::size_of_val(&params) as wgpu::BufferAddress,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                readback: self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vv-render i420 readback"),
                    size: buffer_size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
            });
        }
        let I420Buffers { storage, params: params_buffer, readback, .. } = pool.i420.as_ref().unwrap();
        self.queue.write_buffer(params_buffer, 0, bytemuck::cast_slice(&params));
        let texture_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vv-render i420 bind group"),
            layout: &self.i420_pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: storage.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buffer.as_entire_binding(),
                },
            ],
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vv-render i420 encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vv-render i420 pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.i420_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups(groups_x, groups_y, 1);
        }
        encoder.copy_buffer_to_buffer(storage, 0, readback, 0, buffer_size);
        self.queue.submit(Some(encoder.finish()));

        self.map_read(readback, |data| data[..len].to_vec())
    }

    /// Like `render_layers_i420`, but RGBA8 with a transparent background
    /// instead of opaque black and without conversion to YUV: used to compose
    /// the nested timeline of a compound clip, whose result becomes
    /// a layer elsewhere in turn — the real alpha must be preserved, `_i420`
    /// would lose it (I420 has no alpha channel).
    pub fn render_layers_rgba_transparent(&self, layers: &[Layer], output: OutputFrame) -> Vec<u8> {
        let output_texture = self.render_layers_to_texture_transparent(layers, output);
        self.read_rgba_texture(&output_texture, output.width, output.height)
    }

    /// Reads an RGBA8 texture (`OUTPUT_FORMAT`) into a dense `Vec<u8>`,
    /// removing the row padding `wgpu` requires on the destination
    /// buffer.
    fn read_rgba_texture(&self, texture: &wgpu::Texture, width: u32, height: u32) -> Vec<u8> {
        let unpadded_bytes_per_row = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;

        let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vv-render readback buffer"),
            size: (padded_bytes_per_row * height) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vv-render readback encoder"),
            });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        self.map_read(&output_buffer, |data| {
            let mut out = Vec::with_capacity((unpadded_bytes_per_row * height) as usize);
            for row in 0..height {
                let start = (row * padded_bytes_per_row) as usize;
                out.extend_from_slice(&data[start..start + unpadded_bytes_per_row as usize]);
            }
            out
        })
    }

    /// Multi-layer version of [`Compositor::render_frame_to_texture`]
    /// (see [`Compositor::render_layers`]). Opaque black background: for the
    /// final video (preview, export) there is no "transparent".
    pub fn render_layers_to_texture(&self, layers: &[Layer], output: OutputFrame) -> wgpu::Texture {
        self.render_layers_to_texture_with_clear(layers, output, BLACK, Recycle::Immediately)
    }

    /// Like `render_layers_to_texture`, but without forcing an opaque background:
    /// used to compose the nested timeline of a compound clip, whose
    /// result becomes a layer elsewhere in turn — the areas where that
    /// timeline has nothing to show must stay transparent, not
    /// black, or they would cover what is below instead of letting it show
    /// (see `YuvFrame::alpha`, which carries this transparency around).
    pub fn render_layers_to_texture_transparent(&self, layers: &[Layer], output: OutputFrame) -> wgpu::Texture {
        self.render_layers_to_texture_with_clear(layers, output, TRANSPARENT, Recycle::Immediately)
    }

    /// Like `render_layers_to_texture_transparent`, but the texture stays with
    /// whoever receives it until they let it go, so it can be used in the meantime
    /// as a `Layer::Texture`: with immediate recycling the first render of the
    /// same size would draw over it.
    pub fn render_layers_to_owned_texture_transparent(&self, layers: &[Layer], output: OutputFrame) -> PooledTexture {
        PooledTexture {
            texture: Some(self.render_layers_to_texture_with_clear(layers, output, TRANSPARENT, Recycle::OnDrop)),
            pool: Arc::clone(&self.scratch),
        }
    }

    fn render_layers_to_texture_with_clear(
        &self,
        layers: &[Layer],
        output: OutputFrame,
        clear: wgpu::Color,
        recycle: Recycle,
    ) -> wgpu::Texture {
        let output_texture = match recycle {
            Recycle::Immediately => self.output_texture(output.width, output.height),
            Recycle::OnDrop => self.scratch_texture(output.width, output.height),
        };
        let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut planes = Vec::new();
        // Placeholder for the backdrop slot of the Normal layers, which do not
        // sample it (see `backdrop_tex` in the shader).
        let no_backdrop = self.plane_texture(OPAQUE, 1, 1);
        let no_backdrop_view = no_backdrop.create_view(&wgpu::TextureViewDescriptor::default());
        // Copy of the already composed stack, one per pass that needs it.
        let mut backdrops: Vec<Option<wgpu::Texture>> = Vec::new();

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vv-render transform encoder"),
            });

        // No layers: only the clear remains.
        if layers.is_empty() {
            self.pass(&mut encoder, &output_view, wgpu::LoadOp::Clear(clear), None);
        }
        let mut first = true;
        for layer in layers {
            let blend = layer_blend(layer);
            let backdrop_view = |backdrops: &mut Vec<Option<wgpu::Texture>>| {
                let texture = (blend != BlendMode::Normal)
                    .then(|| self.scratch_texture(output.width, output.height));
                let view = texture.as_ref().map_or_else(
                    || no_backdrop_view.clone(),
                    |t| t.create_view(&wgpu::TextureViewDescriptor::default()),
                );
                backdrops.push(texture);
                view
            };
            let backdrop_start = backdrops.len();
            let bind_groups = match layer {
                Layer::Video {
                    frame,
                    transform,
                    source_size,
                    opacity,
                    filters,
                    ..
                } => vec![self.layer_bind_group(
                    &mut planes,
                    frame,
                    transform,
                    output,
                    *source_size,
                    (frame.width, frame.height),
                    Fill::Video,
                    *opacity,
                    filters,
                    blend,
                    &backdrop_view(&mut backdrops),
                )],
                Layer::Texture {
                    texture,
                    transform,
                    source_size,
                    opacity,
                    filters,
                    ..
                } => vec![self.texture_bind_group(
                    &mut planes,
                    texture,
                    transform,
                    output,
                    *source_size,
                    *opacity,
                    filters,
                    blend,
                    &backdrop_view(&mut backdrops),
                )],
                // The color comes from the uniform: the planes are only placeholders.
                Layer::Solid { color, transform, opacity, filters, .. } => vec![self.layer_bind_group(
                    &mut planes,
                    &SOLID_PLACEHOLDER,
                    transform,
                    output,
                    output.timeline_size,
                    output.timeline_size,
                    Fill::Solid(*color),
                    *opacity,
                    filters,
                    blend,
                    &backdrop_view(&mut backdrops),
                )],
                Layer::Text { title, transform, opacity, filters, .. } => {
                    let render = crate::text::render_title(
                        title,
                        output.timeline_size,
                        (output.width, output.height),
                    );
                    let mut groups = Vec::with_capacity(render.layers.len());
                    for (mask, color) in &render.layers {
                        let frame = YuvFrame {
                            y: &mask.data,
                            width: mask.width,
                            height: mask.height,
                            ..SOLID_PLACEHOLDER
                        };
                        // Shadow, background and text are distinct passes: each one
                        // composes onto the earlier ones, backdrop included.
                        let view = backdrop_view(&mut backdrops);
                        groups.push(self.layer_bind_group(
                            &mut planes,
                            &frame,
                            transform,
                            output,
                            output.timeline_size,
                            output.timeline_size,
                            Fill::Mask(*color),
                            *opacity,
                            filters,
                            blend,
                            &view,
                        ));
                    }
                    groups
                }
            };
            let pipeline = if blend == BlendMode::Normal {
                &self.pipeline
            } else {
                &self.blend_pipeline
            };
            for (bind_group, backdrop) in bind_groups.iter().zip(&backdrops[backdrop_start..]) {
                if let Some(backdrop) = backdrop {
                    // The backdrop must be read from a copy: the output texture
                    // is already attached to the pass composing it. If this is the
                    // first layer, the clear must happen before copying it.
                    if first {
                        self.pass(&mut encoder, &output_view, wgpu::LoadOp::Clear(clear), None);
                        first = false;
                    }
                    encoder.copy_texture_to_texture(
                        output_texture.as_image_copy(),
                        backdrop.as_image_copy(),
                        output_texture.size(),
                    );
                }
                let load = if first {
                    wgpu::LoadOp::Clear(clear)
                } else {
                    wgpu::LoadOp::Load
                };
                first = false;
                self.pass(&mut encoder, &output_view, load, Some((bind_group, pipeline)));
            }
        }

        self.queue.submit(Some(encoder.finish()));
        give_back(&mut self.scratch.lock().unwrap(), backdrops.into_iter().flatten());
        let mut pool = self.pool.lock().unwrap();
        planes.push(no_backdrop);
        give_back(&mut pool.planes, planes);
        if recycle == Recycle::Immediately {
            // A copy stays in the pool: the next frame of the same
            // size draws over it, after the GPU has finished with
            // this one (same queue).
            give_back(&mut pool.outputs, [output_texture.clone()]);
        }
        output_texture
    }

    /// Maps `buffer` for reading (waiting for the GPU) and passes the bytes to `read`.
    fn map_read<R>(&self, buffer: &wgpu::Buffer, read: impl FnOnce(&[u8]) -> R) -> R {
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll del device wgpu fallito");
        rx.recv()
            .expect("il callback di map_async non ha risposto")
            .expect("map_async fallita");
        let out = read(&slice.get_mapped_range().expect("get_mapped_range fallita"));
        buffer.unmap();
        out
    }

    /// Uploads the three planes of the layer and the bind group ready for the pass:
    /// crop/zoom, letterbox and YUV→RGB conversion are all in the
    /// shader, here only its inputs are prepared.
    fn layer_bind_group(
        &self,
        planes: &mut Vec<wgpu::Texture>,
        frame: &YuvFrame,
        transform: &Transform,
        output: OutputFrame,
        source_size: (u32, u32),
        fit_size: (u32, u32),
        fill: Fill,
        opacity: f32,
        filters: &[vv_core::FilterKind],
        blend: BlendMode,
        backdrop: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let y_texture = self.plane_texture(frame.y, frame.width, frame.height);
        let u_texture = self.plane_texture(frame.u, frame.chroma_width, frame.chroma_height);
        let v_texture = self.plane_texture(frame.v, frame.chroma_width, frame.chroma_height);
        // A single byte = "opaque everywhere" placeholder (see the docs of
        // `YuvFrame::alpha`): the texture stays 1x1, sampled everywhere
        // by the `ClampToEdge` as Y/U/V already are for Solid/Text.
        let (alpha_w, alpha_h) = if frame.alpha.len() == 1 { (1, 1) } else { (frame.width, frame.height) };
        let a_texture = self.plane_texture(frame.alpha, alpha_w, alpha_h);
        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let u_view = u_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let v_view = v_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let a_view = a_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uniform = TransformUniform::new(
            transform,
            frame.matrix,
            frame.full_range,
            fit_factors(
                (fit_size.0.max(1) as f32, fit_size.1.max(1) as f32),
                (output.width as f32, output.height as f32),
            ),
            output,
            source_size,
            fill,
            opacity,
            filters,
            blend,
        );
        let bind_group = self.bind_group_for([&y_view, &u_view, &v_view, &a_view, backdrop], &uniform);
        planes.extend([y_texture, u_texture, v_texture, a_texture]);
        bind_group
    }

    /// Like `layer_bind_group`, but the source is an already composed RGBA
    /// texture (`Layer::Texture`): it takes the Y plane slot — the layout
    /// only asks for a filterable float 2D texture, and `Rgba8Unorm` satisfies
    /// it as much as `R8Unorm` — and the other slots take the 1x1
    /// placeholders, which with `Fill::Rgba` the shader does not sample (except the alpha,
    /// which must stay opaque).
    fn texture_bind_group(
        &self,
        planes: &mut Vec<wgpu::Texture>,
        texture: &wgpu::Texture,
        transform: &Transform,
        output: OutputFrame,
        source_size: (u32, u32),
        opacity: f32,
        filters: &[vv_core::FilterKind],
        blend: BlendMode,
        backdrop: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let u_texture = self.plane_texture(&[128], 1, 1);
        let v_texture = self.plane_texture(&[128], 1, 1);
        let a_texture = self.plane_texture(OPAQUE, 1, 1);
        let rgba_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let u_view = u_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let v_view = v_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let a_view = a_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uniform = TransformUniform::new(
            transform,
            ColorMatrix::Bt709,
            false,
            fit_factors(
                (texture.width().max(1) as f32, texture.height().max(1) as f32),
                (output.width as f32, output.height as f32),
            ),
            output,
            source_size,
            Fill::Rgba,
            opacity,
            filters,
            blend,
        );
        let bind_group = self.bind_group_for([&rgba_view, &u_view, &v_view, &a_view, backdrop], &uniform);
        planes.extend([u_texture, v_texture, a_texture]);
        bind_group
    }

    /// The bind group of the pass: the views in the order `[source, U, V, alpha,
    /// backdrop]` (the first is the Y plane or the RGBA texture, see `Fill`).
    fn bind_group_for(&self, views: [&wgpu::TextureView; 5], uniform: &TransformUniform) -> wgpu::BindGroup {
        let uniform_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vv-render transform uniform"),
                contents: bytemuck::bytes_of(uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vv-render transform bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(views[3]),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(views[4]),
                },
            ],
        })
    }

    /// An R8 plane with `data`, taken from the pool if there is one of the same
    /// size.
    fn plane_texture(&self, data: &[u8], width: u32, height: u32) -> wgpu::Texture {
        let pooled = take_sized(&mut self.pool.lock().unwrap().planes, width, height);
        let texture = pooled.unwrap_or_else(|| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vv-render plane"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: PLANE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        });
        self.queue.write_texture(
            texture.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            texture.size(),
        );
        texture
    }

    /// Like `output_texture`, but from the intermediates pool (see
    /// `PooledTexture`), separate because there a texture stays taken
    /// until whoever uses it gives it back.
    fn scratch_texture(&self, output_w: u32, output_h: u32) -> wgpu::Texture {
        take_sized(&mut self.scratch.lock().unwrap(), output_w, output_h)
            .unwrap_or_else(|| self.new_output_texture(output_w, output_h))
    }

    fn output_texture(&self, output_w: u32, output_h: u32) -> wgpu::Texture {
        if let Some(texture) = take_sized(&mut self.pool.lock().unwrap().outputs, output_w, output_h)
        {
            return texture;
        }
        self.new_output_texture(output_w, output_h)
    }

    fn new_output_texture(&self, output_w: u32, output_h: u32) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vv-render output frame"),
            size: wgpu::Extent3d {
                width: output_w,
                height: output_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUTPUT_FORMAT,
            // TEXTURE_BINDING is needed by the zero-copy path: egui-wgpu samples it.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    /// One pass on the output texture: `bind_group` absent = only the
    /// `load` (clear of a solid color or of black), no draw.
    fn pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        output_view: &wgpu::TextureView,
        load: wgpu::LoadOp<wgpu::Color>,
        bind_group: Option<(&wgpu::BindGroup, &wgpu::RenderPipeline)>,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vv-render transform pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: output_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some((bind_group, pipeline)) = bind_group {
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}

/// The compositing method of a layer, whatever its source.
fn layer_blend(layer: &Layer) -> BlendMode {
    match layer {
        Layer::Video { blend, .. }
        | Layer::Texture { blend, .. }
        | Layer::Solid { blend, .. }
        | Layer::Text { blend, .. } => *blend,
    }
}

/// Letterbox/pillarbox factors passed to the shader: >1 on the axis that
/// stays uncovered (black bars), 1 on the other.
fn fit_factors(source: (f32, f32), output: (f32, f32)) -> [f32; 2] {
    let source_aspect = source.0 / source.1;
    let output_aspect = output.0 / output.1;
    if source_aspect > output_aspect {
        [1.0, source_aspect / output_aspect]
    } else {
        [output_aspect / source_aspect, 1.0]
    }
}

/// Dimensions with the aspect ratio of `aspect` containing `source` without
/// scaling it: only the bars are added.
pub fn fit_output_size(source: (u32, u32), aspect: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (source.0.max(1) as f64, source.1.max(1) as f64);
    let (aw, ah) = (aspect.0.max(1) as f64, aspect.1.max(1) as f64);
    if sw / sh > aw / ah {
        (source.0.max(1), ((sw * ah / aw).round() as u32).max(1))
    } else {
        (((sh * aw / ah).round() as u32).max(1), source.1.max(1))
    }
}

#[cfg(test)]
impl<'a> YuvFrame<'a> {
    fn borrowed(&self) -> YuvFrame<'a> {
        YuvFrame { ..*self }
    }
}

#[cfg(test)]
impl Compositor {
    /// Composes the stack in alpha-over and reads the result in RGBA.
    pub fn render_layers(&self, layers: &[Layer], output: OutputFrame) -> Vec<u8> {
        let output_texture = self.render_layers_to_texture(layers, output);
        self.read_rgba_texture(&output_texture, output.width, output.height)
    }

    /// A single frame with `transform`, read in RGBA8.
    pub fn render_frame(
        &self,
        frame: &YuvFrame,
        transform: &Transform,
        output: OutputFrame,
    ) -> Vec<u8> {
        self.render_layers(
            &[Layer::Video {
                frame: frame.borrowed(),
                transform: *transform,
                source_size: (frame.width, frame.height),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            output,
        )
    }
    /// Like `render_frame` but stays on the GPU. No waiting: the egui pass
    /// sampling it is submitted afterwards on the same queue.
    pub fn render_frame_to_texture(
        &self,
        frame: &YuvFrame,
        transform: &Transform,
        output: OutputFrame,
    ) -> wgpu::Texture {
        self.render_layers_to_texture(
            &[Layer::Video {
                frame: frame.borrowed(),
                transform: *transform,
                source_size: (frame.width, frame.height),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            output,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// YUV420 frame owned by the test (the planes of `YuvFrame` are
    /// borrowed references): chroma dimensions computed as
    /// `vv_media::FrameYuv420` would compute them, rounded up.
    struct OwnedYuvFrame {
        width: u32,
        height: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        chroma_width: u32,
        chroma_height: u32,
        matrix: ColorMatrix,
        full_range: bool,
    }

    impl OwnedYuvFrame {
        fn as_yuv_frame(&self) -> YuvFrame<'_> {
            YuvFrame {
                y: &self.y,
                width: self.width,
                height: self.height,
                u: &self.u,
                v: &self.v,
                chroma_width: self.chroma_width,
                chroma_height: self.chroma_height,
                matrix: self.matrix,
                full_range: self.full_range,
                alpha: OPAQUE,
            }
        }
    }

    /// Uniform frame: same Y/U/V on every pixel.
    fn solid_frame(
        w: u32,
        h: u32,
        y: u8,
        u: u8,
        v: u8,
        matrix: ColorMatrix,
        full_range: bool,
    ) -> OwnedYuvFrame {
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        OwnedYuvFrame {
            width: w,
            height: h,
            y: vec![y; (w * h) as usize],
            u: vec![u; (cw * ch) as usize],
            v: vec![v; (cw * ch) as usize],
            chroma_width: cw,
            chroma_height: ch,
            matrix,
            full_range,
        }
    }

    /// 4x4 frame with four quadrants at different Y, neutral chroma
    /// (U=V=128) and full range: with neutral chroma R=G=B=Y exactly
    /// (see `yuv_to_rgb_reference`), useful to check *where* the
    /// crop picks from by looking at the red channel alone, without the
    /// color conversion adding another variable to the test.
    fn quadrant_frame() -> OwnedYuvFrame {
        let w: u32 = 4;
        let h: u32 = 4;
        let mut y_plane = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let level = match (x < w / 2, y < h / 2) {
                    (true, true) => 40u8,    // top-left
                    (false, true) => 100u8,  // top-right
                    (true, false) => 160u8,  // bottom-left
                    (false, false) => 220u8, // bottom-right
                };
                y_plane[(y * w + x) as usize] = level;
            }
        }
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        OwnedYuvFrame {
            width: w,
            height: h,
            y: y_plane,
            u: vec![128; (cw * ch) as usize],
            v: vec![128; (cw * ch) as usize],
            chroma_width: cw,
            chroma_height: ch,
            matrix: ColorMatrix::Bt601,
            full_range: true,
        }
    }


    /// Even for a "neutral" chroma (128), 128/255 is not exactly
    /// 0.5: a residue of a few levels in the 8 bits is expected
    /// quantization of the YUV→RGB matrix (the same residue appears
    /// in the f64 reference implementation, not just in the
    /// f32 shader), not an error — hence a small tolerance instead
    /// of an exact equality.
    fn assert_close_rgba(got: [u8; 4], expected: [u8; 4]) {
        for i in 0..4 {
            assert!(
                (got[i] as i16 - expected[i] as i16).abs() <= 2,
                "got={got:?} expected={expected:?}"
            );
        }
    }

    /// Reference implementation (CPU, f64) of the same formula
    /// used in the shader (`transform.wgsl`, `yuv_to_rgb`): it serves to
    /// check that the computation on the GPU (f32) is effectively
    /// that formula, not a silently different approximation
    /// (plans/REFACTOR_PIPELINE.md §5, frame accuracy is non-negotiable).
    fn yuv_to_rgb_reference(y: u8, u: u8, v: u8, matrix: ColorMatrix, full_range: bool) -> [u8; 3] {
        let (y_n, u_n, v_n) = if full_range {
            (
                y as f64 / 255.0,
                u as f64 / 255.0 - 0.5,
                v as f64 / 255.0 - 0.5,
            )
        } else {
            (
                (y as f64 - 16.0) / 219.0,
                (u as f64 - 128.0) / 224.0,
                (v as f64 - 128.0) / 224.0,
            )
        };
        let (kr, kb) = match matrix {
            ColorMatrix::Bt601 => (0.299, 0.114),
            ColorMatrix::Bt709 => (0.2126, 0.0722),
            ColorMatrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        let r = y_n + 2.0 * (1.0 - kr) * v_n;
        let b = y_n + 2.0 * (1.0 - kb) * u_n;
        let g = y_n - (2.0 * kr * (1.0 - kr) / kg) * v_n - (2.0 * kb * (1.0 - kb) / kg) * u_n;
        [r, g, b].map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    #[test]
    fn identity_transform_passes_through_solid_color() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(8, 8, 128, 128, 128, ColorMatrix::Bt601, true);
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), OutputFrame::exact(8, 8));

        assert_eq!(out.len(), 8 * 8 * 4);
        // Neutral chroma (128) and full range: Y=128 maps to R=G=B=128 within
        // a quantization residue (see assert_close_rgba).
        for px in out.as_chunks::<4>().0 {
            assert_close_rgba(*px, [128, 128, 128, 255]);
        }
    }

    /// Checks the conversion formula itself (not just that "a
    /// color gets through"): for every matrix/range, the GPU output must
    /// match the same formula computed on the CPU, within a
    /// small f32-vs-f64 rounding difference.
    #[test]
    fn yuv_to_rgb_matches_the_reference_formula_across_matrices_and_ranges() {
        let compositor = Compositor::new_headless();
        let cases = [
            (ColorMatrix::Bt601, false),
            (ColorMatrix::Bt601, true),
            (ColorMatrix::Bt709, false),
            (ColorMatrix::Bt709, true),
            (ColorMatrix::Bt2020, false),
            (ColorMatrix::Bt2020, true),
        ];
        // Non-degenerate Y/U/V (not all at half scale): really exercises
        // the matrix instead of reducing to a neutral grey.
        let (y, u, v) = (100u8, 90u8, 180u8);

        for (matrix, full_range) in cases {
            let input = solid_frame(2, 2, y, u, v, matrix, full_range);
            let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), OutputFrame::exact(2, 2));
            let expected = yuv_to_rgb_reference(y, u, v, matrix, full_range);
            let got = &out[0..3];
            for i in 0..3 {
                assert!(
                    (got[i] as i16 - expected[i] as i16).abs() <= 2,
                    "matrix={matrix:?} full_range={full_range}: got={got:?} expected={expected:?}"
                );
            }
        }
    }

    /// The crop just cuts: what is left keeps falling where it was
    /// in the frame, it is not recentered nor enlarged to fill it (where it was
    /// cut the layer below shows, here the black of the clear).
    #[test]
    fn crop_cuts_without_moving_what_is_left() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();

        let transform = Transform {
            crop: [0.0, 0.0, 2.0, 2.0], // away with the right half and the bottom half (2 px of 4)
            zoom: [1.0, 1.0],
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        // Neutral chroma: R matches the quadrant's Y exactly (40).
        assert_close_rgba(pixel(4, 4), [40, 40, 40, 255]);
        assert_eq!(pixel(12, 4), [0, 0, 0, 255], "alto-destra: tagliato");
        assert_eq!(pixel(4, 12), [0, 0, 0, 255], "basso-sinistra: tagliato");
        assert_eq!(pixel(12, 12), [0, 0, 0, 255], "basso-destra: tagliato");
    }

    #[test]
    fn crop_to_bottom_right_quadrant_leaves_it_in_the_bottom_right() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();

        let transform = Transform {
            crop: [2.0, 2.0, 0.0, 0.0],
            zoom: [1.0, 1.0],
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        assert_close_rgba(pixel(12, 12), [220, 220, 220, 255]);
        assert_eq!(pixel(4, 4), [0, 0, 0, 255], "alto-sinistra: tagliato");
    }

    #[test]
    fn taller_source_in_wider_output_gets_black_side_bars() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(8, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), OutputFrame::exact(32, 16));

        let pixel = |x: usize, y: usize| {
            let i = (y * 32 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };
        // 8:16 into 32:16 -> content 8 px wide, centered: columns 12..20.
        assert_eq!(pixel(0, 8), [0, 0, 0, 255]);
        assert_eq!(pixel(31, 8), [0, 0, 0, 255]);
        assert_close_rgba(pixel(16, 8), [255, 255, 255, 255]);
    }

    #[test]
    fn matching_aspect_ratio_leaves_no_bars() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(8, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), OutputFrame::exact(16, 32));

        for corner in [0usize, 15, 16 * 31, 16 * 32 - 1] {
            let i = corner * 4;
            assert_close_rgba(
                [out[i], out[i + 1], out[i + 2], out[i + 3]],
                [255, 255, 255, 255],
            );
        }
    }

    /// The zoom enlarges the clip *relative to the output frame*: a 9:16
    /// zoomed enough comes to cover a whole 16:9 frame, bars
    /// included (case reported by the user).
    #[test]
    fn zoom_enlarges_the_clip_until_it_covers_the_whole_output_frame() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(8, 16, 235, 128, 128, ColorMatrix::Bt709, false);

        let bars = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), OutputFrame::exact(32, 16));
        assert_eq!(&bars[0..4], &[0, 0, 0, 255], "a zoom 1 restano le bande");

        let transform = Transform {
            crop: [0.0; 4],
            zoom: [5.0, 5.0], // > 32/16 : 8/16, i.e. the factor covering the width
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let zoomed = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(32, 16));
        for px in zoomed.as_chunks::<4>().0 {
            assert_close_rgba(*px, [255, 255, 255, 255]);
        }
    }

    /// The position moves the clip *inside* the frame, not the content
    /// inside the clip.
    #[test]
    fn position_moves_the_clip_inside_the_output_frame() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(4, 4, 235, 128, 128, ColorMatrix::Bt709, false);

        let transform = Transform {
            crop: [0.0; 4],
            zoom: [1.0, 1.0],
            position: [8.0, 0.0], // half a frame to the right (output 16x16)
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        assert_eq!(pixel(2, 8), [0, 0, 0, 255], "metà sinistra: clip uscita");
        assert_close_rgba(pixel(14, 8), [255, 255, 255, 255]);
    }

    /// 90° rotation: the top-left quadrant ends up at the top
    /// right (clockwise rotation), and on a square output it is not deformed.
    #[test]
    fn rotation_turns_the_clip_clockwise_around_its_center() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            rotation: 90.0,
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        assert_close_rgba(pixel(12, 4), [40, 40, 40, 255]);
        assert_close_rgba(pixel(4, 4), [160, 160, 160, 255]);
    }

    /// The anchor point moves the zoom pivot: zooming around
    /// the top-left corner of the clip, that corner stays put.
    #[test]
    fn zoom_scales_around_the_anchor_point() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            zoom: [2.0, 2.0],
            anchor: [-8.0, 8.0], // top-left corner of the clip (output 16x16)
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        // With the pivot on the top-left corner of the clip, the top-left
        // quadrant widens until it covers the whole frame by itself:
        // with the pivot at the center, at (10, 10) one would instead see the
        // bottom-right quadrant.
        assert_close_rgba(pixel(2, 2), [40, 40, 40, 255]);
        assert_close_rgba(pixel(10, 10), [40, 40, 40, 255]);
    }

    /// Position and anchor are in *timeline* pixels: the preview composes
    /// at reduced resolution (proxy), but a clip moved by half a frame
    /// stays moved by half a frame.
    #[test]
    fn position_is_in_timeline_pixels_whatever_the_output_resolution() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(4, 4, 235, 128, 128, ColorMatrix::Bt709, false);
        let transform = Transform {
            position: [960.0, 0.0], // half a frame on a 1920x1080 timeline
            ..Transform::default()
        };
        // Output at 1/120 of the timeline: the clip must still start from
        // half the frame.
        let out = compositor.render_frame(
            &input.as_yuv_frame(),
            &transform,
            OutputFrame::scaled(16, 9, (1920, 1080)),
        );
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        assert_eq!(pixel(2, 4), [0, 0, 0, 255], "metà sinistra: vuota");
        assert_close_rgba(pixel(13, 4), [255, 255, 255, 255]);
    }

    /// The crop is in media pixels at its native resolution: on a
    /// proxy (smaller decoded frame) it cuts the same portion.
    #[test]
    fn crop_is_in_native_source_pixels_even_on_a_proxy_frame() {
        let compositor = Compositor::new_headless();
        // 4x4 decoded frame for a 1920x1080 native media.
        let input = quadrant_frame();
        let transform = Transform {
            crop: [0.0, 0.0, 960.0, 540.0], // away with the right half and the bottom half
            ..Transform::default()
        };
        let out = compositor.render_layers(
            &[Layer::Video {
                frame: input.as_yuv_frame(),
                transform,
                source_size: (1920, 1080),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            OutputFrame::scaled(16, 16, (1920, 1080)),
        );
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        assert_close_rgba(pixel(4, 4), [40, 40, 40, 255]);
        assert_eq!(pixel(12, 12), [0, 0, 0, 255]);
    }

    /// The Y axis is an NLE's, not the uv one: positive = up.
    #[test]
    fn a_positive_y_position_lifts_the_clip() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            position: [0.0, 8.0], // half a frame up (output 16x16)
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        // Raised by half a frame: at the top the bottom half of the clip remains, at the
        // bottom there is nothing left.
        assert_close_rgba(pixel(4, 4), [160, 160, 160, 255]);
        assert_eq!(pixel(4, 12), [0, 0, 0, 255]);
    }

    #[test]
    fn flip_mirrors_the_clip_on_each_axis() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            flip: [true, false],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        assert_close_rgba(pixel(12, 4), [40, 40, 40, 255]);
        assert_close_rgba(pixel(4, 4), [100, 100, 100, 255]);
    }

    /// The softness acts on the alpha: at the crop edge the layer becomes
    /// progressively transparent instead of cutting sharply. Negative = towards
    /// the inside of the crop.
    #[test]
    fn negative_crop_softness_fades_inward_from_the_edge() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(16, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let transform = Transform {
            crop: [0.0, 0.0, 8.0, 0.0], // away with the right half (8 px of 16)
            crop_softness: -1.6,
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let luma = |x: usize, y: usize| out[(y * 16 + x) * 4] as i32;

        // Black clear below: the closer to the crop edge, the darker.
        assert!(luma(4, 8) > 200, "lontano dal bordo: pieno");
        assert!(
            luma(7, 8) < luma(6, 8) && luma(6, 8) < luma(4, 8),
            "gradiente verso il bordo: {} {} {}",
            luma(4, 8),
            luma(6, 8),
            luma(7, 8)
        );
        assert_eq!(luma(9, 8), 0, "oltre il crop non si sfuma, si taglia");
    }

    /// Positive softness: the ramp falls *past* the crop edge, so
    /// it shows only where something was cut.
    #[test]
    fn positive_crop_softness_fades_outward_past_the_edge() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(16, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let transform = Transform {
            crop: [0.0, 0.0, 8.0, 0.0],
            crop_softness: 1.6,
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let luma = |x: usize, y: usize| out[(y * 16 + x) * 4] as i32;

        assert!(luma(7, 8) > 200, "dentro il crop resta pieno fino al bordo");
        assert!(
            luma(8, 8) > 0 && luma(8, 8) < luma(7, 8),
            "appena oltre il bordo si sfuma invece di sparire: {} {}",
            luma(7, 8),
            luma(8, 8)
        );
        assert_eq!(luma(12, 8), 0, "oltre la rampa non resta nulla");
    }

    /// The case reported by the user: a 9:16 clip on top of a 16:9 one in a
    /// 16:9 timeline — on the side bars the clip below must show,
    /// not black.
    #[test]
    fn side_bars_of_the_top_layer_show_the_layer_below() {
        let compositor = Compositor::new_headless();
        // White below (16:8 like the output), black above (8:16, narrow).
        let below = solid_frame(32, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let above = solid_frame(8, 16, 16, 128, 128, ColorMatrix::Bt709, false);
        let out = compositor.render_layers(
            &[
                Layer::Video {
                    frame: below.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (below.width, below.height),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
                Layer::Video {
                    frame: above.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (above.width, above.height),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
            ],
            OutputFrame::exact(32, 16),
        );

        let pixel = |x: usize, y: usize| {
            let i = (y * 32 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };
        assert_close_rgba(pixel(1, 8), [255, 255, 255, 255]);
        assert_close_rgba(pixel(30, 8), [255, 255, 255, 255]);
        assert_close_rgba(pixel(16, 8), [0, 0, 0, 255]);
    }

    #[test]
    fn a_solid_layer_covers_everything_below_it() {
        let compositor = Compositor::new_headless();
        let below = solid_frame(8, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let out = compositor.render_layers(
            &[
                Layer::Video {
                    frame: below.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (below.width, below.height),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
                Layer::Solid {
                    color: RED,
                    transform: Transform::default(),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
            ],
            OutputFrame::exact(16, 16),
        );
        assert!(out.as_chunks::<4>().0.iter().all(|px| px == &[255, 0, 0, 255]));
    }

    /// The black and white filter converts any kind of layer to luma,
    /// not just video.
    #[test]
    fn grayscale_flattens_a_solid_layer_to_its_luma() {
        let compositor = Compositor::new_headless();
        let out = compositor.render_layers(
            &[Layer::Solid {
                color: RED,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[vv_core::FilterKind::Grayscale],
                blend: BlendMode::Normal,
            }],
            OutputFrame::exact(4, 4),
        );
        let pixel = out.as_chunks::<4>().0[0];
        assert_eq!(pixel[0], pixel[1], "grigio: R=G=B");
        assert_eq!(pixel[1], pixel[2]);
        assert!(pixel[0] > 0 && pixel[0] < 255, "luma del rosso, non nero né bianco");
    }

    #[test]
    fn a_text_layer_paints_its_color_only_where_the_glyphs_are() {
        let compositor = Compositor::new_headless();
        let title = vv_core::TitleParams {
            content: "II".into(),
            color: RED,
            size: 60.0,
            ..Default::default()
        };
        let out = compositor.render_layers(
            &[Layer::Text {
                title: &title,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            OutputFrame::exact(160, 90),
        );
        let pixels = out.as_chunks::<4>().0;
        assert_eq!(pixels[0], [0, 0, 0, 255], "fuori dal testo resta il nero");
        assert!(pixels.iter().any(|px| px == &[255, 0, 0, 255]), "nessun pixel del testo");
    }

    #[test]
    fn a_text_shadow_darkens_the_layer_below() {
        let compositor = Compositor::new_headless();
        let title = vv_core::TitleParams {
            content: "II".into(),
            size: 60.0,
            shadow: vv_core::TitleShadow {
                enabled: true,
                offset: [10.0, -10.0],
                blur: 0.0,
                opacity: 100.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let out = compositor.render_layers(
            &[
                Layer::Solid {
                    color: RED,
                    transform: Transform::default(),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
                Layer::Text {
                    title: &title,
                    transform: Transform::default(),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
            ],
            OutputFrame::exact(160, 90),
        );
        let pixels = out.as_chunks::<4>().0;
        assert!(pixels.iter().any(|px| px == &[0, 0, 0, 255]), "nessun pixel d'ombra");
    }

    const BLUE: vv_core::Rgba = vv_core::Rgba {
        r: 0.0,
        g: 0.0,
        b: 1.0,
        a: 1.0,
    };

    const WHITE: vv_core::Rgba = vv_core::Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };

    /// Composing into an intermediate (the nested timeline of a compound
    /// clip) and reusing it as a `Layer::Texture` must give the same pixels
    /// as composing its layers directly: alpha-over is associative,
    /// and the round-trip through the texture must not introduce
    /// differences.
    #[test]
    fn a_texture_layer_composites_like_the_layers_it_was_made_of() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(16, 16);
        // Zoom < 1: around the red it stays uncovered, i.e. transparent
        // in the intermediate — it is the part that must let the blue show.
        let inner = Transform {
            zoom: [0.5, 0.5],
            ..Transform::default()
        };
        let below = || Layer::Solid {
            color: BLUE,
            transform: Transform::default(),
            opacity: 1.0,
            filters: &[],
            blend: BlendMode::Normal,
        };
        let above = || Layer::Solid {
            color: RED,
            transform: inner,
            opacity: 1.0,
            filters: &[],
            blend: BlendMode::Normal,
        };

        let nested = compositor.render_layers_to_owned_texture_transparent(&[above()], output);
        let via_texture = compositor.render_layers(
            &[
                below(),
                Layer::Texture {
                    texture: &nested,
                    transform: Transform::default(),
                    source_size: (16, 16),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
            ],
            output,
        );
        let direct = compositor.render_layers(&[below(), above()], output);

        for (i, (a, b)) in via_texture.iter().zip(direct.iter()).enumerate() {
            assert!(
                (*a as i16 - *b as i16).abs() <= 2,
                "byte {i}: via texture {a}, diretto {b}"
            );
        }
    }

    /// An intermediate is composed onto a transparent background with
    /// `ALPHA_BLENDING`, so its color is already multiplied by the
    /// alpha: reusing it as a layer without dividing it out would attenuate it a
    /// second time (alpha squared on the edges and on the fades).
    #[test]
    fn a_semitransparent_texture_layer_is_not_faded_twice() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(8, 8);
        let nested = compositor.render_layers_to_owned_texture_transparent(
            &[Layer::Solid {
                color: RED,
                transform: Transform::default(),
                opacity: 0.5,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            output,
        );
        let out = compositor.render_layers(
            &[
                Layer::Solid {
                    color: WHITE,
                    transform: Transform::default(),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
                Layer::Texture {
                    texture: &nested,
                    transform: Transform::default(),
                    source_size: (8, 8),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
            ],
            output,
        );
        // Red at 50% over white. With the double multiplication the
        // red would drop to ~191.
        assert_close_rgba(out.as_chunks::<4>().0[0], [255, 128, 128, 255]);
    }

    /// The intermediate goes back to its pool as soon as whoever uses it lets it go,
    /// and the next render finds it again instead of allocating.
    #[test]
    fn an_owned_texture_returns_to_the_pool_when_dropped() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(8, 8);

        let texture = compositor.render_layers_to_owned_texture_transparent(&[], output);
        assert!(compositor.scratch.lock().unwrap().is_empty(), "in uso, non nel pool");
        drop(texture);
        assert_eq!(compositor.scratch.lock().unwrap().len(), 1);

        let _reused = compositor.render_layers_to_owned_texture_transparent(&[], output);
        assert!(compositor.scratch.lock().unwrap().is_empty(), "ripresa dal pool, non allocata");
    }

    /// `render_layers_to_owned_texture_transparent` does not put the texture
    /// back into the pool: a later render of the same size must not
    /// draw over it, or the retained nested frame would be corrupted.
    #[test]
    fn an_owned_texture_is_not_recycled_by_the_next_render() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(8, 8);
        let nested = compositor.render_layers_to_owned_texture_transparent(
            &[Layer::Solid {
                color: RED,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            output,
        );
        compositor.render_layers(
            &[Layer::Solid {
                color: BLUE,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            output,
        );
        assert_eq!(read_back(&compositor, &nested, 8, 8).as_chunks::<4>().0[0], [255, 0, 0, 255]);
    }

    const RED: vv_core::Rgba = vv_core::Rgba {
        r: 1.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };

    #[test]
    fn a_solid_layer_is_cropped_and_moved_like_a_video_layer() {
        let compositor = Compositor::new_headless();
        // Crop in timeline pixels: right half cut, then moved
        // a quarter to the right.
        let out = compositor.render_layers(
            &[Layer::Solid {
                color: RED,
                transform: Transform {
                    crop: [0.0, 0.0, 8.0, 0.0],
                    position: [4.0, 0.0],
                    ..Transform::default()
                },
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            OutputFrame::scaled(8, 4, (16, 8)),
        );
        let px = |x: usize, y: usize| &out[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
        assert_eq!(px(1, 2), &[0, 0, 0, 255], "a sinistra resta scoperto");
        assert_eq!(px(3, 2), &[255, 0, 0, 255]);
        assert_eq!(px(6, 2), &[0, 0, 0, 255], "oltre il crop");
    }

    /// The layer opacity (clip fades) attenuates the alpha it composes with
    /// onto the layer below, both for video and for a solid color.
    #[test]
    fn layer_opacity_blends_with_what_is_below() {
        let compositor = Compositor::new_headless();
        let below = solid_frame(4, 4, 235, 128, 128, ColorMatrix::Bt709, false); // white
        let out = compositor.render_layers(
            &[
                Layer::Video {
                    frame: below.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (below.width, below.height),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
                Layer::Solid {
                    color: RED,
                    transform: Transform::default(),
                    opacity: 0.5,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
            ],
            OutputFrame::exact(4, 4),
        );
        let px = |out: &[u8], x: usize, y: usize| -> [u8; 4] {
            out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].try_into().unwrap()
        };
        assert_close_rgba(px(&out, 2, 2), [255, 127, 127, 255]); // 50% red over white = pink

        // Opacity 0: the layer above is not visible at all.
        let out = compositor.render_layers(
            &[
                Layer::Video {
                    frame: below.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (below.width, below.height),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
                Layer::Solid { color: RED, transform: Transform::default(), opacity: 0.0, filters: &[], blend: BlendMode::Normal },
            ],
            OutputFrame::exact(4, 4),
        );
        assert_close_rgba(px(&out, 2, 2), [255, 255, 255, 255]);
    }

    #[test]
    fn render_layers_i420_packs_dense_planes_for_odd_sizes() {
        let compositor = Compositor::new_headless();
        let out = compositor.render_layers_i420(
            &[Layer::Solid {
                color: RED,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            OutputFrame::exact(5, 3),
        );
        // Chroma 3x2; 15 + 6 + 6 = 27 bytes, not a multiple of 4.
        let mut expected = vec![63u8; 15];
        expected.extend([102; 6]);
        expected.extend([240; 6]);
        assert_eq!(out, expected);
    }

    #[test]
    fn render_layers_i420_reuses_its_buffers_across_frames_and_sizes() {
        let compositor = Compositor::new_headless();
        let solid = |color| {
            [Layer::Solid {
                color,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }]
        };
        let fresh = |color, w, h| {
            Compositor::new_headless().render_layers_i420(&solid(color), OutputFrame::exact(w, h))
        };
        let blue = vv_core::Rgba::from([0.0, 0.0, 1.0, 1.0]);
        for (color, w, h) in [(RED, 5, 3), (blue, 5, 3), (RED, 8, 6), (RED, 5, 3)] {
            let out = compositor.render_layers_i420(&solid(color), OutputFrame::exact(w, h));
            assert_eq!(out, fresh(color, w, h), "{w}x{h}");
        }
    }

    #[test]
    fn no_layers_renders_a_black_frame() {
        let compositor = Compositor::new_headless();
        let out = compositor.render_layers(&[], OutputFrame::exact(4, 4));
        assert!(out.as_chunks::<4>().0.iter().all(|px| px == &[0, 0, 0, 255]));
    }

    /// `render_layers_rgba_transparent` is what composes the nested
    /// timeline of a compound clip: the areas with nothing above must
    /// stay transparent (alpha 0), not black as for the final video —
    /// otherwise they would cover what is below when the compound clip
    /// becomes a layer elsewhere in turn.
    #[test]
    fn no_layers_renders_fully_transparent_with_the_transparent_variant() {
        let compositor = Compositor::new_headless();
        let out = compositor.render_layers_rgba_transparent(&[], OutputFrame::exact(4, 4));
        assert!(out.as_chunks::<4>().0.iter().all(|px| px == &[0, 0, 0, 0]));
    }

    #[test]
    fn render_layers_rgba_transparent_leaves_uncovered_areas_transparent_not_black() {
        let compositor = Compositor::new_headless();
        // Crop in timeline pixels: right half cut away.
        let out = compositor.render_layers_rgba_transparent(
            &[Layer::Solid {
                color: RED,
                transform: Transform {
                    crop: [0.0, 0.0, 8.0, 0.0],
                    ..Transform::default()
                },
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            OutputFrame::exact(16, 8),
        );
        let px = |x: usize, y: usize| &out[(y * 16 + x) * 4..(y * 16 + x) * 4 + 4];
        assert_eq!(px(2, 4), &[255, 0, 0, 255], "coperto dal layer: rosso opaco");
        assert_eq!(px(12, 4), &[0, 0, 0, 0], "scoperto: trasparente, non nero");
    }

    /// The mechanism by which an already composed compound clip comes back as a
    /// layer elsewhere: `YuvFrame::alpha` carries the real per-pixel coverage,
    /// not just the uniform `opacity` multiplier — where it is 0 it must
    /// let what is below show, exactly as a hole in the nested timeline
    /// it comes from would.
    #[test]
    fn a_videos_own_alpha_plane_lets_the_layer_below_show_through() {
        let compositor = Compositor::new_headless();
        let frame = solid_frame(4, 4, 255, 128, 128, ColorMatrix::Bt709, true); // white
        // Left opaque, right transparent.
        let alpha: Vec<u8> = (0..16u32).map(|i| if i % 4 < 2 { 255 } else { 0 }).collect();
        let out = compositor.render_layers(
            &[Layer::Video {
                frame: YuvFrame { alpha: &alpha, ..frame.as_yuv_frame() },
                transform: Transform::default(),
                source_size: (4, 4),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Normal,
            }],
            OutputFrame::exact(4, 4),
        );
        let px = |x: usize, y: usize| &out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(px(0, 0), &[255, 255, 255, 255], "sinistra: il video si vede, opaco");
        assert_eq!(px(3, 0), &[0, 0, 0, 255], "destra: alpha 0 nel piano lascia vedere il nero sotto");
    }

    #[test]
    fn fit_output_size_wraps_the_source_in_the_requested_aspect() {
        assert_eq!(fit_output_size((540, 960), (1920, 1080)), (1707, 960));
        assert_eq!(fit_output_size((1920, 1080), (1080, 1920)), (1920, 3413));
        assert_eq!(fit_output_size((1280, 720), (1920, 1080)), (1280, 720));
    }

    #[test]
    fn output_size_can_differ_from_input_size() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(4, 4, 1, 2, 3, ColorMatrix::Bt601, true);
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), OutputFrame::exact(37, 21));
        assert_eq!(out.len(), 37 * 21 * 4);
    }

    /// Reads back a texture created by this same `Compositor`
    /// (same device/queue) — it is not part of the public API, it only serves
    /// to check in the test that the zero-copy path produces exactly
    /// the same bytes as the readback path: a performance-only
    /// change must not alter a single pixel of what is
    /// shown (plans/REFACTOR_PIPELINE.md §5, frame accuracy is
    /// non-negotiable).
    fn read_back(compositor: &Compositor, texture: &wgpu::Texture, w: u32, h: u32) -> Vec<u8> {
        let unpadded_bytes_per_row = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;

        let buffer = compositor.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test readback buffer"),
            size: (padded_bytes_per_row * h) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = compositor
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        compositor.queue.submit(Some(encoder.finish()));

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        compositor
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        rx.recv().unwrap().unwrap();

        let mut out = Vec::with_capacity((unpadded_bytes_per_row * h) as usize);
        {
            let data = slice.get_mapped_range().unwrap();
            for row in 0..h {
                let start = (row * padded_bytes_per_row) as usize;
                out.extend_from_slice(&data[start..start + unpadded_bytes_per_row as usize]);
            }
        }
        buffer.unmap();
        out
    }

    #[test]
    fn render_frame_to_texture_produces_the_same_pixels_as_render_frame() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            crop: [0.0, 0.0, 0.5, 0.5],
            zoom: [1.0, 1.0],
            position: [0.0, 0.0],
            ..Transform::default()
        };

        let via_readback = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let texture = compositor.render_frame_to_texture(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let via_texture = read_back(&compositor, &texture, 16, 16);

        assert_eq!(
            via_readback, via_texture,
            "il path zero-copy deve produrre esattamente gli stessi pixel del path con readback"
        );
    }

    /// The compositing methods other than Normal read the already composed
    /// stack and apply their formula to it, channel by channel.
    #[test]
    fn a_blend_mode_combines_the_layer_with_what_is_below() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(8, 8);
        let grey = vv_core::Rgba { r: 0.5, g: 0.5, b: 0.5, a: 1.0 };
        let blended = |mode| {
            let out = compositor.render_layers(
                &[
                    Layer::Solid {
                        color: grey,
                        transform: Transform::default(),
                        opacity: 1.0,
                        filters: &[],
                        blend: BlendMode::Normal,
                    },
                    Layer::Solid {
                        color: grey,
                        transform: Transform::default(),
                        opacity: 1.0,
                        filters: &[],
                        blend: mode,
                    },
                ],
                output,
            );
            out.as_chunks::<4>().0[0]
        };
        assert_close_rgba(blended(BlendMode::Normal), [128, 128, 128, 255]);
        assert_close_rgba(blended(BlendMode::Multiply), [64, 64, 64, 255]);
        assert_close_rgba(blended(BlendMode::Screen), [191, 191, 191, 255]);
        assert_close_rgba(blended(BlendMode::Add), [255, 255, 255, 255]);
        assert_close_rgba(blended(BlendMode::Difference), [0, 0, 0, 255]);
        assert_close_rgba(blended(BlendMode::Subtract), [0, 0, 0, 255]);
    }

    /// A blended layer does not erase the background where it does not cover: outside the
    /// crop what was below remains (the REPLACE pipeline would write
    /// zeros if the shader did not recompose the backdrop).
    #[test]
    fn a_blended_layer_leaves_the_backdrop_where_it_does_not_cover() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(16, 16);
        let out = compositor.render_layers(
            &[
                Layer::Solid {
                    color: BLUE,
                    transform: Transform::default(),
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Normal,
                },
                Layer::Solid {
                    color: WHITE,
                    // Away with the right half: the blue must remain there.
                    transform: Transform {
                        crop: [0.0, 0.0, 8.0, 0.0],
                        ..Transform::default()
                    },
                    opacity: 1.0,
                    filters: &[],
                    blend: BlendMode::Screen,
                },
            ],
            output,
        );
        let pixels = out.as_chunks::<4>().0;
        assert_close_rgba(pixels[0], [255, 255, 255, 255]);
        assert_close_rgba(pixels[12], [0, 0, 255, 255]);
    }

    /// The first layer of a stack can be blended: the clear must happen
    /// first, or the backdrop it reads would be the previous frame.
    #[test]
    fn the_first_layer_can_be_blended_over_the_clear() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(8, 8);
        let out = compositor.render_layers(
            &[Layer::Solid {
                color: RED,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[],
                blend: BlendMode::Screen,
            }],
            output,
        );
        // Screen over black (the clear) leaves the color as it is.
        assert_close_rgba(out.as_chunks::<4>().0[0], [255, 0, 0, 255]);
    }

}
