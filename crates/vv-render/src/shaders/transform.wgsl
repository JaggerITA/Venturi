// Transform (crop, zoom, rotazione, posizione) e conversione YUV420->RGB
// in un triangolo fullscreen, senza vertex buffer.

struct TransformUniform {
    // left, top, right, bottom in coordinate normalizzate [0,1] sul source.
    crop: vec4<f32>,
    // zoom.x, zoom.y, position.x, position.y
    zoom_pos: vec4<f32>,
    // x/y: fattori di letterbox (>1 sull'asse scoperto). z: rotazione in
    // radianti, oraria. w: sfumatura del crop in frazioni del sorgente
    // (negativa verso l'interno).
    fit_rot: vec4<f32>,
    // anchor.x, anchor.y (pivot di zoom e rotazione, in frazioni del
    // frame di output dal centro della clip), flip.x, flip.y (0 o 1).
    anchor_flip: vec4<f32>,
    // x: matrice (0=BT.601, 1=BT.709, 2=BT.2020). y: 1 se full range.
    // z: aspect dell'output, per ruotare senza deformare.
    // w: 0 video, 1 colore pieno, 2 colore pieno con copertura nel piano Y
    // (testo).
    color: vec4<f32>,
    // RGBA del layer a colore pieno, al posto dei piani Y/U/V.
    solid: vec4<f32>,
    // x: opacità dell'intero layer (dissolvenze di clip). y/z/w inutilizzati.
    extra: vec4<f32>,
    // Id shader dei filtri attivi della clip, in ordine di applicazione
    // (0 = slot vuoto); vedi `filter_shader_id` in compositor.rs, l'unico
    // punto che sa a quale `FilterKind` corrisponde ciascun id.
    filters: array<vec4<f32>, 2>,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var input_sampler: sampler;
@group(0) @binding(4) var<uniform> transform: TransformUniform;
// Copertura per pixel (1x1 opaco per un layer senza vera trasparenza, es.
// un video decodificato — vedi YuvFrame::alpha in compositor.rs).
@group(0) @binding(5) var a_tex: texture_2d<f32>;

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

// Slot `i` (0..8) dentro i due vec4 di `TransformUniform.filters`: un
// array<vec4,2> non si indicizza linearmente in WGSL, va spacchettato.
fn filter_id_at(filters: array<vec4<f32>, 2>, i: i32) -> f32 {
    let group = filters[i / 4];
    let lane = i % 4;
    if (lane == 0) { return group.x; }
    if (lane == 1) { return group.y; }
    if (lane == 2) { return group.z; }
    return group.w;
}

// Applica un filtro in sequenza a `rgb`; l'ordine di chiamata (vedi il
// loop in `fs_main`) è l'ordine scelto dall'utente. Nuovi filtri: un nuovo
// id (`filter_shader_id`) e un nuovo ramo qui, nient'altro nella pipeline.
fn apply_filter(rgb: vec3<f32>, id: f32) -> vec3<f32> {
    if (id > 0.5 && id < 1.5) { // Grayscale
        let luma = dot(rgb, vec3<f32>(0.299, 0.587, 0.114));
        return vec3<f32>(luma, luma, luma);
    }
    return rgb;
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

    // Dall'output al sorgente: inverso di position, poi di rotazione e zoom
    // attorno all'anchor, poi del fit.
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
    let softness = transform.fit_rot.w;

    // Fuori dalla clip non c'è nulla da mostrare (la sfumatura verso
    // l'esterno non deve spalmare il bordo del sorgente).
    if (any(source_uv < vec2<f32>(0.0, 0.0)) || any(source_uv > vec2<f32>(1.0, 1.0))) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }

    // Distanza dal bordo di crop più vicino, negativa fuori: la sfumatura è
    // una rampa di alpha attorno a quel bordo.
    let inside = min(source_uv - crop_min, crop_max - source_uv);
    let edge_distance = min(inside.x, inside.y);
    var alpha = 1.0;
    if (softness < 0.0) {
        if (edge_distance < 0.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 0.0);
        }
        alpha = smoothstep(0.0, -softness, edge_distance);
    } else if (softness > 0.0) {
        if (edge_distance < -softness) {
            return vec4<f32>(0.0, 0.0, 0.0, 0.0);
        }
        alpha = smoothstep(-softness, 0.0, edge_distance);
    } else if (edge_distance < 0.0) {
        // Crop netto: si vede il layer sotto.
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    alpha = alpha * transform.extra.x;

    // U/V sono a metà risoluzione (4:2:0): campionarli alla stessa uv
    // del piano Y con un sampler bilineare fa anche l'upsampling della
    // croma, gratis.
    let mode = transform.color.w;
    var rgb: vec3<f32>;
    var out_alpha: f32;
    if (mode > 0.5 && mode < 1.5) {
        rgb = transform.solid.rgb;
        out_alpha = alpha * transform.solid.a;
    } else {
        let y_sample = textureSample(y_tex, input_sampler, source_uv).r;
        if (mode > 1.5) {
            rgb = transform.solid.rgb;
            out_alpha = alpha * transform.solid.a * y_sample;
        } else {
            let u_sample = textureSample(u_tex, input_sampler, source_uv).r;
            let v_sample = textureSample(v_tex, input_sampler, source_uv).r;
            let matrix_id = i32(transform.color.x);
            let full_range = transform.color.y > 0.5;
            rgb = yuv_to_rgb(y_sample, u_sample, v_sample, matrix_id, full_range);
            out_alpha = alpha;
        }
    }
    for (var slot = 0; slot < 8; slot = slot + 1) {
        let id = filter_id_at(transform.filters, slot);
        if (id > 0.5) {
            rgb = apply_filter(rgb, id);
        }
    }
    // Placeholder 1x1 per un layer senza vera copertura per pixel: campiona
    // sempre 1.0, nessun effetto (vedi doc di `a_tex`).
    out_alpha = out_alpha * textureSample(a_tex, input_sampler, source_uv).r;
    return vec4<f32>(rgb, out_alpha);
}
