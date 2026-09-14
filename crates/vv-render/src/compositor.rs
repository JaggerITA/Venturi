//! Pipeline di compositing per-frame (vedi ARCHITECTURE.md § Compositing
//! GPU): crop + zoom via shader wgpu, invece di manipolare i pixel su CPU.
//!
//! Due modi di ottenere il risultato, che condividono lo stesso pass
//! (`render_to_texture`, privato):
//! - [`Compositor::render_frame`] fa un round-trip CPU→GPU→CPU (upload,
//!   render, readback) — usato dall'export, che ha bisogno di byte RGBA
//!   densi da passare all'encoder (`vv_media::Encoder::write_video_frame`),
//!   non di una texture GPU.
//! - [`Compositor::render_frame_to_texture`] resta sulla GPU, nessun
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

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct TransformUniform {
    crop: [f32; 4],
    zoom_pos: [f32; 4],
}

impl From<&Transform> for TransformUniform {
    fn from(t: &Transform) -> Self {
        Self {
            crop: t.crop,
            zoom_pos: [t.zoom, t.position[0], t.position[1], 0.0],
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

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vv-render transform bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
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
                    blend: Some(wgpu::BlendState::REPLACE),
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

    /// Applica `transform` a un frame RGBA8 e restituisce il risultato
    /// come RGBA8 denso (`output_w * output_h * 4` byte), ridimensionato a
    /// `output_w x output_h`. Fa un readback GPU→CPU: per il path
    /// zero-copy verso l'anteprima egui vedi
    /// [`Compositor::render_frame_to_texture`].
    pub fn render_frame(
        &self,
        input_rgba: &[u8],
        input_w: u32,
        input_h: u32,
        transform: &Transform,
        output_w: u32,
        output_h: u32,
    ) -> Vec<u8> {
        let output_texture =
            self.render_to_texture(input_rgba, input_w, input_h, transform, output_w, output_h);

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
    /// chiamata a `self.queue.submit` dentro `render_to_texture` è
    /// sufficiente: non serve attendere il completamento, il pass che
    /// campiona questa texture (quello di egui) verrà sottomesso *dopo*
    /// sulla stessa coda, quindi la GPU la esegue comunque in ordine.
    pub fn render_frame_to_texture(
        &self,
        input_rgba: &[u8],
        input_w: u32,
        input_h: u32,
        transform: &Transform,
        output_w: u32,
        output_h: u32,
    ) -> wgpu::Texture {
        self.render_to_texture(input_rgba, input_w, input_h, transform, output_w, output_h)
    }

    /// Il pass condiviso da `render_frame` e `render_frame_to_texture`:
    /// upload del frame in ingresso, crop/zoom via shader, draw nella
    /// texture di output — tutto ciò che precede la scelta "leggi
    /// indietro su CPU o lascia sulla GPU", che sta ai due metodi
    /// pubblici sopra.
    fn render_to_texture(
        &self,
        input_rgba: &[u8],
        input_w: u32,
        input_h: u32,
        transform: &Transform,
        output_w: u32,
        output_h: u32,
    ) -> wgpu::Texture {
        let input_texture = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some("vv-render input frame"),
                size: wgpu::Extent3d {
                    width: input_w,
                    height: input_h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: OUTPUT_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            input_rgba,
        );
        let input_view = input_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uniform = TransformUniform::from(transform);
        let uniform_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("vv-render transform uniform"),
                contents: bytemuck::bytes_of(&uniform),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vv-render transform bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&input_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        });

        let output_texture = self.device.create_texture(&wgpu::TextureDescriptor {
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
        });
        let output_view = output_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vv-render transform encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vv-render transform pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        output_texture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            data.extend_from_slice(&rgba);
        }
        data
    }

    /// Frame 4x4 con quattro quadranti di colore diverso: utile per
    /// verificare *dove* il crop va a pescare, non solo che il colore medio
    /// torni giusto.
    fn quadrant_frame() -> (u32, u32, Vec<u8>) {
        let w = 4;
        let h = 4;
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let color = match (x < w / 2, y < h / 2) {
                    (true, true) => [255, 0, 0, 255],     // alto-sinistra: rosso
                    (false, true) => [0, 255, 0, 255],    // alto-destra: verde
                    (true, false) => [0, 0, 255, 255],    // basso-sinistra: blu
                    (false, false) => [255, 255, 0, 255], // basso-destra: giallo
                };
                let idx = ((y * w + x) * 4) as usize;
                data[idx..idx + 4].copy_from_slice(&color);
            }
        }
        (w, h, data)
    }

    fn center_pixel(rgba: &[u8], width: u32, height: u32) -> [u8; 4] {
        let x = width / 2;
        let y = height / 2;
        let idx = ((y * width + x) * 4) as usize;
        rgba[idx..idx + 4].try_into().unwrap()
    }

    #[test]
    fn identity_transform_passes_through_solid_color() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(8, 8, [10, 20, 30, 255]);
        let out = compositor.render_frame(&input, 8, 8, &Transform::default(), 8, 8);

        assert_eq!(out.len(), 8 * 8 * 4);
        for px in out.as_chunks::<4>().0 {
            assert_eq!(px, &[10, 20, 30, 255]);
        }
    }

    #[test]
    fn crop_to_top_left_quadrant_shows_only_that_color() {
        let compositor = Compositor::new_headless();
        let (w, h, input) = quadrant_frame();

        let transform = Transform {
            crop: [0.0, 0.0, 0.5, 0.5], // solo il quadrante alto-sinistra (rosso)
            zoom: 1.0,
            position: [0.0, 0.0],
        };
        let out = compositor.render_frame(&input, w, h, &transform, 16, 16);

        // Solo il pixel centrale, non tutta l'immagine: ai bordi del crop il
        // filtro bilineare sfuma legittimamente coi texel vicini (specie
        // all'angolo dove convergono tutti e 4 i quadranti) — non è un bug
        // della matematica di crop, è come ci si aspetta si comporti un
        // sampler lineare. Il centro del crop invece cade tra due texel
        // dello stesso colore, quindi deve restare puro.
        assert_eq!(center_pixel(&out, 16, 16), [255, 0, 0, 255]);
    }

    #[test]
    fn crop_to_bottom_right_quadrant_shows_only_that_color() {
        let compositor = Compositor::new_headless();
        let (w, h, input) = quadrant_frame();

        let transform = Transform {
            crop: [0.5, 0.5, 1.0, 1.0], // quadrante basso-destra (giallo)
            zoom: 1.0,
            position: [0.0, 0.0],
        };
        let out = compositor.render_frame(&input, w, h, &transform, 16, 16);

        assert_eq!(center_pixel(&out, 16, 16), [255, 255, 0, 255]);
    }

    #[test]
    fn output_size_can_differ_from_input_size() {
        let compositor = Compositor::new_headless();
        let input = solid_frame(4, 4, [1, 2, 3, 255]);
        let out = compositor.render_frame(&input, 4, 4, &Transform::default(), 37, 21);
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
        let (w, h, input) = quadrant_frame();
        let transform = Transform {
            crop: [0.0, 0.0, 0.5, 0.5],
            zoom: 1.0,
            position: [0.0, 0.0],
        };

        let via_readback = compositor.render_frame(&input, w, h, &transform, 16, 16);
        let texture = compositor.render_frame_to_texture(&input, w, h, &transform, 16, 16);
        let via_texture = read_back(&compositor, &texture, 16, 16);

        assert_eq!(
            via_readback, via_texture,
            "il path zero-copy deve produrre esattamente gli stessi pixel del path con readback"
        );
    }
}
