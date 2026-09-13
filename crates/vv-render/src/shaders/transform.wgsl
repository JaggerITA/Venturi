// Crop + zoom + position su un frame video (milestone 5). Un solo triangolo
// fullscreen (nessun vertex buffer) campiona la texture sorgente con le
// coordinate rimappate secondo il Transform.

struct TransformUniform {
    // left, top, right, bottom in coordinate normalizzate [0,1] sul source.
    crop: vec4<f32>,
    // zoom, position.x, position.y, (inutilizzato, per l'allineamento a 16 byte)
    zoom_pos: vec4<f32>,
};

@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> transform: TransformUniform;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // Trucco del "triangolo fullscreen": 3 vertici che coprono l'intero
    // viewport senza bisogno di un vertex buffer.
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);

    var out: VertexOutput;
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let crop_min = transform.crop.xy;
    let crop_max = transform.crop.zw;
    let crop_size = crop_max - crop_min;
    let crop_center = (crop_min + crop_max) * 0.5;

    let zoom = max(transform.zoom_pos.x, 0.0001);
    let position = transform.zoom_pos.yz;

    let centered = in.uv - vec2<f32>(0.5, 0.5);
    let sample_uv = crop_center + (centered / zoom) * crop_size - position;

    // Fuori dal crop: clamp sul bordo piuttosto che leggere fuori texture.
    let clamped = clamp(sample_uv, vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0));
    return textureSample(input_tex, input_sampler, clamped);
}
