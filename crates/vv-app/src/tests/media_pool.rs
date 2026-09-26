use super::*;

/// `MediaId` is a slotmap key, not constructible by hand.
fn ids(n: usize) -> Vec<MediaId> {
    let mut project = vv_core::Project::default();
    (0..n)
        .map(|_| {
            project.media_pool.insert(vv_core::MediaItem {
                path: "/x".into(),
                meta: vv_core::MediaMeta {
                    duration_frames: 0,
                    fps: vv_core::Rational::new(25, 1),
                    width: 0,
                    height: 0,
                    has_video: true,
                    has_audio: false,
                    sample_rate: 0,
                    channels: 0,
                    audio_streams: 0,
                    file: Default::default(),
                },
                content_hash: 0,
                compound: None,
            })
        })
        .collect()
}

fn plain() -> egui::Modifiers {
    egui::Modifiers::NONE
}

fn ctrl() -> egui::Modifiers {
    egui::Modifiers::COMMAND
}

fn shift() -> egui::Modifiers {
    egui::Modifiers::SHIFT
}

#[test]
fn plain_click_replaces_selection() {
    let order = ids(3);
    let mut state = MediaPoolState::default();
    state.click(order[0], plain(), &order);
    state.click(order[2], plain(), &order);
    assert_eq!(state.selected, BTreeSet::from([order[2]]));
}

#[test]
fn ctrl_click_toggles() {
    let order = ids(3);
    let mut state = MediaPoolState::default();
    state.click(order[0], plain(), &order);
    state.click(order[2], ctrl(), &order);
    assert_eq!(state.selected, BTreeSet::from([order[0], order[2]]));
    state.click(order[0], ctrl(), &order);
    assert_eq!(state.selected, BTreeSet::from([order[2]]));
}

#[test]
fn shift_click_selects_range_and_keeps_the_anchor() {
    let order = ids(4);
    let mut state = MediaPoolState::default();
    state.click(order[1], plain(), &order);
    state.click(order[3], shift(), &order);
    assert_eq!(
        state.selected,
        BTreeSet::from([order[1], order[2], order[3]])
    );
    // The anchor stayed the first one: a second shift+click still starts from there.
    state.click(order[0], shift(), &order);
    assert_eq!(state.selected, BTreeSet::from([order[0], order[1]]));
}

fn names(items: &[(&str, f64)]) -> Vec<(String, f64)> {
    items.iter().map(|(n, d)| ((*n).to_string(), *d)).collect()
}

fn sorted(items: &[(&str, f64)], sort: Sort) -> Vec<String> {
    let mut rows = names(items);
    sort_items(&mut rows, sort, |r| r.0.as_str(), |r| r.1);
    rows.into_iter().map(|r| r.0).collect()
}

#[test]
fn sorts_by_name_and_duration_in_both_directions() {
    let items = [("b.mp4", 5.0), ("a.mp4", 9.0), ("c.mp4", 1.0)];
    assert_eq!(
        sorted(
            &items,
            Sort {
                key: SortKey::Name,
                ascending: true
            }
        ),
        ["a.mp4", "b.mp4", "c.mp4"]
    );
    assert_eq!(
        sorted(
            &items,
            Sort {
                key: SortKey::Name,
                ascending: false
            }
        ),
        ["c.mp4", "b.mp4", "a.mp4"]
    );
    assert_eq!(
        sorted(
            &items,
            Sort {
                key: SortKey::Duration,
                ascending: true
            }
        ),
        ["c.mp4", "b.mp4", "a.mp4"]
    );
    assert_eq!(
        sorted(
            &items,
            Sort {
                key: SortKey::Duration,
                ascending: false
            }
        ),
        ["a.mp4", "b.mp4", "c.mp4"]
    );
}

#[test]
fn equal_durations_keep_the_name_order() {
    let items = [("b.mp4", 5.0), ("a.mp4", 5.0)];
    assert_eq!(
        sorted(
            &items,
            Sort {
                key: SortKey::Duration,
                ascending: true
            }
        ),
        ["a.mp4", "b.mp4"]
    );
}

#[test]
fn clicking_the_same_column_inverts_the_order() {
    let mut state = MediaPoolState::default();
    assert_eq!(
        state.sort,
        Sort {
            key: SortKey::Name,
            ascending: true
        }
    );
    state.toggle_sort(SortKey::Name);
    assert_eq!(
        state.sort,
        Sort {
            key: SortKey::Name,
            ascending: false
        }
    );
    state.toggle_sort(SortKey::Duration);
    assert_eq!(
        state.sort,
        Sort {
            key: SortKey::Duration,
            ascending: true
        }
    );
    state.toggle_sort(SortKey::Duration);
    assert_eq!(
        state.sort,
        Sort {
            key: SortKey::Duration,
            ascending: false
        }
    );
    state.toggle_sort(SortKey::Name);
    assert_eq!(
        state.sort,
        Sort {
            key: SortKey::Name,
            ascending: true
        }
    );
}

#[test]
fn shift_click_without_anchor_selects_only_the_clicked_one() {
    let order = ids(3);
    let mut state = MediaPoolState::default();
    state.click(order[2], shift(), &order);
    assert_eq!(state.selected, BTreeSet::from([order[2]]));
}
