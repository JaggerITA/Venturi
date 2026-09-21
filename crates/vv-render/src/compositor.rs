//! Compositing GPU: input YUV420 planare convertito a RGB nello shader,
//! layer composti in alpha-over dal basso verso l'alto.
//!
//! - `render_layers` / `render_layers_i420`: readback in RGBA o I420
//!   (export).
//! - `render_layers_to_texture`: resta sulla GPU, per l'anteprima che
//!   registra la texture in egui-wgpu. Richiede `Compositor::new` sullo
//!   stesso device di egui.

use std::sync::{Arc, Mutex};
use vv_core::{ColorMatrix, Transform};
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
/// Placeholder di `YuvFrame::alpha` quando il layer non porta una vera
/// copertura per pixel: un solo byte, campionato ovunque (`ClampToEdge`) —
/// zero costo per il caso comune (video/solid/text, sempre opachi).
const OPAQUE: &[u8] = &[255];
/// Formato dei tre piani di input (Y/U/V): un solo canale 8 bit, letto
/// come `.r` nello shader.
const PLANE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Selettore della matrice per lo shader: deve restare allineato a
/// `kr_kb` in `transform.wgsl`.
fn shader_matrix_id(matrix: ColorMatrix) -> f32 {
    match matrix {
        ColorMatrix::Bt601 => 0.0,
        ColorMatrix::Bt709 => 1.0,
        ColorMatrix::Bt2020 => 2.0,
    }
}

/// Frame YUV420 8 bit con i metadati colore. Piani densi, senza padding.
pub struct YuvFrame<'a> {
    pub y: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub u: &'a [u8],
    pub v: &'a [u8],
    /// Dimensioni dei piani U/V (sottocampionati 4:2:0, tipicamente
    /// `(width+1)/2` x `(height+1)/2` ma non ricalcolate qui: il
    /// chiamante passa le dimensioni reali allocate dal decoder).
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub matrix: ColorMatrix,
    /// `true` = range JPEG/full (0-255), `false` = range MPEG/limited.
    pub full_range: bool,
    /// Copertura per pixel, non sottocampionata: un solo byte (`&[255]`,
    /// vedi `SOLID_PLACEHOLDER`) per "opaco ovunque" (un file decodificato
    /// non ha canale alpha), altrimenti `width`x`height` byte come Y — vedi
    /// `FrameYuv420::alpha`, da cui viene quando non è il placeholder.
    pub alpha: &'a [u8],
}

/// Un layer dello stack. `Solid` e `Text` si trattano come sorgenti grandi
/// quanto la timeline: stesso transform/crop.
pub enum Layer<'a> {
    Video {
        frame: YuvFrame<'a>,
        transform: Transform,
        /// Risoluzione *nativa* del media, in cui è espresso il crop in
        /// pixel del `Transform`: non quella di `frame`, che può essere un
        /// proxy a risoluzione ridotta.
        source_size: (u32, u32),
        /// Moltiplicatore di alpha di tutto il layer (dissolvenze di clip):
        /// 1.0 = nessuna attenuazione.
        opacity: f32,
        /// Filtri attivi della clip (`EffectStack::filters`), nell'ordine
        /// in cui vanno applicati: vv-render non sa cosa ciascuno significhi,
        /// solo l'id dello shader che gli corrisponde (`filter_shader_id`).
        filters: &'a [vv_core::FilterKind],
    },
    /// Un frame già composto e residente sulla GPU: la timeline annidata di
    /// una compound clip, che torna layer nella timeline esterna senza
    /// passare dalla CPU. RGBA premoltiplicato — vedi `Fill::Rgba`.
    Texture {
        texture: &'a wgpu::Texture,
        transform: Transform,
        /// Come in `Video`: le unità del crop, che possono non essere le
        /// dimensioni di `texture` (anteprima a risoluzione ridotta).
        source_size: (u32, u32),
        opacity: f32,
        filters: &'a [vv_core::FilterKind],
    },
    Solid {
        color: vv_core::Rgba,
        transform: Transform,
        opacity: f32,
        filters: &'a [vv_core::FilterKind],
    },
    /// Titolo: rasterizzato alla risoluzione di output (vedi `text`), poi
    /// trattato come un `Solid` grande quanto la timeline.
    Text {
        title: &'a vv_core::TitleParams,
        transform: Transform,
        opacity: f32,
        filters: &'a [vv_core::FilterKind],
    },
}

/// Fino a quanti filtri per layer può portare l'uniform (vedi `filters` in
/// `TransformUniform`): oltre, i filtri in eccesso sono ignorati. Generoso
/// per l'uso reale, evita un buffer di dimensione dinamica per lo shader.
const MAX_LAYER_FILTERS: usize = 8;

/// Id per lo shader di ciascun `FilterKind`; 0 è riservato a "slot vuoto".
fn filter_shader_id(kind: vv_core::FilterKind) -> f32 {
    match kind {
        vv_core::FilterKind::Grayscale => 1.0,
    }
}

/// Cosa colora un layer: i piani Y/U/V, un colore pieno, o un colore
/// pieno con la copertura presa dal piano Y.
#[derive(Clone, Copy)]
enum Fill {
    Video,
    Solid(vv_core::Rgba),
    Mask(vv_core::Rgba),
    /// Texture RGBA già composta, con il colore premoltiplicato per l'alpha
    /// (è il risultato di un `ALPHA_BLENDING` su clear trasparente): lo
    /// shader lo divide prima di rimetterlo in alpha-over.
    Rgba,
}

/// Risoluzione in pixel della texture prodotta e quella logica della
/// timeline, in cui sono espressi posizione e anchor. Coincidono
/// nell'export; l'anteprima compone alla risoluzione del frame decodificato.
#[derive(Debug, Clone, Copy)]
pub struct OutputFrame {
    pub width: u32,
    pub height: u32,
    pub timeline_size: (u32, u32),
}

impl OutputFrame {
    /// Output alla risoluzione della timeline.
    pub fn exact(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            timeline_size: (width, height),
        }
    }

    /// Output a una risoluzione diversa da quella della timeline, con lo
    /// stesso aspect ratio.
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
    /// x: opacità dell'intero layer (dissolvenze di clip). y/z/w inutilizzati.
    extra: [f32; 4],
    /// Id shader dei filtri attivi, nell'ordine di applicazione (vedi
    /// `filter_shader_id`); 0 = slot vuoto. `MAX_LAYER_FILTERS` in due vec4
    /// per l'allineamento dell'uniform.
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
    ) -> Self {
        let (mode, solid) = match fill {
            Fill::Video => (0.0, None),
            Fill::Solid(c) => (1.0, Some(c)),
            Fill::Mask(c) => (2.0, Some(c)),
            Fill::Rgba => (3.0, None),
        };
        // Il `Transform` è in pixel — di timeline per posizione e anchor,
        // del media per il crop; lo shader lavora in coordinate
        // normalizzate.
        let (frame_w, frame_h) = (
            output.timeline_size.0.max(1) as f32,
            output.timeline_size.1.max(1) as f32,
        );
        let (source_w, source_h) = (source_size.0.max(1) as f32, source_size.1.max(1) as f32);
        Self {
            // Dai tagli per lato al rettangolo che lo shader campiona.
            crop: [
                t.crop[0] / source_w,
                t.crop[1] / source_h,
                1.0 - t.crop[2] / source_w,
                1.0 - t.crop[3] / source_h,
            ],
            // L'asse Y del modello punta in alto (come in un NLE), quello
            // delle uv in basso.
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
                // La sfumatura segue il crop: pixel del media, e sull'asse
                // più corto, così resta isotropa.
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
            extra: [opacity.clamp(0.0, 1.0), 0.0, 0.0, 0.0],
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
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    i420_pipeline: wgpu::ComputePipeline,
    /// Texture riusate da un frame all'altro, per dimensione: allocarne di
    /// nuove a ogni frame costa più del disegno stesso.
    pool: Mutex<TexturePool>,
}

#[derive(Default)]
struct TexturePool {
    planes: Vec<wgpu::Texture>,
    outputs: Vec<wgpu::Texture>,
    i420: Option<I420Buffers>,
}

/// Buffer della conversione I420, per la dimensione dell'ultimo frame.
struct I420Buffers {
    size: wgpu::BufferAddress,
    storage: wgpu::Buffer,
    params: wgpu::Buffer,
    readback: wgpu::Buffer,
}

/// Se la texture di output torna nel pool o resta a chi la riceve: vedi
/// `render_layers_to_owned_texture_transparent`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Recycle {
    Yes,
    No,
}

/// Oltre, le texture di dimensioni non più usate vengono lasciate andare.
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

        // Tre texture di input (Y/U/V, binding 0-2) invece di una sola
        // RGBA: la conversione YUV→RGB avviene nello shader
        // (REFACTOR_PIPELINE.md B3), qui arrivano solo i piani grezzi.
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
                plane_entry(5), // Alpha (copertura per pixel, vedi YuvFrame::alpha)
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vv-render transform pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vv-render transform pipeline"),
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
                    // Non REPLACE: le zone scoperte (letterbox) escono con alpha 0 e devono
                    // mostrare il layer sotto.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

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
            bind_group_layout,
            sampler,
            i420_pipeline,
            pool: Mutex::default(),
        }
    }

    /// Crea un device wgpu indipendente (headless, nessuna surface) per
    /// usare il compositor fuori da un contesto eframe/egui-wgpu — utile
    /// per l'app oggi e per i test.
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

    /// Come `render_layers`, ma I420 denso BT.709 limited: la conversione su
    /// GPU risparmia quella su CPU e dimezza il readback.
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

    /// Come `render_layers_i420`, ma RGBA8 con lo sfondo trasparente
    /// invece che nero opaco e senza conversione a YUV: usata per comporre
    /// la timeline annidata di una compound clip, il cui risultato torna a
    /// sua volta un layer altrove — l'alpha vera va preservata, `_i420` la
    /// perderebbe (l'I420 non ha canale alpha).
    pub fn render_layers_rgba_transparent(&self, layers: &[Layer], output: OutputFrame) -> Vec<u8> {
        let output_texture = self.render_layers_to_texture_transparent(layers, output);
        self.read_rgba_texture(&output_texture, output.width, output.height)
    }

    /// Legge una texture RGBA8 (`OUTPUT_FORMAT`) in un `Vec<u8>` denso,
    /// rimuovendo il padding di riga che `wgpu` richiede sul buffer di
    /// destinazione.
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

    /// Versione multi-layer di [`Compositor::render_frame_to_texture`]
    /// (vedi [`Compositor::render_layers`]). Sfondo nero opaco: per il
    /// video finale (anteprima, export) non esiste "trasparente".
    pub fn render_layers_to_texture(&self, layers: &[Layer], output: OutputFrame) -> wgpu::Texture {
        self.render_layers_to_texture_with_clear(layers, output, BLACK, Recycle::Yes)
    }

    /// Come `render_layers_to_texture`, ma senza forzare uno sfondo opaco:
    /// usata per comporre la timeline annidata di una compound clip, il cui
    /// risultato torna a sua volta un layer altrove — le zone dove quella
    /// timeline non ha nulla da mostrare devono restare trasparenti, non
    /// nere, o coprirebbero quel che c'è sotto invece di lasciarlo vedere
    /// (vedi `YuvFrame::alpha`, che porta questa trasparenza in giro).
    pub fn render_layers_to_texture_transparent(&self, layers: &[Layer], output: OutputFrame) -> wgpu::Texture {
        self.render_layers_to_texture_with_clear(layers, output, TRANSPARENT, Recycle::Yes)
    }

    /// Come `render_layers_to_texture_transparent`, ma la texture **non**
    /// torna nel pool: la tiene chi la riceve, per riusarla come
    /// `Layer::Texture` in una composizione successiva. Col riciclo, il
    /// primo render della stessa dimensione le disegnerebbe sopra.
    pub fn render_layers_to_owned_texture_transparent(&self, layers: &[Layer], output: OutputFrame) -> wgpu::Texture {
        self.render_layers_to_texture_with_clear(layers, output, TRANSPARENT, Recycle::No)
    }

    fn render_layers_to_texture_with_clear(
        &self,
        layers: &[Layer],
        output: OutputFrame,
        clear: wgpu::Color,
        recycle: Recycle,
    ) -> wgpu::Texture {
        let output_texture = self.output_texture(output.width, output.height);
        let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut planes = Vec::new();

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vv-render transform encoder"),
            });

        // Nessun layer: resta il solo clear.
        if layers.is_empty() {
            self.pass(&mut encoder, &output_view, wgpu::LoadOp::Clear(clear), None);
        }
        let mut first = true;
        for layer in layers {
            let bind_groups = match layer {
                Layer::Video {
                    frame,
                    transform,
                    source_size,
                    opacity,
                    filters,
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
                )],
                Layer::Texture {
                    texture,
                    transform,
                    source_size,
                    opacity,
                    filters,
                } => vec![self.texture_bind_group(
                    &mut planes,
                    texture,
                    transform,
                    output,
                    *source_size,
                    *opacity,
                    filters,
                )],
                // Il colore arriva dall'uniform: i piani sono solo segnaposto.
                Layer::Solid { color, transform, opacity, filters } => vec![self.layer_bind_group(
                    &mut planes,
                    &SOLID_PLACEHOLDER,
                    transform,
                    output,
                    output.timeline_size,
                    output.timeline_size,
                    Fill::Solid(*color),
                    *opacity,
                    filters,
                )],
                Layer::Text { title, transform, opacity, filters } => {
                    let render = crate::text::render_title(
                        title,
                        output.timeline_size,
                        (output.width, output.height),
                    );
                    render
                        .layers
                        .iter()
                        .map(|(mask, color)| {
                            let frame = YuvFrame {
                                y: &mask.data,
                                width: mask.width,
                                height: mask.height,
                                ..SOLID_PLACEHOLDER
                            };
                            self.layer_bind_group(
                                &mut planes,
                                &frame,
                                transform,
                                output,
                                output.timeline_size,
                                output.timeline_size,
                                Fill::Mask(*color),
                                *opacity,
                                filters,
                            )
                        })
                        .collect()
                }
            };
            for bind_group in &bind_groups {
                let load = if first {
                    wgpu::LoadOp::Clear(clear)
                } else {
                    wgpu::LoadOp::Load
                };
                first = false;
                self.pass(&mut encoder, &output_view, load, Some(bind_group));
            }
        }

        self.queue.submit(Some(encoder.finish()));
        let mut pool = self.pool.lock().unwrap();
        give_back(&mut pool.planes, planes);
        if recycle == Recycle::Yes {
            // Una copia resta nel pool: il prossimo frame della stessa
            // dimensione ci ridisegna sopra, dopo che la GPU ha finito con
            // questo (stessa coda).
            give_back(&mut pool.outputs, [output_texture.clone()]);
        }
        output_texture
    }

    /// Mappa `buffer` in lettura (aspettando la GPU) e passa i byte a `read`.
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

    /// Upload dei tre piani del layer e bind group pronto per il pass:
    /// crop/zoom, letterbox e conversione YUV→RGB stanno tutti nello
    /// shader, qui si preparano solo i suoi input.
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
    ) -> wgpu::BindGroup {
        let y_texture = self.plane_texture(frame.y, frame.width, frame.height);
        let u_texture = self.plane_texture(frame.u, frame.chroma_width, frame.chroma_height);
        let v_texture = self.plane_texture(frame.v, frame.chroma_width, frame.chroma_height);
        // Un solo byte = placeholder "opaco ovunque" (vedi doc di
        // `YuvFrame::alpha`): la texture resta 1x1, campionata ovunque
        // dal `ClampToEdge` come già Y/U/V per Solid/Text.
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
        );
        let bind_group = self.bind_group_for([&y_view, &u_view, &v_view, &a_view], &uniform);
        planes.extend([y_texture, u_texture, v_texture, a_texture]);
        bind_group
    }

    /// Come `layer_bind_group`, ma la sorgente è una texture RGBA già
    /// composta (`Layer::Texture`): occupa lo slot del piano Y — il layout
    /// chiede solo una texture 2D float filtrabile, e `Rgba8Unorm` lo
    /// soddisfa quanto `R8Unorm` — e gli altri slot prendono i placeholder
    /// 1x1, che con `Fill::Rgba` lo shader non campiona (tranne l'alpha,
    /// che deve restare opaco).
    fn texture_bind_group(
        &self,
        planes: &mut Vec<wgpu::Texture>,
        texture: &wgpu::Texture,
        transform: &Transform,
        output: OutputFrame,
        source_size: (u32, u32),
        opacity: f32,
        filters: &[vv_core::FilterKind],
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
        );
        let bind_group = self.bind_group_for([&rgba_view, &u_view, &v_view, &a_view], &uniform);
        planes.extend([u_texture, v_texture, a_texture]);
        bind_group
    }

    /// Il bind group del pass: le view nell'ordine `[sorgente, U, V, alpha]`
    /// (la prima è il piano Y o la texture RGBA, vedi `Fill`).
    fn bind_group_for(&self, views: [&wgpu::TextureView; 4], uniform: &TransformUniform) -> wgpu::BindGroup {
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
            ],
        })
    }

    /// Un piano R8 con `data`, preso dal pool se ce n'è uno della stessa
    /// dimensione.
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

    fn output_texture(&self, output_w: u32, output_h: u32) -> wgpu::Texture {
        if let Some(texture) = take_sized(&mut self.pool.lock().unwrap().outputs, output_w, output_h)
        {
            return texture;
        }
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
            // TEXTURE_BINDING serve al path zero-copy: egui-wgpu la campiona.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    /// Un pass sulla texture di output: `bind_group` assente = solo il
    /// `load` (clear di un colore pieno o di nero), nessun draw.
    fn pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        output_view: &wgpu::TextureView,
        load: wgpu::LoadOp<wgpu::Color>,
        bind_group: Option<&wgpu::BindGroup>,
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
        if let Some(bind_group) = bind_group {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}

/// Fattori di letterbox/pillarbox passati allo shader: >1 sull'asse che
/// resta scoperto (bande nere), 1 sull'altro.
fn fit_factors(source: (f32, f32), output: (f32, f32)) -> [f32; 2] {
    let source_aspect = source.0 / source.1;
    let output_aspect = output.0 / output.1;
    if source_aspect > output_aspect {
        [1.0, source_aspect / output_aspect]
    } else {
        [output_aspect / source_aspect, 1.0]
    }
}

/// Dimensioni con l'aspect ratio di `aspect` che contengono `source` senza
/// scalarlo: si aggiungono solo le bande.
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
    /// Compone lo stack in alpha-over e legge il risultato in RGBA.
    pub fn render_layers(&self, layers: &[Layer], output: OutputFrame) -> Vec<u8> {
        let output_texture = self.render_layers_to_texture(layers, output);
        self.read_rgba_texture(&output_texture, output.width, output.height)
    }

    /// Un solo frame con `transform`, letto in RGBA8.
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
            }],
            output,
        )
    }
    /// Come `render_frame` ma resta sulla GPU. Nessuna attesa: il pass di egui
    /// che la campiona è sottomesso dopo sulla stessa coda.
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
            }],
            output,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame YUV420 posseduto dal test (i piani di `YuvFrame` sono
    /// riferimenti in prestito): dimensioni croma calcolate come
    /// `vv_media::FrameYuv420` le calcolerebbe, arrotondate per eccesso.
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

    /// Frame uniforme: stesso Y/U/V su ogni pixel.
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

    /// Frame 4x4 con quattro quadranti a Y diverso, croma neutra
    /// (U=V=128) e range full: con croma neutra R=G=B=Y esattamente
    /// (vedi `yuv_to_rgb_reference`), utile per verificare *dove* il
    /// crop va a pescare guardando solo il canale rosso, senza che la
    /// conversione colore aggiunga un'altra variabile al test.
    fn quadrant_frame() -> OwnedYuvFrame {
        let w: u32 = 4;
        let h: u32 = 4;
        let mut y_plane = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let level = match (x < w / 2, y < h / 2) {
                    (true, true) => 40u8,    // alto-sinistra
                    (false, true) => 100u8,  // alto-destra
                    (true, false) => 160u8,  // basso-sinistra
                    (false, false) => 220u8, // basso-destra
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


    /// Anche per una croma "neutra" (128), 128/255 non è esattamente
    /// 0.5: un residuo di pochi livelli negli 8 bit è quantizzazione
    /// attesa della matrice YUV→RGB (lo stesso residuo compare
    /// nell'implementazione di riferimento in f64, non solo nello
    /// shader f32), non un errore — da qui una tolleranza piccola invece
    /// di un'uguaglianza esatta.
    fn assert_close_rgba(got: [u8; 4], expected: [u8; 4]) {
        for i in 0..4 {
            assert!(
                (got[i] as i16 - expected[i] as i16).abs() <= 2,
                "got={got:?} expected={expected:?}"
            );
        }
    }

    /// Implementazione di riferimento (CPU, f64) della stessa formula
    /// usata nello shader (`transform.wgsl`, `yuv_to_rgb`): serve a
    /// verificare che il calcolo sulla GPU (f32) sia effettivamente
    /// quella formula, non una approssimazione silenziosamente diversa
    /// (REFACTOR_PIPELINE.md §5, accuratezza del frame non negoziabile).
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
        // Croma neutra (128) e range full: Y=128 mappa a R=G=B=128 a
        // meno di un residuo di quantizzazione (vedi assert_close_rgba).
        for px in out.as_chunks::<4>().0 {
            assert_close_rgba(*px, [128, 128, 128, 255]);
        }
    }

    /// Verifica la formula di conversione stessa (non solo che "un
    /// colore passa"): per ogni matrice/range, l'output della GPU deve
    /// combaciare con la stessa formula calcolata su CPU, a meno di un
    /// piccolo scarto di arrotondamento f32-vs-f64.
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
        // Y/U/V non degeneri (non tutti a metà scala): esercita davvero
        // la matrice invece di ridursi a un grigio neutro.
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

    /// Il crop taglia e basta: quel che resta continua a cadere dov'era
    /// nel frame, non viene ricentrato né ingrandito per riempirlo (dove è
    /// stato tagliato si vede il layer sotto, qui il nero del clear).
    #[test]
    fn crop_cuts_without_moving_what_is_left() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();

        let transform = Transform {
            crop: [0.0, 0.0, 2.0, 2.0], // via metà destra e metà bassa (2 px su 4)
            zoom: [1.0, 1.0],
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        // Croma neutra: R combacia esattamente con la Y del quadrante (40).
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
        // 8:16 in 32:16 -> contenuto largo 8 px, centrato: colonne 12..20.
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

    /// Lo zoom ingrandisce la clip *rispetto al frame di output*: una 9:16
    /// zoomata abbastanza arriva a coprire tutto un frame 16:9, bande
    /// comprese (caso segnalato dall'utente).
    #[test]
    fn zoom_enlarges_the_clip_until_it_covers_the_whole_output_frame() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(8, 16, 235, 128, 128, ColorMatrix::Bt709, false);

        let bars = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), OutputFrame::exact(32, 16));
        assert_eq!(&bars[0..4], &[0, 0, 0, 255], "a zoom 1 restano le bande");

        let transform = Transform {
            crop: [0.0; 4],
            zoom: [5.0, 5.0], // > 32/16 : 8/16, cioè il fattore che copre la larghezza
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let zoomed = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(32, 16));
        for px in zoomed.as_chunks::<4>().0 {
            assert_close_rgba(*px, [255, 255, 255, 255]);
        }
    }

    /// La posizione sposta la clip *dentro* il frame, non il contenuto
    /// dentro la clip.
    #[test]
    fn position_moves_the_clip_inside_the_output_frame() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(4, 4, 235, 128, 128, ColorMatrix::Bt709, false);

        let transform = Transform {
            crop: [0.0; 4],
            zoom: [1.0, 1.0],
            position: [8.0, 0.0], // mezzo frame a destra (output 16x16)
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

    /// Rotazione di 90°: il quadrante alto-sinistra finisce in alto a
    /// destra (rotazione oraria), e su un output quadrato non si deforma.
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

    /// L'anchor point sposta il pivot dello zoom: zoomando attorno
    /// all'angolo alto-sinistra della clip, quell'angolo resta fermo.
    #[test]
    fn zoom_scales_around_the_anchor_point() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            zoom: [2.0, 2.0],
            anchor: [-8.0, 8.0], // angolo alto-sinistra della clip (output 16x16)
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        // Con il pivot sull'angolo alto-sinistra della clip, il quadrante
        // alto-sinistra si allarga fino a coprire da solo tutto il frame:
        // con il pivot al centro, a (10, 10) si vedrebbe invece il
        // quadrante basso-destra.
        assert_close_rgba(pixel(2, 2), [40, 40, 40, 255]);
        assert_close_rgba(pixel(10, 10), [40, 40, 40, 255]);
    }

    /// Posizione e anchor sono in pixel *di timeline*: l'anteprima compone
    /// a risoluzione ridotta (proxy), ma una clip spostata di mezzo frame
    /// resta spostata di mezzo frame.
    #[test]
    fn position_is_in_timeline_pixels_whatever_the_output_resolution() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(4, 4, 235, 128, 128, ColorMatrix::Bt709, false);
        let transform = Transform {
            position: [960.0, 0.0], // mezzo frame su una timeline 1920x1080
            ..Transform::default()
        };
        // Output a 1/120 della timeline: la clip deve comunque partire da
        // metà frame.
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

    /// Il crop è in pixel del media alla sua risoluzione nativa: su un
    /// proxy (frame decodificato più piccolo) taglia la stessa porzione.
    #[test]
    fn crop_is_in_native_source_pixels_even_on_a_proxy_frame() {
        let compositor = Compositor::new_headless();
        // Frame decodificato 4x4 per un media nativo 1920x1080.
        let input = quadrant_frame();
        let transform = Transform {
            crop: [0.0, 0.0, 960.0, 540.0], // via metà destra e metà bassa
            ..Transform::default()
        };
        let out = compositor.render_layers(
            &[Layer::Video {
                frame: input.as_yuv_frame(),
                transform,
                source_size: (1920, 1080),
                opacity: 1.0,
                filters: &[],
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

    /// L'asse Y è quello di un NLE, non quello delle uv: positivo = in alto.
    #[test]
    fn a_positive_y_position_lifts_the_clip() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            position: [0.0, 8.0], // mezzo frame in su (output 16x16)
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        // Alzata di mezzo frame: in alto resta la metà bassa della clip, in
        // basso non c'è più niente.
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

    /// La sfumatura agisce sull'alpha: sul bordo del crop il layer diventa
    /// via via trasparente invece di tagliare di netto. Negativa = verso
    /// l'interno del crop.
    #[test]
    fn negative_crop_softness_fades_inward_from_the_edge() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(16, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let transform = Transform {
            crop: [0.0, 0.0, 8.0, 0.0], // via la metà destra (8 px su 16)
            crop_softness: -1.6,
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, OutputFrame::exact(16, 16));
        let luma = |x: usize, y: usize| out[(y * 16 + x) * 4] as i32;

        // Clear nero sotto: più ci si avvicina al bordo del crop, più scuro.
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

    /// Sfumatura positiva: la rampa cade *oltre* il bordo di crop, quindi
    /// si vede solo dove qualcosa è stato tagliato.
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

    /// Il caso segnalato dall'utente: clip 9:16 in cima a una 16:9 in una
    /// timeline 16:9 — sulle bande laterali si deve vedere la clip sotto,
    /// non il nero.
    #[test]
    fn side_bars_of_the_top_layer_show_the_layer_below() {
        let compositor = Compositor::new_headless();
        // Bianco sotto (16:8 come l'output), nero sopra (8:16, stretto).
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
                },
                Layer::Video {
                    frame: above.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (above.width, above.height),
                    opacity: 1.0,
                    filters: &[],
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
                },
                Layer::Solid {
                    color: RED,
                    transform: Transform::default(),
                    opacity: 1.0,
                    filters: &[],
                },
            ],
            OutputFrame::exact(16, 16),
        );
        assert!(out.as_chunks::<4>().0.iter().all(|px| px == &[255, 0, 0, 255]));
    }

    /// Il filtro bianco e nero converte in luma qualunque tipo di layer,
    /// non solo il video.
    #[test]
    fn grayscale_flattens_a_solid_layer_to_its_luma() {
        let compositor = Compositor::new_headless();
        let out = compositor.render_layers(
            &[Layer::Solid {
                color: RED,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[vv_core::FilterKind::Grayscale],
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
                },
                Layer::Text {
                    title: &title,
                    transform: Transform::default(),
                    opacity: 1.0,
                    filters: &[],
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

    /// Comporre in un intermedio (la timeline annidata di una compound
    /// clip) e riusarlo come `Layer::Texture` deve dare gli stessi pixel
    /// che comporre i suoi layer direttamente: l'alpha-over è associativo,
    /// e il round-trip attraverso la texture non deve introdurre
    /// differenze.
    #[test]
    fn a_texture_layer_composites_like_the_layers_it_was_made_of() {
        let compositor = Compositor::new_headless();
        let output = OutputFrame::exact(16, 16);
        // Zoom < 1: attorno al rosso resta scoperto, cioè trasparente
        // nell'intermedio — è la parte che deve lasciar vedere il blu.
        let inner = Transform {
            zoom: [0.5, 0.5],
            ..Transform::default()
        };
        let below = || Layer::Solid {
            color: BLUE,
            transform: Transform::default(),
            opacity: 1.0,
            filters: &[],
        };
        let above = || Layer::Solid {
            color: RED,
            transform: inner,
            opacity: 1.0,
            filters: &[],
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

    /// Un intermedio è composto su sfondo trasparente con
    /// `ALPHA_BLENDING`, quindi il suo colore è già moltiplicato per
    /// l'alpha: riusarlo come layer senza dividerlo lo attenuerebbe una
    /// seconda volta (alpha al quadrato sui bordi e sulle dissolvenze).
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
                },
                Layer::Texture {
                    texture: &nested,
                    transform: Transform::default(),
                    source_size: (8, 8),
                    opacity: 1.0,
                    filters: &[],
                },
            ],
            output,
        );
        // Rosso al 50% sopra il bianco. Con la doppia moltiplicazione il
        // rosso scenderebbe a ~191.
        assert_close_rgba(out.as_chunks::<4>().0[0], [255, 128, 128, 255]);
    }

    /// `render_layers_to_owned_texture_transparent` non rimette la texture
    /// nel pool: un render successivo della stessa dimensione non deve
    /// disegnarci sopra, o il frame annidato conservato si corromperebbe.
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
            }],
            output,
        );
        compositor.render_layers(
            &[Layer::Solid {
                color: BLUE,
                transform: Transform::default(),
                opacity: 1.0,
                filters: &[],
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
        // Crop in pixel di timeline: metà destra tagliata, poi spostata
        // di un quarto a destra.
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
            }],
            OutputFrame::scaled(8, 4, (16, 8)),
        );
        let px = |x: usize, y: usize| &out[(y * 8 + x) * 4..(y * 8 + x) * 4 + 4];
        assert_eq!(px(1, 2), &[0, 0, 0, 255], "a sinistra resta scoperto");
        assert_eq!(px(3, 2), &[255, 0, 0, 255]);
        assert_eq!(px(6, 2), &[0, 0, 0, 255], "oltre il crop");
    }

    /// L'opacità del layer (dissolvenze di clip) attenua l'alpha con cui si
    /// compone sul layer sotto, sia per il video sia per un colore pieno.
    #[test]
    fn layer_opacity_blends_with_what_is_below() {
        let compositor = Compositor::new_headless();
        let below = solid_frame(4, 4, 235, 128, 128, ColorMatrix::Bt709, false); // bianco
        let out = compositor.render_layers(
            &[
                Layer::Video {
                    frame: below.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (below.width, below.height),
                    opacity: 1.0,
                    filters: &[],
                },
                Layer::Solid {
                    color: RED,
                    transform: Transform::default(),
                    opacity: 0.5,
                    filters: &[],
                },
            ],
            OutputFrame::exact(4, 4),
        );
        let px = |out: &[u8], x: usize, y: usize| -> [u8; 4] {
            out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].try_into().unwrap()
        };
        assert_close_rgba(px(&out, 2, 2), [255, 127, 127, 255]); // 50% rosso su bianco = rosa

        // Opacità 0: il layer sopra non si vede affatto.
        let out = compositor.render_layers(
            &[
                Layer::Video {
                    frame: below.as_yuv_frame(),
                    transform: Transform::default(),
                    source_size: (below.width, below.height),
                    opacity: 1.0,
                    filters: &[],
                },
                Layer::Solid { color: RED, transform: Transform::default(), opacity: 0.0, filters: &[] },
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
            }],
            OutputFrame::exact(5, 3),
        );
        // Croma 3x2; 15 + 6 + 6 = 27 byte, non multiplo di 4.
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

    /// `render_layers_rgba_transparent` è quel che compone la timeline
    /// annidata di una compound clip: le zone senza nulla sopra devono
    /// restare trasparenti (alpha 0), non nere come per il video finale —
    /// altrimenti coprirebbero quel che c'è sotto quando la compound clip
    /// diventa a sua volta un layer altrove.
    #[test]
    fn no_layers_renders_fully_transparent_with_the_transparent_variant() {
        let compositor = Compositor::new_headless();
        let out = compositor.render_layers_rgba_transparent(&[], OutputFrame::exact(4, 4));
        assert!(out.as_chunks::<4>().0.iter().all(|px| px == &[0, 0, 0, 0]));
    }

    #[test]
    fn render_layers_rgba_transparent_leaves_uncovered_areas_transparent_not_black() {
        let compositor = Compositor::new_headless();
        // Crop in pixel di timeline: metà destra tagliata via.
        let out = compositor.render_layers_rgba_transparent(
            &[Layer::Solid {
                color: RED,
                transform: Transform {
                    crop: [0.0, 0.0, 8.0, 0.0],
                    ..Transform::default()
                },
                opacity: 1.0,
                filters: &[],
            }],
            OutputFrame::exact(16, 8),
        );
        let px = |x: usize, y: usize| &out[(y * 16 + x) * 4..(y * 16 + x) * 4 + 4];
        assert_eq!(px(2, 4), &[255, 0, 0, 255], "coperto dal layer: rosso opaco");
        assert_eq!(px(12, 4), &[0, 0, 0, 0], "scoperto: trasparente, non nero");
    }

    /// Il meccanismo con cui una compound clip già composta ritorna un
    /// layer altrove: `YuvFrame::alpha` porta la vera copertura per pixel,
    /// non solo il moltiplicatore uniforme `opacity` — dove vale 0 deve
    /// lasciar vedere quel che c'è sotto, esattamente come farebbe un
    /// buco della timeline annidata da cui viene.
    #[test]
    fn a_videos_own_alpha_plane_lets_the_layer_below_show_through() {
        let compositor = Compositor::new_headless();
        let frame = solid_frame(4, 4, 255, 128, 128, ColorMatrix::Bt709, true); // bianco
        // Sinistra opaca, destra trasparente.
        let alpha: Vec<u8> = (0..16u32).map(|i| if i % 4 < 2 { 255 } else { 0 }).collect();
        let out = compositor.render_layers(
            &[Layer::Video {
                frame: YuvFrame { alpha: &alpha, ..frame.as_yuv_frame() },
                transform: Transform::default(),
                source_size: (4, 4),
                opacity: 1.0,
                filters: &[],
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

    /// Legge indietro una texture creata da questo stesso `Compositor`
    /// (stesso device/queue) — non fa parte dell'API pubblica, serve solo
    /// a verificare nel test che il path zero-copy produca esattamente
    /// gli stessi byte del path con readback: un cambio di sola
    /// performance non deve alterare un solo pixel di quel che viene
    /// mostrato (REFACTOR_PIPELINE.md §5, accuratezza del frame non
    /// negoziabile).
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
}
