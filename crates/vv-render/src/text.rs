//! Title rasterization (`ClipSource::Text`) via `cosmic-text`. Each element
//! (background, outline, shadow, text) is a single color: an 8-bit coverage
//! mask per element is enough, which the compositor colors and stacks
//! (`LayerContent::Text`).

use cosmic_text::{
    Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    UnderlineStyle, Weight,
};
use std::sync::{Arc, Mutex, OnceLock};
use vv_core::{HAnchor, Rgba, TextAlign, TitleParams, VAnchor};

/// Text coverage (0 = transparent), as large as the output frame.
pub struct TextMask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// The masks of a title from bottom to top, with the color of each one
/// (opacity already in the alpha).
pub struct TitleRender {
    pub layers: Vec<(TextMask, Rgba)>,
}

impl TitleRender {
    pub fn text(&self) -> &TextMask {
        &self.layers.last().expect("the text is always there").0
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
    /// Most recent last.
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

/// Scanning the system fonts takes a while: doing it ahead of time
/// avoids a stall on the first title.
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

/// Title masks for an output frame of `output_size`; the measures in
/// `params` are in pixels of a timeline of `timeline_size`.
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
    let mask = Arc::new(rasterize(
        font_system,
        swash,
        params,
        timeline_size,
        output_size,
    ));
    if state.cache.len() >= CACHE_LEN {
        state.cache.remove(0);
    }
    state.cache.push((key, mask.clone()));
    mask
}

/// Space the background leaves around the text, in font sizes.
const BACKGROUND_PADDING: f32 = 0.2;

fn text_attrs(params: &TitleParams) -> (Attrs<'_>, Align) {
    let family = if params.font_family.is_empty() {
        Family::SansSerif
    } else {
        Family::Name(&params.font_family)
    };
    let mut attrs = Attrs::new()
        .family(family)
        .weight(Weight(params.font_weight))
        .style(if params.italic {
            Style::Italic
        } else {
            Style::Normal
        })
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
    (attrs, align)
}

/// Shapes the text into `buffer` and returns the size of the block.
/// Without a width, alignment has no reference: the layout is redone on the
/// longest line. The extra pixel keeps rounding from wrapping precisely
/// that line.
fn shape_block(
    font_system: &mut FontSystem,
    buffer: &mut Buffer,
    params: &TitleParams,
    attrs: &Attrs,
    align: Align,
) -> (f32, f32) {
    buffer.set_size(None, None);
    buffer.set_text(
        &params.display_text(),
        attrs,
        Shaping::Advanced,
        Some(align),
    );
    buffer.shape_until_scroll(font_system, false);
    let block_w = buffer.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
    buffer.set_size(Some(block_w + 1.0), None);
    buffer.shape_until_scroll(font_system, false);
    let block_h = buffer
        .layout_runs()
        .map(|r| r.line_top + r.line_height)
        .fold(0.0, f32::max);
    (block_w, block_h)
}

/// Text block and background padding of a title, in timeline pixels: what
/// the background covers on the axes left at 0, which mean "around the
/// text" (see `TitleBackground`). The OTIO export needs them separately,
/// because Resolve has no such shorthand and shapes the text its own way.
pub fn title_metrics(params: &TitleParams) -> vv_core::TitleMetrics {
    let mut state = lock();
    let TextState { font_system, .. } = &mut *state;
    let font_size = params.size.max(1.0);
    let line_height = (font_size * 1.2 + params.line_spacing).max(1.0);
    let (attrs, align) = text_attrs(params);
    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    let block = shape_block(font_system, &mut buffer, params, &attrs, align);
    vv_core::TitleMetrics {
        block,
        padding: font_size * BACKGROUND_PADDING,
    }
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

    let (attrs, align) = text_attrs(params);

    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    let (block_w, block_h) = shape_block(font_system, &mut buffer, params, &attrs, align);

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

    buffer.draw(
        font_system,
        swash,
        Color::rgb(255, 255, 255),
        |x, y, w, h, color| {
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
                    // Overlapping glyphs (negative tracking): "over" between coverages.
                    *dst = (*dst as u32 + coverage * (255 - *dst as u32) / 255) as u8;
                }
            }
        },
    );

    let scale_px = |v: f32| v * scale;
    let text = TextMask {
        width,
        height,
        data,
    };
    let mut layers = Vec::new();

    let bg = &params.background;
    if bg.enabled {
        let padding = font_size * BACKGROUND_PADDING;
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
        let (fill, ring) =
            rounded_rect_masks(width, height, center, (box_w, box_h), radius, outline);
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

/// Fill and (if `outline > 0`) inner border of a rounded rectangle, with
/// the edges antialiased from the signed distance.
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

/// Three separable box blurs, a cheap approximation of a gaussian:
/// the halo extends by about `radius` pixels.
fn blur(mask: &mut TextMask, radius: f32) {
    let box_radius = (radius / 3.0).round() as usize;
    if box_radius == 0 {
        return;
    }
    let (w, h) = (mask.width as usize, mask.height as usize);
    // Outside the covered box, widened by how much the three passes
    // spread, everything is zero and stays zero: only blur inside it.
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
            box_blur_line(&line, box_radius, |x, v| {
                mask.data[y * w + xs.start + x] = v
            });
        }
        for x in xs.clone() {
            line.clear();
            line.extend(ys.clone().map(|y| mask.data[y * w + x]));
            box_blur_line(&line, box_radius, |y, v| {
                mask.data[(ys.start + y) * w + x] = v
            });
        }
    }
}

/// `(x0, y0, x1, y1)` inclusive of the non-zero pixels.
fn coverage_bounds(mask: &TextMask) -> Option<(usize, usize, usize, usize)> {
    let w = mask.width as usize;
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for (y, row) in mask.data.chunks_exact(w.max(1)).enumerate() {
        let (Some(first), Some(last)) = (
            row.iter().position(|&v| v > 0),
            row.iter().rposition(|&v| v > 0),
        ) else {
            continue;
        };
        let b = bounds.get_or_insert((first, y, last, y));
        b.0 = b.0.min(first);
        b.2 = b.2.max(last);
        b.3 = y;
    }
    bounds
}

/// Moving average over `2 * radius + 1` samples, with zeros past the edges.
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
#[path = "tests/text.rs"]
mod tests;
