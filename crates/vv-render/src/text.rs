//! Rasterizzazione dei titoli (`ClipSource::Text`) via `cosmic-text`: il
//! testo è di un solo colore, quindi basta una maschera di copertura a 8
//! bit che il compositor colora (`Layer::Text`).

use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    UnderlineStyle, Weight,
};
use std::sync::{Arc, Mutex, OnceLock};
use vv_core::{HAnchor, TextAlign, TitleParams, VAnchor};

/// Copertura del testo (0 = trasparente), grande quanto il frame di output.
pub struct TextMask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
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
    cache: Vec<(CacheKey, Arc<TextMask>)>,
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

/// Maschera del titolo per un frame di output `output_size`; le misure di
/// `params` sono in pixel di una timeline `timeline_size`.
pub fn render_title(
    params: &TitleParams,
    timeline_size: (u32, u32),
    output_size: (u32, u32),
) -> Arc<TextMask> {
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

fn rasterize(
    font_system: &mut FontSystem,
    swash: &mut SwashCache,
    params: &TitleParams,
    timeline_size: (u32, u32),
    output_size: (u32, u32),
) -> TextMask {
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

    TextMask {
        width,
        height,
        data,
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
        let mask = render_title(&TitleParams::default(), (640, 360), (640, 360));
        let (x0, y0, x1, y1) = covered_bounds(&mask).expect("nessun pixel disegnato");
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
        let mask = render_title(&params, (640, 360), (640, 360));
        let (x0, ..) = covered_bounds(&mask).unwrap();
        assert!((x0 as i32 - 320).abs() < 12, "inizio x {x0}");
    }

    #[test]
    fn output_smaller_than_timeline_scales_the_text() {
        let full = render_title(&TitleParams::default(), (640, 360), (640, 360));
        let half = render_title(&TitleParams::default(), (640, 360), (320, 180));
        let width = |m: &TextMask| {
            let (x0, _, x1, _) = covered_bounds(m).unwrap();
            (x1 - x0) as f32
        };
        let ratio = width(&half) / width(&full);
        assert!((ratio - 0.5).abs() < 0.08, "rapporto {ratio}");
    }
}
