// Crop + zoom + position su un frame video (milestone 5), più la
// conversione YUV420->RGB (REFACTOR_PIPELINE.md B3): il frame arriva in
// ingresso come tre piani (Y/U/V) invece di RGBA già espanso, la
// conversione avviene qui nello shader invece che su CPU in vv-media.
// Un solo triangolo fullscreen (nessun vertex buffer) campiona le tre
// texture sorgente con le coordinate rimappate secondo il Transform.

struct TransformUniform {
    // left, top, right, bottom in coordinate normalizzate [0,1] sul source.
    crop: vec4<f32>,
    // zoom, position.x, position.y, (inutilizzato, per l'allineamento a 16 byte)
    zoom_pos: vec4<f32>,
    // x: matrice colore (0=BT.601, 1=BT.709, 2=BT.2020, vedi
    //    vv_media::ColorMatrix). y: 1.0 se range full (JPEG), 0.0 se
    //    limited (MPEG) — vedi vv_media::FrameYuv420::full_range. z/w
    //    inutilizzati, per l'allineamento a 16 byte.
    color: vec4<f32>,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var input_sampler: sampler;
@group(0) @binding(4) var<uniform> transform: TransformUniform;

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

// Coefficienti Kr/Kb per la matrice richiesta (REFACTOR_PIPELINE.md B3,
// vedi doc di vv_media::ColorMatrix per la scelta di quale matrice usare
// caso per caso — qui solo l'applicazione).
fn kr_kb(matrix_id: i32) -> vec2<f32> {
    if (matrix_id == 1) {
        return vec2<f32>(0.2126, 0.0722); // BT.709
    } else if (matrix_id == 2) {
        return vec2<f32>(0.2627, 0.0593); // BT.2020
    }
    return vec2<f32>(0.299, 0.114); // BT.601 (anche il fallback per matrici non gestite)
}

fn yuv_to_rgb(y_sample: f32, u_sample: f32, v_sample: f32, matrix_id: i32, full_range: bool) -> vec3<f32> {
    var y_n: f32;
    var u_n: f32;
    var v_n: f32;
    if (full_range) {
        y_n = y_sample;
        u_n = u_sample - 0.5;
        v_n = v_sample - 0.5;
    } else {
        // Limited/MPEG: codici 16-235 (luma) / 16-240 (croma) su 8 bit,
        // già normalizzati [0,1] dal sampler (16/255..235/255 ecc.) —
        // riespande all'intervallo pieno prima di applicare la matrice.
        y_n = (y_sample - 16.0 / 255.0) * (255.0 / 219.0);
        u_n = (u_sample - 128.0 / 255.0) * (255.0 / 224.0);
        v_n = (v_sample - 128.0 / 255.0) * (255.0 / 224.0);
    }

    let kkb = kr_kb(matrix_id);
    let kr = kkb.x;
    let kb = kkb.y;
    let kg = 1.0 - kr - kb;

    let r = y_n + 2.0 * (1.0 - kr) * v_n;
    let b = y_n + 2.0 * (1.0 - kb) * u_n;
    let g = y_n - (2.0 * kr * (1.0 - kr) / kg) * v_n - (2.0 * kb * (1.0 - kb) / kg) * u_n;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
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

    // U/V sono a metà risoluzione (4:2:0): campionarli alla stessa uv
    // del piano Y con un sampler bilineare fa anche l'upsampling della
    // croma, gratis.
    let y_sample = textureSample(y_tex, input_sampler, clamped).r;
    let u_sample = textureSample(u_tex, input_sampler, clamped).r;
    let v_sample = textureSample(v_tex, input_sampler, clamped).r;

    let matrix_id = i32(transform.color.x);
    let full_range = transform.color.y > 0.5;
    let rgb = yuv_to_rgb(y_sample, u_sample, v_sample, matrix_id, full_range);
    return vec4<f32>(rgb, 1.0);
}
