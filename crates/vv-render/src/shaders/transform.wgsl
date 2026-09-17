// Crop + zoom + position su un frame video (milestone 5), più la
// conversione YUV420->RGB (REFACTOR_PIPELINE.md B3): il frame arriva in
// ingresso come tre piani (Y/U/V) invece di RGBA già espanso, la
// conversione avviene qui nello shader invece che su CPU in vv-media.
// Un solo triangolo fullscreen (nessun vertex buffer) campiona le tre
// texture sorgente con le coordinate rimappate secondo il Transform.

struct TransformUniform {
    // left, top, right, bottom in coordinate normalizzate [0,1] sul source.
    crop: vec4<f32>,
    // zoom.x, zoom.y, position.x, position.y
    zoom_pos: vec4<f32>,
    // x/y: fattori di letterbox (>1 sull'asse che resta scoperto): il
    // frame sorgente viene inscritto nell'output mantenendo il suo
    // aspect ratio invece di essere deformato. z: rotazione in radianti
    // (positiva = oraria). w: sfumatura dei bordi di crop, in frazioni
    // del frame sorgente.
    fit_rot: vec4<f32>,
    // anchor.x, anchor.y (pivot di zoom e rotazione, in frazioni del
    // frame di output dal centro della clip), flip.x, flip.y (0 o 1).
    anchor_flip: vec4<f32>,
    // x: matrice colore (0=BT.601, 1=BT.709, 2=BT.2020, vedi
    //    vv_media::ColorMatrix). y: 1.0 se range full (JPEG), 0.0 se
    //    limited (MPEG) — vedi vv_media::FrameYuv420::full_range.
    //    z: aspect ratio dell'output (w/h), serve a far ruotare senza
    //    deformare. w inutilizzato, per l'allineamento a 16 byte.
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
    let zoom = max(abs(transform.zoom_pos.xy), vec2<f32>(0.0001, 0.0001));
    let position = transform.zoom_pos.zw;
    let anchor = transform.anchor_flip.xy;
    let angle = transform.fit_rot.z;
    let aspect = max(transform.color.z, 0.0001);

    // Dallo spazio dell'output a quello del sorgente, l'inverso di come si
    // ragiona sulla clip: `position` la sposta dentro il frame, rotazione e
    // `zoom` la girano e la ingrandiscono attorno all'anchor point (quindi
    // lo zoom può arrivare a coprire tutto il frame, anche partendo da un
    // aspect ratio diverso), `fit` la inscrive senza deformarla (vedi
    // Compositor::fit_factors).
    var q = in.uv - vec2<f32>(0.5, 0.5) - position - anchor;
    // La rotazione va fatta in uno spazio isotropo, altrimenti un frame non
    // quadrato la trasformerebbe in una deformazione a taglio.
    q = vec2<f32>(q.x * aspect, q.y);
    let cs = cos(angle);
    let sn = sin(angle);
    q = vec2<f32>(q.x * cs + q.y * sn, -q.x * sn + q.y * cs);
    q = vec2<f32>(q.x / aspect, q.y);
    q = q / zoom + anchor;

    let flip = vec2<f32>(
        select(1.0, -1.0, transform.anchor_flip.z > 0.5),
        select(1.0, -1.0, transform.anchor_flip.w > 0.5),
    );
    let source_uv = q * flip * transform.fit_rot.xy + vec2<f32>(0.5, 0.5);

    let crop_min = transform.crop.xy;
    let crop_max = transform.crop.zw;
    // Fuori dal crop (o fuori dalla clip): trasparente, si vede il layer
    // sotto. Il crop taglia e basta — non ricentra né ridimensiona quel
    // che resta, che continua a cadere dov'era nel frame.
    if (any(source_uv < crop_min) || any(source_uv > crop_max)) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    // Sfumatura del bordo di crop: quanto si è dentro il rettangolo, sul
    // lato più vicino.
    var alpha = 1.0;
    let softness = transform.fit_rot.w;
    if (softness > 0.0) {
        let inside = min(source_uv - crop_min, crop_max - source_uv);
        alpha = smoothstep(0.0, softness, min(inside.x, inside.y));
    }

    // U/V sono a metà risoluzione (4:2:0): campionarli alla stessa uv
    // del piano Y con un sampler bilineare fa anche l'upsampling della
    // croma, gratis.
    let y_sample = textureSample(y_tex, input_sampler, source_uv).r;
    let u_sample = textureSample(u_tex, input_sampler, source_uv).r;
    let v_sample = textureSample(v_tex, input_sampler, source_uv).r;

    let matrix_id = i32(transform.color.x);
    let full_range = transform.color.y > 0.5;
    let rgb = yuv_to_rgb(y_sample, u_sample, v_sample, matrix_id, full_range);
    return vec4<f32>(rgb, alpha);
}
