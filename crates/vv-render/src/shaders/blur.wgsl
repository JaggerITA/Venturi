// One axis of a separable blur, on a premultiplied RGBA texture.

struct BlurUniform {
    // xy: uv step between two taps, z: taps per side, w: 0 box, 1 gaussian.
    step_taps: vec4<f32>,
    // x: radius in taps (fractional), y: gaussian sigma in taps.
    shape: vec4<f32>,
};

@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var input_sampler: sampler;
@group(0) @binding(2) var<uniform> blur: BlurUniform;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);

    var out: VertexOutput;
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

fn weight(d: f32) -> f32 {
    if (blur.step_taps.w > 0.5) {
        let sigma = blur.shape.y;
        return exp(-(d * d) / (2.0 * sigma * sigma));
    }
    // The last tap takes the fractional part of the radius, so that the
    // slider changes the result continuously.
    return clamp(blur.shape.x - d + 1.0, 0.0, 1.0);
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let taps = i32(blur.step_taps.z);
    var sum = vec4<f32>(0.0);
    var total = 0.0;
    for (var i = -taps; i <= taps; i = i + 1) {
        let d = f32(i);
        let w = weight(abs(d));
        sum = sum + w * textureSampleLevel(input_tex, input_sampler, in.uv + blur.step_taps.xy * d, 0.0);
        total = total + w;
    }
    return sum / max(total, 1e-6);
}
