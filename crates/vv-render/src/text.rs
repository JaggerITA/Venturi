//! Rasterizzazione dei titoli (`ClipSource::Text`) via `cosmic-text`. Ogni
//! elemento (sfondo, bordo, ombra, testo) è di un solo colore: basta una
//! maschera di copertura a 8 bit per ciascuno, che il compositor colora e
//! sovrappone (`Layer::Text`).

use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    UnderlineStyle, Weight,
};
use std::sync::{Arc, Mutex, OnceLock};
use vv_core::{HAnchor, Rgba, TextAlign, TitleParams, VAnchor};

/// Copertura del testo (0 = trasparente), grande quanto il frame di output.
pub struct TextMask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Le maschere di un titolo dal basso verso l'alto, col colore di
/// ciascuna (opacità già nell'alpha).
pub struct TitleRender {
    pub layers: Vec<(TextMask, Rgba)>,
}

impl TitleRender {
    pub fn text(&self) -> &TextMask {
        &self.layers.last().expect("il testo c'è sempre").0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFace {
    pub weight: u16,
    pub italic: bool,
    pub name: String,
}

type CacheKey = (TitleParams, (u32, u32), (u32, u32));

struct TextState {
    font_system: FontSystem,
    swash: SwashCache,
    /// Più recente in coda.
    cache: Vec<(CacheKey, Arc<TitleRender>)>,
}

const CACHE_LEN: usize = 16;

fn state() -> &'static Mutex<TextState> {
    static STATE: OnceLock<Mutex<TextState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(TextState {
            font_system: FontSystem::new(),
            swash: SwashCache::new(),
            cache: Vec::new(),
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, TextState> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// La scansione dei font di sistema richiede un po': farla in anticipo
/// evita un blocco al primo titolo.
pub fn warm_up() {
    let _ = state();
}

pub fn font_families() -> Vec<String> {
    let state = lock();
    let mut families: Vec<String> = state
        .font_system
        .db()
        .faces()
        .filter_map(|f| f.families.first().map(|(name, _)| name.clone()))
        .collect();
    families.sort_by_key(|f| f.to_lowercase());
    families.dedup();
    families
}

pub fn font_faces(family: &str) -> Vec<FontFace> {
    let state = lock();
    let mut faces: Vec<FontFace> = state
        .font_system
        .db()
        .faces()
        .filter(|f| f.families.iter().any(|(name, _)| name == family))
        .map(|f| {
            let italic = f.style != cosmic_text::fontdb::Style::Normal;
            FontFace {
                weight: f.weight.0,
                italic,
                name: face_name(f.weight.0, italic),
            }
        })
        .collect();
    faces.sort_by_key(|f| (f.italic, f.weight));
    faces.dedup_by_key(|f| (f.italic, f.weight));
    faces
}

pub fn face_name(weight: u16, italic: bool) -> String {
    let base = match weight {
        0..=149 => "Thin",
        150..=249 => "ExtraLight",
        250..=349 => "Light",
        350..=449 => "Regular",
        450..=549 => "Medium",
        550..=649 => "SemiBold",
        650..=749 => "Bold",
        750..=849 => "ExtraBold",
        _ => "Black",
    };
    match (italic, base) {
        (true, "Regular") => "Italic".into(),
        (true, _) => format!("{base} Italic"),
        (false, _) => base.into(),
    }
}

/// Maschere del titolo per un frame di output `output_size`; le misure di
/// `params` sono in pixel di una timeline `timeline_size`.
pub fn render_title(
    params: &TitleParams,
    timeline_size: (u32, u32),
    output_size: (u32, u32),
) -> Arc<TitleRender> {
    let key: CacheKey = (params.clone(), timeline_size, output_size);
    let mut state = lock();
    if let Some(pos) = state.cache.iter().position(|(k, _)| *k == key) {
        let entry = state.cache.remove(pos);
        let mask = entry.1.clone();
        state.cache.push(entry);
        return mask;
    }
    let TextState {
        font_system, swash, ..
    } = &mut *state;
    let mask = Arc::new(rasterize(font_system, swash, params, timeline_size, output_size));
    if state.cache.len() >= CACHE_LEN {
        state.cache.remove(0);
    }
    state.cache.push((key, mask.clone()));
    mask
}

fn with_opacity(color: Rgba, opacity: f32) -> Rgba {
    Rgba {
        a: color.a * (opacity / 100.0).clamp(0.0, 1.0),
        ..color
    }
}

fn rasterize(
    font_system: &mut FontSystem,
    swash: &mut SwashCache,
    params: &TitleParams,
    timeline_size: (u32, u32),
    output_size: (u32, u32),
) -> TitleRender {
    let (width, height) = (output_size.0.max(1), output_size.1.max(1));
    let mut data = vec![0u8; (width * height) as usize];
    let scale = width as f32 / timeline_size.0.max(1) as f32;
    let font_size = (params.size * scale).max(1.0);
    let line_height = (font_size * 1.2 + params.line_spacing * scale).max(1.0);

    let family = if params.font_family.is_empty() {
        Family::SansSerif
    } else {
        Family::Name(&params.font_family)
    };
    let mut attrs = Attrs::new()
        .family(family)
        .weight(Weight(params.font_weight))
        .style(if params.italic { Style::Italic } else { Style::Normal })
        .letter_spacing(params.tracking / 1000.0);
    if params.underline {
        attrs.text_decoration.underline = UnderlineStyle::Single;
    }
    attrs.text_decoration.strikethrough = params.strikethrough;
    let align = match params.align {
        TextAlign::Left => Align::Left,
        TextAlign::Center => Align::Center,
        TextAlign::Right => Align::Right,
        TextAlign::Justify => Align::Justified,
    };

    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    buffer.set_size(None, None);
    buffer.set_text(&params.display_text(), &attrs, Shaping::Advanced, Some(align));
    buffer.shape_until_scroll(font_system, false);
    // Senza larghezza l'allineamento non ha un riferimento: si rifà il
    // layout sulla riga più lunga. Il pixel in più evita che l'arrotondamento
    // mandi a capo proprio quella riga.
    let block_w = buffer.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
    buffer.set_size(Some(block_w + 1.0), None);
    buffer.shape_until_scroll(font_system, false);
    let block_h = buffer
        .layout_runs()
        .map(|r| r.line_top + r.line_height)
        .fold(0.0, f32::max);

    let anchor_x = width as f32 / 2.0 + params.position[0] * scale;
    let anchor_y = height as f32 / 2.0 - params.position[1] * scale;
    let origin_x = anchor_x
        - match params.anchor.0 {
            HAnchor::Left => 0.0,
            HAnchor::Center => block_w / 2.0,
            HAnchor::Right => block_w,
        };
    let origin_y = anchor_y
        - match params.anchor.1 {
            VAnchor::Top => 0.0,
            VAnchor::Middle => block_h / 2.0,
            VAnchor::Bottom => block_h,
        };
    let (origin_x, origin_y) = (origin_x.round() as i32, origin_y.round() as i32);

    buffer.draw(font_system, swash, Color::rgb(255, 255, 255), |x, y, w, h, color| {
        let coverage = color.a() as u32;
        if coverage == 0 {
            return;
        }
        for py in y + origin_y..y + origin_y + h as i32 {
            if py < 0 || py >= height as i32 {
                continue;
            }
            for px in x + origin_x..x + origin_x + w as i32 {
                if px < 0 || px >= width as i32 {
                    continue;
                }
                let dst = &mut data[(py as u32 * width + px as u32) as usize];
                // Glifi sovrapposti (tracking negativo): "over" fra coperture.
                *dst = (*dst as u32 + coverage * (255 - *dst as u32) / 255) as u8;
            }
        }
    });

    let scale_px = |v: f32| v * scale;
    let text = TextMask {
        width,
        height,
        data,
    };
    let mut layers = Vec::new();

    let bg = &params.background;
    if bg.enabled {
        let padding = font_size * 0.2;
        let box_w = if bg.width > 0.0 {
            bg.width * width as f32
        } else {
            block_w + padding * 2.0
        };
        let box_h = if bg.height > 0.0 {
            bg.height * height as f32
        } else {
            block_h + padding * 2.0
        };
        let center = (
            origin_x as f32 + block_w / 2.0 + scale_px(bg.center[0]),
            origin_y as f32 + block_h / 2.0 - scale_px(bg.center[1]),
        );
        let radius = bg.corner_radius.clamp(0.0, 0.5) * box_w.min(box_h);
        let outline = scale_px(bg.outline_width).max(0.0);
        let (fill, ring) = rounded_rect_masks(width, height, center, (box_w, box_h), radius, outline);
        layers.push((fill, with_opacity(bg.color, bg.opacity)));
        if let Some(ring) = ring {
            layers.push((ring, with_opacity(bg.outline_color, bg.opacity)));
        }
    }

    let shadow = &params.shadow;
    if shadow.enabled {
        let dx = scale_px(shadow.offset[0]).round() as i32;
        let dy = -scale_px(shadow.offset[1]).round() as i32;
        let mut mask = shifted(&text, dx, dy);
        blur(&mut mask, scale_px(shadow.blur).max(0.0));
        layers.push((mask, with_opacity(shadow.color, shadow.opacity)));
    }

    layers.push((text, params.color));
    TitleRender { layers }
}

/// Riempimento e (se `outline > 0`) bordo interno di un rettangolo
/// arrotondato, con i bordi antialiasati dalla distanza con segno.
fn rounded_rect_masks(
    width: u32,
    height: u32,
    center: (f32, f32),
    size: (f32, f32),
    radius: f32,
    outline: f32,
) -> (TextMask, Option<TextMask>) {
    let len = (width * height) as usize;
    let mut fill = vec![0u8; len];
    let mut ring = (outline > 0.0).then(|| vec![0u8; len]);
    let half = (size.0 / 2.0, size.1 / 2.0);
    let x_range = ((center.0 - half.0 - 1.0).floor().max(0.0) as u32)
        ..((center.0 + half.0 + 1.0).ceil().clamp(0.0, width as f32) as u32);
    let y_range = ((center.1 - half.1 - 1.0).floor().max(0.0) as u32)
        ..((center.1 + half.1 + 1.0).ceil().clamp(0.0, height as f32) as u32);
    let coverage = |d: f32| (0.5 - d).clamp(0.0, 1.0);
    for y in y_range {
        for x in x_range.clone() {
            let px = (x as f32 + 0.5 - center.0).abs() - (half.0 - radius);
            let py = (y as f32 + 0.5 - center.1).abs() - (half.1 - radius);
            let outside = (px.max(0.0).powi(2) + py.max(0.0).powi(2)).sqrt();
            let d = outside + px.max(py).min(0.0) - radius;
            let i = (y * width + x) as usize;
            let inside = coverage(d);
            fill[i] = (inside * 255.0).round() as u8;
            if let Some(ring) = &mut ring {
                ring[i] = ((inside - coverage(d + outline)).max(0.0) * 255.0).round() as u8;
            }
        }
    }
    let mask = |data| TextMask {
        width,
        height,
        data,
    };
    (mask(fill), ring.map(mask))
}

fn shifted(mask: &TextMask, dx: i32, dy: i32) -> TextMask {
    let (w, h) = (mask.width as i32, mask.height as i32);
    let mut data = vec![0u8; mask.data.len()];
    for y in 0..h {
        let sy = y - dy;
        if sy < 0 || sy >= h {
            continue;
        }
        for x in 0..w {
            let sx = x - dx;
            if sx >= 0 && sx < w {
                data[(y * w + x) as usize] = mask.data[(sy * w + sx) as usize];
            }
        }
    }
    TextMask {
        width: mask.width,
        height: mask.height,
        data,
    }
}

/// Tre box blur separabili, un'approssimazione economica di una gaussiana:
/// l'alone si estende di circa `radius` pixel.
fn blur(mask: &mut TextMask, radius: f32) {
    let box_radius = (radius / 3.0).round() as usize;
    if box_radius == 0 {
        return;
    }
    let (w, h) = (mask.width as usize, mask.height as usize);
    // Fuori dal riquadro coperto, allargato di quanto i tre passaggi
    // spargono, è tutto zero e resta zero: si sfuma solo lì dentro.
    let Some((x0, y0, x1, y1)) = coverage_bounds(mask) else {
        return;
    };
    let spread = 3 * box_radius;
    let xs = x0.saturating_sub(spread)..(x1 + spread + 1).min(w);
    let ys = y0.saturating_sub(spread)..(y1 + spread + 1).min(h);
    let mut line = Vec::new();
    for _ in 0..3 {
        for y in ys.clone() {
            line.clear();
            line.extend_from_slice(&mask.data[y * w + xs.start..y * w + xs.end]);
            box_blur_line(&line, box_radius, |x, v| mask.data[y * w + xs.start + x] = v);
        }
        for x in xs.clone() {
            line.clear();
            line.extend(ys.clone().map(|y| mask.data[y * w + x]));
            box_blur_line(&line, box_radius, |y, v| mask.data[(ys.start + y) * w + x] = v);
        }
    }
}

/// `(x0, y0, x1, y1)` inclusivi dei pixel non nulli.
fn coverage_bounds(mask: &TextMask) -> Option<(usize, usize, usize, usize)> {
    let w = mask.width as usize;
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for (y, row) in mask.data.chunks_exact(w.max(1)).enumerate() {
        let (Some(first), Some(last)) =
            (row.iter().position(|&v| v > 0), row.iter().rposition(|&v| v > 0))
        else {
            continue;
        };
        let b = bounds.get_or_insert((first, y, last, y));
        b.0 = b.0.min(first);
        b.2 = b.2.max(last);
        b.3 = y;
    }
    bounds
}

/// Media mobile su `2 * radius + 1` campioni, con zeri oltre i bordi.
fn box_blur_line(src: &[u8], radius: usize, mut write: impl FnMut(usize, u8)) {
    let window = (2 * radius + 1) as u32;
    let mut sum: u32 = src.iter().take(radius + 1).map(|&v| v as u32).sum();
    for i in 0..src.len() {
        write(i, ((sum + window / 2) / window) as u8);
        if let Some(&v) = src.get(i + radius + 1) {
            sum += v as u32;
        }
        if i >= radius {
            sum -= src[i - radius] as u32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covered_bounds(mask: &TextMask) -> Option<(u32, u32, u32, u32)> {
        let mut bounds: Option<(u32, u32, u32, u32)> = None;
        for y in 0..mask.height {
            for x in 0..mask.width {
                if mask.data[(y * mask.width + x) as usize] > 0 {
                    let b = bounds.get_or_insert((x, y, x, y));
                    b.0 = b.0.min(x);
                    b.1 = b.1.min(y);
                    b.2 = b.2.max(x);
                    b.3 = b.3.max(y);
                }
            }
        }
        bounds
    }

    #[test]
    fn default_title_is_drawn_around_the_center() {
        let render = render_title(&TitleParams::default(), (640, 360), (640, 360));
        let (x0, y0, x1, y1) = covered_bounds(render.text()).expect("nessun pixel disegnato");
        let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
        assert!((cx as i32 - 320).abs() < 20, "centro x {cx}");
        assert!((cy as i32 - 180).abs() < 30, "centro y {cy}");
    }

    #[test]
    fn left_anchor_starts_the_text_at_the_position() {
        let params = TitleParams {
            anchor: (HAnchor::Left, VAnchor::Middle),
            ..Default::default()
        };
        let render = render_title(&params, (640, 360), (640, 360));
        let (x0, ..) = covered_bounds(render.text()).unwrap();
        assert!((x0 as i32 - 320).abs() < 12, "inizio x {x0}");
    }

    #[test]
    fn background_is_under_the_text_and_contains_it() {
        let params = TitleParams {
            background: vv_core::TitleBackground {
                enabled: true,
                outline_width: 2.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let render = render_title(&params, (640, 360), (640, 360));
        assert_eq!(render.layers.len(), 3, "sfondo, bordo, testo");
        let (tx0, ty0, tx1, ty1) = covered_bounds(render.text()).unwrap();
        let (bx0, by0, bx1, by1) = covered_bounds(&render.layers[0].0).unwrap();
        assert!(bx0 < tx0 && by0 < ty0 && bx1 > tx1 && by1 > ty1);
        // Il bordo sta sul perimetro: al centro del rettangolo non c'è.
        let ring = &render.layers[1].0;
        let (cx, cy) = ((bx0 + bx1) / 2, (by0 + by1) / 2);
        assert_eq!(ring.data[(cy * ring.width + cx) as usize], 0);
        assert!(ring.data[(cy * ring.width + bx0 + 1) as usize] > 0);
    }

    #[test]
    fn blur_limited_to_the_covered_area_matches_a_full_frame_blur() {
        let (w, h) = (64usize, 40usize);
        let mut data = vec![0u8; w * h];
        for y in 15..20 {
            for x in 20..30 {
                data[y * w + x] = 255;
            }
        }
        let mut mask = TextMask {
            width: w as u32,
            height: h as u32,
            data: data.clone(),
        };
        blur(&mut mask, 9.0);

        let mut line = Vec::new();
        for _ in 0..3 {
            for y in 0..h {
                line.clear();
                line.extend_from_slice(&data[y * w..(y + 1) * w]);
                box_blur_line(&line, 3, |x, v| data[y * w + x] = v);
            }
            for x in 0..w {
                line.clear();
                line.extend((0..h).map(|y| data[y * w + x]));
                box_blur_line(&line, 3, |y, v| data[y * w + x] = v);
            }
        }
        assert_eq!(mask.data, data);
    }

    #[test]
    fn shadow_follows_the_offset_and_spreads_with_blur() {
        let shadow = |blur| TitleParams {
            shadow: vv_core::TitleShadow {
                enabled: true,
                offset: [20.0, -10.0],
                blur,
                ..Default::default()
            },
            ..Default::default()
        };
        let sharp = render_title(&shadow(0.0), (640, 360), (640, 360));
        let (tx0, ty0, ..) = covered_bounds(sharp.text()).unwrap();
        let (sx0, sy0, ..) = covered_bounds(&sharp.layers[0].0).unwrap();
        assert_eq!((sx0 as i32 - tx0 as i32, sy0 as i32 - ty0 as i32), (20, 10));

        let soft = render_title(&shadow(12.0), (640, 360), (640, 360));
        let (bx0, ..) = covered_bounds(&soft.layers[0].0).unwrap();
        assert!(bx0 < sx0, "la sfocatura allarga l'ombra");
    }

    #[test]
    fn output_smaller_than_timeline_scales_the_text() {
        let full = render_title(&TitleParams::default(), (640, 360), (640, 360));
        let half = render_title(&TitleParams::default(), (640, 360), (320, 180));
        let width = |m: &TextMask| {
            let (x0, _, x1, _) = covered_bounds(m).unwrap();
            (x1 - x0) as f32
        };
        let ratio = width(half.text()) / width(full.text());
        assert!((ratio - 0.5).abs() < 0.08, "rapporto {ratio}");
    }
}
