use super::*;

#[test]
fn fader_curve_puts_unity_at_its_mark_and_spans_the_whole_gain_range() {
    assert_eq!(fader_position(0.0), 0.7);
    assert_eq!(fader_position(vv_core::GAIN_DB_MIN), 0.0);
    assert_eq!(fader_position(vv_core::GAIN_DB_MAX), 1.0);
    assert_eq!(fader_position(-1000.0), 0.0, "clamped");
}

#[test]
fn fader_db_inverts_fader_position() {
    for db in [-100.0, -73.5, -30.0, -7.2, 0.0, 4.5, 30.0] {
        assert!((fader_db(fader_position(db)) - db).abs() < 1e-3, "{db}");
    }
}

#[test]
fn pan_reads_as_centre_or_side_percent() {
    assert_eq!(format_pan(0.0), "C");
    assert_eq!(format_pan(0.001), "C");
    assert_eq!(format_pan(-0.3), "L30");
    assert_eq!(format_pan(1.0), "R100");
}

#[test]
fn the_settings_window_follows_the_removal_of_an_effect_above_it() {
    let timeline = <vv_core::TimelineId as vv_core::Id>::from_raw(1);
    let channel = MixerChannel::Track(2);
    let mut state = MixerPanelState::default();
    let remove = |index| StripEdit {
        remove_effect: Some(index),
        ..StripEdit::default()
    };

    state.follow_edit(
        &StripEdit {
            open_settings: Some(2),
            ..StripEdit::default()
        },
        timeline,
        channel,
    );
    state.follow_edit(&remove(3), timeline, channel);
    state.follow_edit(&remove(0), timeline, MixerChannel::Master);
    assert_eq!(state.open_effect, Some((timeline, channel, 2)));
    state.follow_edit(&remove(0), timeline, channel);
    assert_eq!(state.open_effect, Some((timeline, channel, 1)));
    state.follow_edit(&remove(1), timeline, channel);
    assert_eq!(state.open_effect, None);
}

#[test]
fn dropping_in_the_gaps_around_an_effect_leaves_it_where_it_is() {
    assert_eq!(moved_to(1, 1), None);
    assert_eq!(moved_to(1, 2), None);
    assert_eq!(moved_to(1, 0), Some(0));
    assert_eq!(moved_to(0, 3), Some(2), "to the end of three");
    assert_eq!(moved_to(2, 1), Some(1));
}

#[test]
fn the_other_effects_shift_around_a_moved_one() {
    // [a b c d], b to the end: [a c d b].
    let after: Vec<usize> = (0..4).map(|i| index_after_move(i, 1, 3)).collect();
    assert_eq!(after, [0, 3, 1, 2]);
    // [a b c d], d to the front: [d a b c].
    let after: Vec<usize> = (0..4).map(|i| index_after_move(i, 3, 0)).collect();
    assert_eq!(after, [1, 2, 3, 0]);
}
