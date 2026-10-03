use super::*;

#[test]
fn ease_in_and_ease_out_are_slow_then_fast_and_viceversa() {
    assert!(Interpolation::EaseIn.ease(0.5) < 0.5);
    assert!(Interpolation::EaseOut.ease(0.5) > 0.5);
    for interp in Interpolation::PRESETS {
        assert_eq!(
            interp.ease(1.0),
            1.0,
            "{interp:?} must reach the destination"
        );
    }
}

#[test]
fn a_bezier_matches_the_linear_curve_when_its_controls_are_on_the_diagonal() {
    let linear = Interpolation::Bezier {
        c1: [1.0 / 3.0, 1.0 / 3.0],
        c2: [2.0 / 3.0, 2.0 / 3.0],
    };
    for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
        assert!((linear.ease(t) - t).abs() < 1e-3, "t={t}");
    }
}

#[test]
fn a_bezier_keyframe_interpolates_along_its_curve() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    // Controls squashed low: at half the segment the value is
    // still below half.
    k.upsert(
        0,
        0.0,
        Interpolation::Bezier {
            c1: [0.5, 0.0],
            c2: [1.0, 0.0],
        },
    );
    k.upsert(10, 100.0, Interpolation::Linear);
    assert!(k.value_at(5) < 50.0);
    assert_eq!(k.value_at(0), 0.0);
    assert_eq!(k.value_at(10), 100.0);
}

#[test]
fn set_interpolation_returns_the_previous_one_and_ignores_empty_frames() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    k.upsert(4, 1.0, Interpolation::Linear);
    assert_eq!(
        k.set_interpolation(4, Interpolation::Hold),
        Some(Interpolation::Linear)
    );
    assert_eq!(k.keyframe_at(4), Some((1.0, Interpolation::Hold)));
    assert_eq!(k.set_interpolation(7, Interpolation::Hold), None);
}

#[test]
fn value_at_returns_default_when_no_keyframes() {
    let k: Keyframed<f32> = Keyframed::constant(5.0);
    assert_eq!(k.value_at(0), 5.0);
    assert_eq!(k.value_at(1000), 5.0);
}

#[test]
fn value_at_holds_extremes_before_first_and_after_last() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    k.upsert(10, 100.0, Interpolation::Linear);
    k.upsert(20, 200.0, Interpolation::Linear);

    assert_eq!(k.value_at(0), 100.0, "before the first: value of the first");
    assert_eq!(k.value_at(30), 200.0, "after the last: value of the last");
}

#[test]
fn value_at_interpolates_linearly_between_two_keyframes() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    k.upsert(0, 0.0, Interpolation::Linear);
    k.upsert(10, 100.0, Interpolation::Linear);

    assert_eq!(k.value_at(0), 0.0);
    assert_eq!(k.value_at(5), 50.0);
    assert_eq!(k.value_at(10), 100.0);
}

#[test]
fn value_at_hold_steps_instead_of_interpolating() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    k.upsert(0, 0.0, Interpolation::Hold);
    k.upsert(10, 100.0, Interpolation::Hold);

    assert_eq!(k.value_at(5), 0.0, "Hold keeps the starting value");
    assert_eq!(k.value_at(9), 0.0);
    assert_eq!(
        k.value_at(10),
        100.0,
        "on the keyframe itself it has its value"
    );
}

#[test]
fn value_at_ease_in_out_matches_endpoints_and_stays_monotonic() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    k.upsert(0, 0.0, Interpolation::EaseInOut);
    k.upsert(10, 100.0, Interpolation::EaseInOut);

    assert_eq!(k.value_at(0), 0.0);
    assert_eq!(k.value_at(10), 100.0);
    let mid = k.value_at(5);
    assert!((0.0..=100.0).contains(&mid));
    // Monotonicity: values increasing with time.
    let mut last = k.value_at(0);
    for f in 1..=10 {
        let v = k.value_at(f);
        assert!(v >= last, "value_at must increase monotonically");
        last = v;
    }
}

#[test]
fn upsert_replaces_existing_keyframe_at_same_frame() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    k.upsert(5, 1.0, Interpolation::Linear);
    k.upsert(5, 2.0, Interpolation::Hold);

    assert_eq!(k.keyframes().len(), 1);
    assert_eq!(k.keyframe_at(5), Some((2.0, Interpolation::Hold)));
}

#[test]
fn remove_at_deletes_and_returns_the_keyframe() {
    let mut k: Keyframed<f32> = Keyframed::constant(0.0);
    k.upsert(5, 42.0, Interpolation::Linear);

    let removed = k.remove_at(5);
    assert_eq!(removed, Some((42.0, Interpolation::Linear)));
    assert!(k.is_constant());
    assert_eq!(k.remove_at(5), None, "removing twice must do nothing");
}

#[test]
fn transform_param_index_follows_all() {
    for (i, p) in TransformParam::ALL.iter().enumerate() {
        assert_eq!(p.index(), i);
    }
}

#[test]
fn transform_tracks_animate_each_param_on_its_own() {
    let mut tracks = TransformTracks::default();
    tracks
        .track_mut(TransformParam::PositionX)
        .upsert(0, 0.0, Interpolation::Linear);
    tracks
        .track_mut(TransformParam::PositionX)
        .upsert(10, 100.0, Interpolation::Linear);

    assert_eq!(tracks.value_at(5).position, [50.0, 0.0]);
    assert_eq!(
        tracks.value_at(5).zoom,
        [1.0, 1.0],
        "the other parameters stay at their default"
    );
    assert!(!tracks.is_constant());
}

#[test]
fn transform_tracks_find_the_nearest_keyframe_in_each_direction() {
    let mut tracks = TransformTracks::default();
    tracks
        .track_mut(TransformParam::Rotation)
        .upsert(10, 0.0, Interpolation::Linear);
    tracks
        .track_mut(TransformParam::ZoomX)
        .upsert(30, 2.0, Interpolation::Linear);

    let both = [TransformParam::Rotation, TransformParam::ZoomX];
    assert_eq!(tracks.previous_keyframe(&both, 20), Some(10));
    assert_eq!(tracks.next_keyframe(&both, 20), Some(30));
    assert_eq!(tracks.previous_keyframe(&both, 10), None, "not itself");
    assert_eq!(
        tracks.next_keyframe(&[TransformParam::Rotation], 20),
        None,
        "only the keyframes of the requested parameters"
    );
}

#[test]
fn transform_lerp_interpolates_each_field() {
    let a = Transform::default();
    let b = Transform {
        crop: [0.2, 0.2, 0.8, 0.8],
        crop_softness: 0.4,
        zoom: [3.0, 5.0],
        position: [1.0, -1.0],
        rotation: 90.0,
        anchor: [0.2, 0.4],
        flip: [true, true],
        opacity: 0.0,
    };
    let mid = Transform::lerp(&a, &b, 0.5);
    assert_eq!(mid.crop, [0.1, 0.1, 0.4, 0.4]);
    assert_eq!(mid.crop_softness, 0.2);
    assert_eq!(mid.zoom, [2.0, 3.0]);
    assert_eq!(mid.position, [0.5, -0.5]);
    assert_eq!(mid.rotation, 45.0);
    assert_eq!(mid.anchor, [0.1, 0.2]);
    assert_eq!(mid.opacity, 50.0);
    assert_eq!(
        mid.flip,
        [true, true],
        "flip snaps halfway, it does not blend"
    );
}

#[test]
fn rgba_lerp_interpolates_each_channel() {
    let a = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    };
    let b = Rgba {
        r: 1.0,
        g: 0.5,
        b: 0.2,
        a: 0.0,
    };
    let mid = Rgba::lerp(&a, &b, 0.5);
    assert_eq!((mid.r, mid.g, mid.b, mid.a), (0.5, 0.25, 0.1, 0.5));
}

#[test]
fn a_blur_direction_holds_until_the_next_keyframe() {
    let mut direction = Keyframed::constant(BlurDirection::Both);
    direction.upsert(0, BlurDirection::Horizontal, Interpolation::Linear);
    direction.upsert(10, BlurDirection::Vertical, Interpolation::Linear);
    assert_eq!(direction.value_at(9), BlurDirection::Horizontal);
    assert_eq!(direction.value_at(10), BlurDirection::Vertical);
}

#[test]
fn filter_keyframes_follow_trims_and_shifts_like_the_others() {
    let mut effects = EffectStack::default();
    let mut filter = ClipFilter::new(FilterKind::BoxBlur);
    filter.radius.upsert(10, 0.0, Interpolation::Linear);
    filter.radius.upsert(20, 20.0, Interpolation::Linear);
    filter
        .direction
        .upsert(10, BlurDirection::Vertical, Interpolation::Hold);
    effects.filters.push(filter);

    effects.drop_keyframes_before(15);
    let filter = &effects.filters[0];
    assert_eq!(
        filter.radius.value_at(15),
        10.0,
        "the value at the cut stays"
    );
    assert_eq!(filter.direction.value_at(15), BlurDirection::Vertical);

    effects.shift_keyframes(5);
    assert_eq!(effects.filters[0].radius.keyframes().last().unwrap().0, 25);
}
