//! Pipeline di compositing per-frame (vedi ARCHITECTURE.md § Compositing
//! GPU): input in YUV420 planare ([`YuvFrame`], REFACTOR_PIPELINE.md B3
//! — la conversione a RGB avviene qui, nello shader, non più su CPU in
//! vv-media), crop + zoom via shader wgpu.
//!
//! Lo stack di track video viene composto in alpha-over, un pass per
//! layer ([`Compositor::render_layers`]/[`Layer`]): dove il layer sopra
//! non copre il frame di output — aspect ratio diverso da quello della
//! timeline, vedi `fit_factors` — si vede quello sotto.
//!
//! Due modi di ottenere il risultato, che condividono gli stessi pass:
//! - [`Compositor::render_layers`] fa un round-trip CPU→GPU→CPU (upload,
//!   render, readback) — usato dall'export, che ha bisogno di byte RGBA
//!   densi da passare all'encoder (`vv_media::Encoder::write_video_frame`),
//!   non di una texture GPU.
//! - [`Compositor::render_layers_to_texture`] resta sulla GPU, nessun
//!   readback: usato dall'anteprima (`vv-app::main`), che registra la
//!   texture direttamente in `egui-wgpu` (`Renderer::register_native_texture`
//!   / `update_egui_texture_from_wgpu_texture`) ed evita sia il readback
//!   sia il re-upload che `egui::ColorImage` avrebbe comunque richiesto —
//!   il round-trip che questo secondo metodo elimina (REFACTOR_PIPELINE.md
//!   B2). Richiede che il `Compositor` sia stato costruito con
//!   [`Compositor::new`] condividendo il device/queue di `egui-wgpu`
//!   ([`Compositor::new_headless`] resta per l'export e per i test, che
//!   non hanno bisogno di mostrare nulla in una finestra egui): una
//!   texture creata su un device diverso da quello del renderer egui non
//!   può essere condivisa con lui.

use std::sync::Arc;
use vv_core::Transform;
use wgpu::util::DeviceExt;

const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const BLACK: wgpu::Color = wgpu::Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};
/// Formato dei tre piani di input (Y/U/V): un solo canale 8 bit, letto
/// come `.r` nello shader.
const PLANE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Matrice di conversione YUV→RGB (REFACTOR_PIPELINE.md B3) — stessi tre
/// casi di `vv_media::ColorMatrix`, ridefinita qui invece di dipendere
/// da vv-media: vv-render non sa cos'è un media decodificato, prende
/// solo piani di byte grezzi (stessa convenzione già in uso per il
/// resto di questo modulo — vedi `YuvFrame`, non un
/// `vv_media::FrameYuv420`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl ColorMatrix {
    /// Selettore passato allo shader (`transform.wgsl`, funzione
    /// `kr_kb`): deve restare sincronizzato con quella funzione.
    fn shader_id(self) -> f32 {
        match self {
            Self::Bt601 => 0.0,
            Self::Bt709 => 1.0,
            Self::Bt2020 => 2.0,
        }
    }
}

/// Un frame video decodificato in YUV420 planare 8 bit, con i metadati
/// colore necessari a convertirlo in RGB correttamente — l'input di
/// [`Compositor::render_frame`]/[`Compositor::render_frame_to_texture`].
/// I piani devono essere densi (nessun padding di riga, vedi
/// `vv_media::FrameYuv420` per come vengono prodotti dal decoder).
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
}

impl<'a> YuvFrame<'a> {
    fn borrowed(&self) -> YuvFrame<'a> {
        YuvFrame { ..*self }
    }
}

/// Un livello dello stack di compositing, dal basso verso l'alto (vedi
/// [`Compositor::render_layers`]): una track video con una clip Media
/// (`Video`) o con una clip generatore a colore pieno (`Solid`, che
/// copre il frame intero e quindi occlude tutto ciò che sta sotto).
pub enum Layer<'a> {
    Video {
        frame: YuvFrame<'a>,
        transform: Transform,
    },
    Solid(vv_core::Rgba),
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct TransformUniform {
    crop: [f32; 4],
    zoom_pos: [f32; 4],
    fit_rot: [f32; 4],
    anchor_flip: [f32; 4],
    color: [f32; 4],
}

impl TransformUniform {
    fn new(
        t: &Transform,
        matrix: ColorMatrix,
        full_range: bool,
        fit: [f32; 2],
        output_aspect: f32,
    ) -> Self {
        Self {
            crop: t.crop,
            zoom_pos: [t.zoom[0], t.zoom[1], t.position[0], t.position[1]],
            fit_rot: [
                fit[0],
                fit[1],
                t.rotation.to_radians(),
                t.crop_softness.max(0.0),
            ],
            anchor_flip: [
                t.anchor[0],
                t.anchor[1],
                if t.flip[0] { 1.0 } else { 0.0 },
                if t.flip[1] { 1.0 } else { 0.0 },
            ],
            color: [
                matrix.shader_id(),
                if full_range { 1.0 } else { 0.0 },
                output_aspect,
                0.0,
            ],
        }
    }
}

pub struct Compositor {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
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
                    // Alpha blending, non REPLACE: le zone scoperte di un
                    // layer (letterbox, vedi `fit_factors`) escono dallo
                    // shader con alpha 0 e devono lasciar vedere il layer
                    // sotto, non coprirlo di nero.
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

        Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler,
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

    /// Applica `transform` a un frame YUV420 e restituisce il risultato
    /// come RGBA8 denso (`output_w * output_h * 4` byte), ridimensionato a
    /// `output_w x output_h`. Fa un readback GPU→CPU: per il path
    /// zero-copy verso l'anteprima egui vedi
    /// [`Compositor::render_frame_to_texture`].
    pub fn render_frame(
        &self,
        frame: &YuvFrame,
        transform: &Transform,
        output_w: u32,
        output_h: u32,
    ) -> Vec<u8> {
        self.render_layers(
            &[Layer::Video {
                frame: frame.borrowed(),
                transform: *transform,
            }],
            output_w,
            output_h,
        )
    }

    /// Come [`Compositor::render_frame`] ma per uno stack di layer
    /// (track video dal basso verso l'alto): ogni layer viene composto
    /// sopra il precedente in alpha-over, così le zone scoperte di quello
    /// in cima (bande di letterbox, vedi `fit_factors`) mostrano quello
    /// sotto invece del nero.
    pub fn render_layers(&self, layers: &[Layer], output_w: u32, output_h: u32) -> Vec<u8> {
        let output_texture = self.render_layers_to_texture(layers, output_w, output_h);

        // wgpu richiede che ogni riga del buffer di destinazione sia
        // allineata a COPY_BYTES_PER_ROW_ALIGNMENT: il buffer può quindi
        // avere padding a fine riga che va rimosso in fase di lettura.
        let unpadded_bytes_per_row = output_w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;

        let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vv-render readback buffer"),
            size: (padded_bytes_per_row * output_h) as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vv-render readback encoder"),
            });
        encoder.copy_texture_to_buffer(
            output_texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(output_h),
                },
            },
            wgpu::Extent3d {
                width: output_w,
                height: output_h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = output_buffer.slice(..);
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

        let mut out = Vec::with_capacity((unpadded_bytes_per_row * output_h) as usize);
        {
            let data = slice.get_mapped_range().expect("get_mapped_range fallita");
            for row in 0..output_h {
                let start = (row * padded_bytes_per_row) as usize;
                let end = start + unpadded_bytes_per_row as usize;
                out.extend_from_slice(&data[start..end]);
            }
        }
        output_buffer.unmap();

        out
    }

    /// Come [`Compositor::render_frame`], ma resta sulla GPU: nessun
    /// readback, restituisce la texture di output direttamente (già
    /// utilizzabile da `egui_wgpu::Renderer::register_native_texture` /
    /// `update_egui_texture_from_wgpu_texture`, dato che il `Compositor`
    /// condivide il device con `egui-wgpu` — vedi doc di modulo). La
    /// chiamata a `self.queue.submit` dentro `render_layers_to_texture` è
    /// sufficiente: non serve attendere il completamento, il pass che
    /// campiona questa texture (quello di egui) verrà sottomesso *dopo*
    /// sulla stessa coda, quindi la GPU la esegue comunque in ordine.
    pub fn render_frame_to_texture(
        &self,
        frame: &YuvFrame,
        transform: &Transform,
        output_w: u32,
        output_h: u32,
    ) -> wgpu::Texture {
        self.render_layers_to_texture(
            &[Layer::Video {
                frame: frame.borrowed(),
                transform: *transform,
            }],
            output_w,
            output_h,
        )
    }

    /// Versione multi-layer di [`Compositor::render_frame_to_texture`]
    /// (vedi [`Compositor::render_layers`]).
    pub fn render_layers_to_texture(
        &self,
        layers: &[Layer],
        output_w: u32,
        output_h: u32,
    ) -> wgpu::Texture {
        let output_texture = self.output_texture(output_w, output_h);
        let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vv-render transform encoder"),
            });

        // Nessun layer: resta il solo clear, cioè un frame nero.
        if layers.is_empty() {
            self.pass(&mut encoder, &output_view, wgpu::LoadOp::Clear(BLACK), None);
        }
        for (i, layer) in layers.iter().enumerate() {
            match layer {
                // Un colore pieno copre tutto il frame: è il clear del
                // pass, nessun draw (e nessun layer sotto sopravvive,
                // giustamente — è opaco).
                Layer::Solid(color) => {
                    let clear = wgpu::Color {
                        r: color.r.clamp(0.0, 1.0) as f64,
                        g: color.g.clamp(0.0, 1.0) as f64,
                        b: color.b.clamp(0.0, 1.0) as f64,
                        a: color.a.clamp(0.0, 1.0) as f64,
                    };
                    self.pass(&mut encoder, &output_view, wgpu::LoadOp::Clear(clear), None);
                }
                Layer::Video { frame, transform } => {
                    let load = if i == 0 {
                        wgpu::LoadOp::Clear(BLACK)
                    } else {
                        wgpu::LoadOp::Load
                    };
                    let bind_group = self.layer_bind_group(frame, transform, output_w, output_h);
                    self.pass(&mut encoder, &output_view, load, Some(&bind_group));
                }
            }
        }

        self.queue.submit(Some(encoder.finish()));
        output_texture
    }

    /// Upload dei tre piani del layer e bind group pronto per il pass:
    /// crop/zoom, letterbox e conversione YUV→RGB stanno tutti nello
    /// shader, qui si preparano solo i suoi input.
    fn layer_bind_group(
        &self,
        frame: &YuvFrame,
        transform: &Transform,
        output_w: u32,
        output_h: u32,
    ) -> wgpu::BindGroup {
        let plane_texture = |label: &str, data: &[u8], w: u32, h: u32| {
            self.device.create_texture_with_data(
                &self.queue,
                &wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: PLANE_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                data,
            )
        };
        let y_texture = plane_texture("vv-render Y plane", frame.y, frame.width, frame.height);
        let u_texture = plane_texture(
            "vv-render U plane",
            frame.u,
            frame.chroma_width,
            frame.chroma_height,
        );
        let v_texture = plane_texture(
            "vv-render V plane",
            frame.v,
            frame.chroma_width,
            frame.chroma_height,
        );
        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let u_view = u_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let v_view = v_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uniform = TransformUniform::new(
            transform,
            frame.matrix,
            frame.full_range,
            fit_factors(
                (frame.width as f32, frame.height as f32),
                (output_w as f32, output_h as f32),
            ),
            output_w as f32 / output_h.max(1) as f32,
        );
        let uniform_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vv-render transform uniform"),
                contents: bytemuck::bytes_of(&uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vv-render transform bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&u_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&v_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        })
    }

    fn output_texture(&self, output_w: u32, output_h: u32) -> wgpu::Texture {
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
            // TEXTURE_BINDING oltre a RENDER_ATTACHMENT/COPY_SRC: serve al
            // path zero-copy (`render_frame_to_texture`), che registra
            // questa stessa texture come input campionabile dal renderer
            // di egui-wgpu — senza, wgpu rifiuterebbe la bind group creata
            // da `register_native_texture`. Nessun costo per il path con
            // readback (`render_frame`), che non la usa.
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

/// Dimensioni di output con l'aspect ratio di `aspect` che contengono
/// `source` alla sua risoluzione nativa: il contenuto non viene né
/// scalato né deformato, si aggiungono solo le bande. Usata
/// dall'anteprima, che compone alla risoluzione del frame decodificato
/// (proxy compreso) e non a quella della timeline.
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
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), 8, 8);

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
            let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), 2, 2);
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
            crop: [0.0, 0.0, 0.5, 0.5], // solo il quadrante alto-sinistra
            zoom: [1.0, 1.0],
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
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
            crop: [0.5, 0.5, 1.0, 1.0],
            zoom: [1.0, 1.0],
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
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
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), 32, 16);

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
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), 16, 32);

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

        let bars = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), 32, 16);
        assert_eq!(&bars[0..4], &[0, 0, 0, 255], "a zoom 1 restano le bande");

        let transform = Transform {
            crop: [0.0, 0.0, 1.0, 1.0],
            zoom: [5.0, 5.0], // > 32/16 : 8/16, cioè il fattore che copre la larghezza
            position: [0.0, 0.0],
            ..Transform::default()
        };
        let zoomed = compositor.render_frame(&input.as_yuv_frame(), &transform, 32, 16);
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
            crop: [0.0, 0.0, 1.0, 1.0],
            zoom: [1.0, 1.0],
            position: [0.5, 0.0], // mezzo frame a destra
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
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
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
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
            anchor: [-0.5, -0.5],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
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

    #[test]
    fn flip_mirrors_the_clip_on_each_axis() {
        let compositor = Compositor::new_headless();
        let input = quadrant_frame();
        let transform = Transform {
            flip: [true, false],
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
        let pixel = |x: usize, y: usize| {
            let i = (y * 16 + x) * 4;
            [out[i], out[i + 1], out[i + 2], out[i + 3]]
        };

        assert_close_rgba(pixel(12, 4), [40, 40, 40, 255]);
        assert_close_rgba(pixel(4, 4), [100, 100, 100, 255]);
    }

    /// La sfumatura agisce sull'alpha: sul bordo del crop il layer diventa
    /// via via trasparente invece di tagliare di netto.
    #[test]
    fn crop_softness_fades_the_edge_instead_of_cutting_it() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(16, 16, 235, 128, 128, ColorMatrix::Bt709, false);
        let transform = Transform {
            crop: [0.0, 0.0, 0.5, 1.0],
            crop_softness: 0.1,
            ..Transform::default()
        };
        let out = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
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
                },
                Layer::Video {
                    frame: above.as_yuv_frame(),
                    transform: Transform::default(),
                },
            ],
            32,
            16,
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
                },
                Layer::Solid(vv_core::Rgba {
                    r: 1.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                }),
            ],
            16,
            16,
        );
        assert!(out.as_chunks::<4>().0.iter().all(|px| px == &[255, 0, 0, 255]));
    }

    #[test]
    fn no_layers_renders_a_black_frame() {
        let compositor = Compositor::new_headless();
        let out = compositor.render_layers(&[], 4, 4);
        assert!(out.as_chunks::<4>().0.iter().all(|px| px == &[0, 0, 0, 255]));
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
        let out = compositor.render_frame(&input.as_yuv_frame(), &Transform::default(), 37, 21);
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

        let via_readback = compositor.render_frame(&input.as_yuv_frame(), &transform, 16, 16);
        let texture = compositor.render_frame_to_texture(&input.as_yuv_frame(), &transform, 16, 16);
        let via_texture = read_back(&compositor, &texture, 16, 16);

        assert_eq!(
            via_readback, via_texture,
            "il path zero-copy deve produrre esattamente gli stessi pixel del path con readback"
        );
    }
}
