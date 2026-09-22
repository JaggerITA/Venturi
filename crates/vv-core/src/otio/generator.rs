//! Resolve's generators: a `GeneratorReference` whose `parameters` hold a
//! `Resolve_OTIO` list of effect blocks, one per section of its inspector.
//! Unlike the effects of a clip, here it writes every parameter even when it
//! is at its default, so we do the same.
//!
//! Colours travel as `#rrggbb` and lose their alpha, and the lengths that
//! are ours in timeline pixels become fractions of the frame.

use super::MeasureTitle;
use crate::model::{
    HAnchor, Rgba, TextAlign, TitleBackground, TitleParams, TitleShadow, VAnchor,
};
use serde_json::{Value, json};

pub(super) fn solid_color(color: Rgba) -> Value {
    reference("Solid Color", "Solid Color", json!([block(
        "Solid Color",
        "Generator",
        5,
        0,
        true,
        json!([
            parameter("Display Name", json!(""), json!(""), "String"),
            parameter("color", hex(color), json!("#000000"), "Color"),
        ]),
    )]))
}

pub(super) fn text(title: &TitleParams, frame: (f32, f32), measure: Option<MeasureTitle>) -> Value {
    reference("Text", "Rich", json!([
        rich_text(title, frame),
        drop_shadow(&title.shadow, frame),
        stroke(),
        background(title, frame, measure),
    ]))
}

fn rich_text(title: &TitleParams, frame: (f32, f32)) -> Value {
    block("Rich Text", "Rich Text", 24, 0, true, json!([
        parameter("rich text", json!(title.content), json!("Title"), "String"),
        json!({ "Parameter ID": "title blob", "Title HTML": html(title) }),
        parameter("anchor", json!(anchor_index(title.anchor)), json!(4), "UInt"),
        point("position", centred(title.position, frame), json!([0.5, 0.5])),
        animatable("transformationZoomX", json!(1.0), json!(1.0), [0.25, 4.0]),
        animatable("transformationZoomY", json!(1.0), json!(1.0), [0.25, 4.0]),
        parameter("transformationZoomLink", json!(true), json!(true), "Bool"),
        animatable("transformationRotationAngle", json!(0.0), json!(0.0), [-100_000.0, 100_000.0]),
    ]))
}

fn drop_shadow(shadow: &TitleShadow, frame: (f32, f32)) -> Value {
    block("Drop Shadow", "Drop Shadow", 8, 1, shadow.enabled, json!([
        parameter("shadow color", hex(shadow.color), json!("#000000"), "Color"),
        point("shadow offset", fraction(shadow.offset, frame), json!([0.0, 0.0])),
        animatable("shadow", json!(shadow.blur.round() as i64), json!(20), [1.0, 100.0]),
        animatable("shadow opacity", json!(shadow.opacity.round() as i64), json!(75), [0.0, 100.0]),
    ]))
}

/// We have no stroke: the block exists only because Resolve writes it.
fn stroke() -> Value {
    block("Stroke", "Stroke", 28, 1, true, json!([
        parameter("strokeColor", json!("#ffffff"), json!("#ffffff"), "Color"),
        animatable_no_keyframes("strokeSize", json!(0), json!(1), [0.0, 16.0]),
        parameter("strokeOutsideOnly", json!(false), json!(false), "Bool"),
    ]))
}

/// A width or a height of 0 means "around the text" for us and literally
/// nothing for Resolve, so the measured box takes its place.
fn background(title: &TitleParams, frame: (f32, f32), measure: Option<MeasureTitle>) -> Value {
    let background = &title.background;
    let box_size = measure.map(|measure| measure(title)).unwrap_or(frame);
    let axis = |value: f32, measured: f32, frame: f32| match value > 0.0 {
        true => value,
        false => (measured / frame).clamp(0.0, 2.0),
    };
    block("Background", "Background", 27, 1, background.enabled, json!([
        parameter("backgroundColor", hex(background.color), json!("#000000"), "Color"),
        parameter("backgroundOutlineColor", hex(background.outline_color), json!("#000000"), "Color"),
        animatable(
            "backgroundOutlineWidth",
            json!(background.outline_width.round() as i64),
            json!(0),
            [0.0, 30.0],
        ),
        animatable(
            "backgroundWidth",
            json!(axis(background.width, box_size.0, frame.0)),
            json!(0.9),
            [0.0, 2.0],
        ),
        animatable(
            "backgroundHeight",
            json!(axis(background.height, box_size.1, frame.1)),
            json!(0.0),
            [0.0, 2.0],
        ),
        animatable(
            "backgroundCornerRadius",
            json!(background.corner_radius),
            json!(0.037_037_037_037_037_035),
            [0.0, 1.0],
        ),
        point("backgroundCenter", fraction(background.center, frame), json!([0.0, 0.0])),
        animatable("backgroundOpacity", json!(background.opacity.round() as i64), json!(50), [0.0, 100.0]),
    ]))
}

fn reference(name: &str, kind: &str, blocks: Value) -> Value {
    json!({
        "OTIO_SCHEMA": "GeneratorReference.1",
        "name": name,
        "generator_kind": kind,
        "available_range": null,
        "available_image_bounds": null,
        "metadata": { "Resolve_OTIO": { "Generator Type": kind } },
        "parameters": { "Resolve_OTIO": blocks },
    })
}

fn block(
    effect_name: &str,
    name: &str,
    type_id: u32,
    display_type: u32,
    enabled: bool,
    parameters: Value,
) -> Value {
    json!({
        "Effect Name": effect_name,
        "Name": name,
        "Type": type_id,
        "Display Type": display_type,
        "Enabled": enabled,
        "Parameters": parameters,
    })
}

fn parameter(id: &str, value: Value, default: Value, variant: &str) -> Value {
    json!({
        "Parameter ID": id,
        "Parameter Value": value,
        "Default Parameter Value": default,
        "Variant Type": variant,
    })
}

fn animatable(id: &str, value: Value, default: Value, range: [f64; 2]) -> Value {
    let variant = if value.is_i64() { "Int" } else { "Double" };
    let mut parameter = parameter(id, value, default, variant);
    parameter["minValue"] = json!(range[0]);
    parameter["maxValue"] = json!(range[1]);
    parameter["Key Frames"] = json!({});
    parameter
}

/// Like `animatable`, for the parameters Resolve bounds but does not
/// keyframe.
fn animatable_no_keyframes(id: &str, value: Value, default: Value, range: [f64; 2]) -> Value {
    let mut parameter = animatable(id, value, default, range);
    parameter.as_object_mut().expect("oggetto").remove("Key Frames");
    parameter
}

fn point(id: &str, value: [f32; 2], default: Value) -> Value {
    let mut parameter = parameter(id, json!(value), default, "POINTF");
    parameter["Key Frames"] = json!({});
    parameter
}

/// From timeline pixels around the centre of the frame to the 0..1 of
/// Resolve, whose origin is the bottom left corner.
fn centred(position: [f32; 2], frame: (f32, f32)) -> [f32; 2] {
    [0.5 + position[0] / frame.0, 0.5 + position[1] / frame.1]
}

/// A displacement, with no origin to move.
fn fraction(offset: [f32; 2], frame: (f32, f32)) -> [f32; 2] {
    [offset[0] / frame.0, offset[1] / frame.1]
}

fn hex(color: Rgba) -> Value {
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    json!(format!("#{:02x}{:02x}{:02x}", channel(color.r), channel(color.g), channel(color.b)))
}

/// Row by row from the top left, so the centre falls on Resolve's default 4.
fn anchor_index(anchor: (HAnchor, VAnchor)) -> u32 {
    let column = match anchor.0 {
        HAnchor::Left => 0,
        HAnchor::Center => 1,
        HAnchor::Right => 2,
    };
    let row = match anchor.1 {
        VAnchor::Top => 0,
        VAnchor::Middle => 1,
        VAnchor::Bottom => 2,
    };
    row * 3 + column
}

/// The Qt rich text Resolve actually draws: the plain `rich text` parameter
/// beside it is only a label.
fn html(title: &TitleParams) -> String {
    let family = match title.font_family.is_empty() {
        true => "Sans Serif",
        false => &title.font_family,
    };
    let align = match title.align {
        TextAlign::Left => "left",
        TextAlign::Center => "center",
        TextAlign::Right => "right",
        TextAlign::Justify => "justify",
    };
    let mut style = format!(
        " font-family:'{}'; font-size:{}pt; font-weight:{}; color:{};",
        escape(family),
        title.size.round() as i64,
        title.font_weight,
        hex(title.color).as_str().unwrap_or("#ffffff"),
    );
    if title.italic {
        style.push_str(" font-style:italic;");
    }
    match (title.underline, title.strikethrough) {
        (true, true) => style.push_str(" text-decoration: underline line-through;"),
        (true, false) => style.push_str(" text-decoration: underline;"),
        (false, true) => style.push_str(" text-decoration: line-through;"),
        (false, false) => {}
    }
    if title.tracking != 0.0 {
        style.push_str(&format!(" letter-spacing:{}em;", title.tracking / 1000.0));
    }

    let paragraphs: String = display_lines(title)
        .map(|line| {
            format!(
                "\n<p align=\"{align}\" style=\" margin-top:0px; margin-bottom:0px; \
                 margin-left:0px; margin-right:0px; -qt-block-indent:0; text-indent:0px; \
                 line-height:{}; -qt-line-height-type: line-distance;\">\
                 <span style=\"{style}\">{}</span></p>",
                title.line_spacing.round() as i64,
                escape(&line),
            )
        })
        .collect();
    format!(
        "<!DOCTYPE HTML PUBLIC \"-//W3C//DTD HTML 4.0//EN\" \
         \"http://www.w3.org/TR/REC-html40/strict.dtd\">\n\
         <html><head><meta name=\"qrichtext\" content=\"1\" /><style type=\"text/css\">\n\
         p, li {{ white-space: pre-wrap; }}\n\
         </style></head><body style=\" font-family:'Sans Serif'; font-size:9pt; \
         font-weight:400; font-style:normal;\">{paragraphs}</body></html>"
    )
}

fn display_lines(title: &TitleParams) -> impl Iterator<Item = String> {
    let text = title.display_text();
    let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    lines.into_iter()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The colour of a `Solid Color`, or `None` if the block is not there.
pub(super) fn read_solid_color(reference: &Value) -> Option<Rgba> {
    let block = blocks(reference).find(|b| b["Effect Name"] == "Solid Color")?;
    read_hex(&value_of(&block, "color")?)
}

/// The title of a `Rich` generator, with what we cannot read left at its
/// default.
pub(super) fn read_text(reference: &Value, frame: (f32, f32)) -> TitleParams {
    let mut title = TitleParams::default();
    for block in blocks(reference) {
        let enabled = block["Enabled"] != false;
        match block["Effect Name"].as_str().unwrap_or("") {
            "Rich Text" => {
                read_rich_text(&block, &mut title, frame);
            }
            "Drop Shadow" => {
                title.shadow.enabled = enabled;
                read_shadow(&block, &mut title.shadow, frame);
            }
            "Background" => {
                title.background.enabled = enabled;
                read_background(&block, &mut title.background, frame);
            }
            _ => {}
        }
    }
    title
}

fn read_rich_text(block: &Value, title: &mut TitleParams, frame: (f32, f32)) {
    if let Some(html) = block["Parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["Parameter ID"] == "title blob")
        .and_then(|p| p["Title HTML"].as_str())
    {
        read_html(html, title);
    }
    if let Some(index) = value_of(block, "anchor").and_then(|v| v.as_u64()) {
        title.anchor = read_anchor(index);
    }
    if let Some(position) = value_of(block, "position").and_then(read_point) {
        title.position = [
            (position[0] - 0.5) * frame.0,
            (position[1] - 0.5) * frame.1,
        ];
    }
}

fn read_shadow(block: &Value, shadow: &mut TitleShadow, frame: (f32, f32)) {
    if let Some(color) = value_of(block, "shadow color").and_then(|v| read_hex(&v)) {
        shadow.color = color;
    }
    if let Some(offset) = value_of(block, "shadow offset").and_then(read_point) {
        shadow.offset = [offset[0] * frame.0, offset[1] * frame.1];
    }
    if let Some(blur) = value_of(block, "shadow").and_then(|v| v.as_f64()) {
        shadow.blur = blur as f32;
    }
    if let Some(opacity) = value_of(block, "shadow opacity").and_then(|v| v.as_f64()) {
        shadow.opacity = opacity as f32;
    }
}

fn read_background(block: &Value, background: &mut TitleBackground, frame: (f32, f32)) {
    let number = |id: &str| value_of(block, id).and_then(|v| v.as_f64()).map(|v| v as f32);
    if let Some(color) = value_of(block, "backgroundColor").and_then(|v| read_hex(&v)) {
        background.color = color;
    }
    if let Some(color) = value_of(block, "backgroundOutlineColor").and_then(|v| read_hex(&v)) {
        background.outline_color = color;
    }
    if let Some(value) = number("backgroundOutlineWidth") {
        background.outline_width = value;
    }
    if let Some(value) = number("backgroundWidth") {
        background.width = value;
    }
    if let Some(value) = number("backgroundHeight") {
        background.height = value;
    }
    if let Some(value) = number("backgroundCornerRadius") {
        background.corner_radius = value;
    }
    if let Some(value) = number("backgroundOpacity") {
        background.opacity = value;
    }
    if let Some(center) = value_of(block, "backgroundCenter").and_then(read_point) {
        background.center = [center[0] * frame.0, center[1] * frame.1];
    }
}

fn blocks(reference: &Value) -> impl Iterator<Item = Value> {
    reference["parameters"]["Resolve_OTIO"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
}

fn value_of(block: &Value, id: &str) -> Option<Value> {
    block["Parameters"]
        .as_array()?
        .iter()
        .find(|p| p["Parameter ID"] == id)
        .map(|p| p["Parameter Value"].clone())
}

fn read_point(value: Value) -> Option<[f32; 2]> {
    let axis = |i: usize| value.get(i)?.as_f64().map(|v| v as f32);
    Some([axis(0)?, axis(1)?])
}

fn read_hex(value: &Value) -> Option<Rgba> {
    let text = value.as_str()?.strip_prefix('#')?;
    let channel = |i: usize| {
        u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok().map(|v| v as f32 / 255.0)
    };
    Some(Rgba { r: channel(0)?, g: channel(1)?, b: channel(2)?, a: 1.0 })
}

fn read_anchor(index: u64) -> (HAnchor, VAnchor) {
    let column = match index % 3 {
        0 => HAnchor::Left,
        1 => HAnchor::Center,
        _ => HAnchor::Right,
    };
    let row = match index / 3 {
        0 => VAnchor::Top,
        1 => VAnchor::Middle,
        _ => VAnchor::Bottom,
    };
    (column, row)
}

/// Reads back the Qt rich text: the paragraphs give the content and the
/// alignment, the first `span` the look. Anything not found keeps the
/// default, so a blob written by someone else degrades instead of failing.
fn read_html(html: &str, title: &mut TitleParams) {
    let paragraphs: Vec<&str> = html.split("<p ").skip(1).collect();
    let lines: Vec<String> = paragraphs
        .iter()
        .filter_map(|p| {
            let start = p.find("<span")?;
            let text = &p[start..];
            let text = &text[text.find('>')? + 1..];
            Some(unescape(&text[..text.find("</span>").unwrap_or(text.len())]))
        })
        .collect();
    if !lines.is_empty() {
        title.content = lines.join("\n");
    }
    if let Some(align) = paragraphs.first().and_then(|p| attribute(p, "align")) {
        title.align = match align.as_str() {
            "left" => TextAlign::Left,
            "right" => TextAlign::Right,
            "justify" => TextAlign::Justify,
            _ => TextAlign::Center,
        };
    }
    let Some(style) = paragraphs
        .first()
        .and_then(|p| p.split("<span style=\"").nth(1))
        .and_then(|s| s.split('"').next())
    else {
        return;
    };
    if let Some(family) = css(style, "font-family") {
        title.font_family = family.trim_matches('\'').to_owned();
    }
    if let Some(size) = css(style, "font-size").and_then(|v| v.trim_end_matches("pt").parse().ok()) {
        title.size = size;
    }
    if let Some(weight) = css(style, "font-weight").and_then(|v| v.parse().ok()) {
        title.font_weight = weight;
    }
    if let Some(color) = css(style, "color").and_then(|v| read_hex(&json!(v))) {
        title.color = color;
    }
    if let Some(spacing) = css(style, "letter-spacing")
        .and_then(|v| v.trim_end_matches("em").parse::<f32>().ok())
    {
        title.tracking = spacing * 1000.0;
    }
    title.italic = css(style, "font-style").as_deref() == Some("italic");
    let decoration = css(style, "text-decoration").unwrap_or_default();
    title.underline = decoration.contains("underline");
    title.strikethrough = decoration.contains("line-through");
    if let Some(spacing) = css(style, "line-height").and_then(|v| v.parse().ok()) {
        title.line_spacing = spacing;
    }
}

/// The paragraph carries the line height, the span the rest.
fn css(style: &str, property: &str) -> Option<String> {
    style
        .split(';')
        .map(str::trim)
        .find_map(|rule| rule.strip_prefix(property)?.strip_prefix(':'))
        .map(|value| value.trim().to_owned())
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<String> {
    let rest = tag.split(&format!("{name}=\"")).nth(1)?;
    Some(rest.split('"').next()?.to_owned())
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}
