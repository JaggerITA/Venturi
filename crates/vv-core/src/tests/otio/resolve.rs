use super::*;

/// Every mode of ours exists in Resolve, and the values are the ones a
/// file with one clip per entry of its menu carries.
#[test]
fn the_composite_modes_map_both_ways() {
    for blend in BlendMode::ALL {
        let mode = composite_mode(blend);
        assert_eq!(blend_mode(mode), Some(blend), "{blend:?}");
    }
    assert_eq!(composite_mode(BlendMode::Divide), 18);
    assert_eq!(blend_mode(2), Some(BlendMode::Subtract));
    assert_eq!(blend_mode(14), None, "we don't have Hue");
}

#[test]
fn the_clip_color_names_map_back_to_the_same_color() {
    for color in ClipColor::ALL {
        let name = clip_color_name(color);
        let back: ClipColor = serde_json::from_value(json!(name)).unwrap();
        assert_eq!(back, color, "{name}");
    }
}
