use super::*;

fn two_media_ids() -> (MediaId, MediaId) {
    use vv_core::{MediaItem, MediaMeta, Project, Rational};
    let mut project = Project::default();
    let item = |w: u32, h: u32| MediaItem {
        path: "dummy.mp4".into(),
        meta: MediaMeta {
            duration_frames: 0,
            fps: Rational::new(25, 1),
            width: w,
            height: h,
            has_video: true,
            has_audio: false,
            sample_rate: 0,
            channels: 0,
            audio_streams: 0,
        },
        content_hash: 0,
        compound: None,
    };
    let a = project.media_pool.insert(item(320, 240));
    let b = project.media_pool.insert(item(320, 240));
    (a, b)
}

/// Frame with `bytes` total bytes, all in the Y plane (the tests using
/// it only check the byte/eviction accounting, not real pixels — where
/// the bytes land among the three planes does not matter).
fn frame_of_size(bytes: usize) -> Arc<FrameYuv420> {
    Arc::new(FrameYuv420 {
        width: 1,
        height: 1,
        y: vec![0; bytes],
        u: vec![],
        v: vec![],
        u_width: 0,
        u_height: 0,
        matrix: crate::decode::ColorMatrix::Bt601,
        full_range: false,
        alpha: None,
    })
}

#[test]
fn shared_cache_reconcile_tier_a_drops_frames_outside_every_current_range() {
    let (media_a, _) = two_media_ids();
    let cache = SharedFrameCache::new();
    for idx in [5, 50, 100] {
        cache.insert(media_a, idx, frame_of_size(4));
    }
    // Only 50 is inside the single interval wanted in this cycle.
    let window = [WantedRange {
        media_id: media_a,
        source_start: 40,
        source_end: 60,
        timeline_start: 40,
        rate: vv_core::Rational::one(),
    }];
    cache.reconcile(50, &window, 1_000_000);

    assert_eq!(cache.cached_ranges(media_a), vec![(50, 50)]);
    assert_eq!(cache.bytes_used(), 4);
}

#[test]
fn shared_cache_clear_empties_every_media_and_resets_bytes_used() {
    let (media_a, media_b) = two_media_ids();
    let cache = SharedFrameCache::new();
    cache.insert(media_a, 1, frame_of_size(4));
    cache.insert(media_b, 1, frame_of_size(4));

    cache.clear();

    assert!(cache.cached_ranges(media_a).is_empty());
    assert!(cache.cached_ranges(media_b).is_empty());
    assert_eq!(cache.bytes_used(), 0);
}

#[test]
fn shared_cache_reconcile_tier_a_drops_media_entirely_outside_the_window() {
    let (media_a, media_b) = two_media_ids();
    let cache = SharedFrameCache::new();
    cache.insert(media_a, 10, frame_of_size(4));
    cache.insert(media_b, 10, frame_of_size(4));

    // Only media_a shows up in the window of this cycle.
    let window = [WantedRange {
        media_id: media_a,
        source_start: 0,
        source_end: 20,
        timeline_start: 0,
        rate: vv_core::Rational::one(),
    }];
    cache.reconcile(10, &window, 1_000_000);

    assert!(cache.contains(media_a, 10));
    assert!(!cache.contains(media_b, 10));
}

/// The heart of the subtlety in plans/REFACTOR_PIPELINE.md §2: during a
/// forward fill the frame *at the playhead* is the first inserted — the
/// "least recent" for any classic LRU. An LRU by recency would evict it
/// first when the budget runs short; Tier B must instead evict the frame
/// FARTHEST from the playhead, keeping the closest one even if it is the
/// oldest.
#[test]
fn shared_cache_reconcile_tier_b_evicts_by_distance_not_by_recency() {
    let (media_a, _) = two_media_ids();
    let cache = SharedFrameCache::new();
    let window = [WantedRange {
        media_id: media_a,
        source_start: 0,
        source_end: 99,
        timeline_start: 0,
        rate: vv_core::Rational::one(),
    }];
    // Inserted first (the "least recent"), but it is the frame at the
    // playhead: it must survive.
    cache.insert(media_a, 0, frame_of_size(4));
    // Inserted last (the "most recent"), but it is the farthest from
    // the playhead: it must be the first to go.
    cache.insert(media_a, 90, frame_of_size(4));

    // Budget for a single frame: forces a choice between the two.
    cache.reconcile(0, &window, 4);

    assert!(
        cache.contains(media_a, 0),
        "the frame at the playhead must never be evicted to make room for a farther one"
    );
    assert!(!cache.contains(media_a, 90));
}

/// On a conformed clip (25 fps on a 50 fps timeline) one source frame
/// is worth two timeline frames: the distance must be measured there.
#[test]
fn shared_cache_reconcile_measures_distance_in_timeline_frames() {
    let (media_a, media_b) = two_media_ids();
    let cache = SharedFrameCache::new();
    let window = [
        WantedRange {
            media_id: media_a,
            source_start: 0,
            source_end: 99,
            timeline_start: 0,
            rate: vv_core::Rational::new(2, 1),
        },
        WantedRange {
            media_id: media_b,
            source_start: 0,
            source_end: 99,
            timeline_start: 0,
            rate: vv_core::Rational::one(),
        },
    ];
    // Frame 30 of A = timeline 60; frame 50 of B = timeline 50.
    cache.insert(media_a, 30, frame_of_size(4));
    cache.insert(media_b, 50, frame_of_size(4));
    cache.reconcile(0, &window, 4);
    assert!(cache.contains(media_b, 50));
    assert!(!cache.contains(media_a, 30));
}

#[test]
fn shared_cache_insert_overwrite_updates_bytes_used_correctly() {
    let (media_a, _) = two_media_ids();
    let cache = SharedFrameCache::new();
    cache.insert(media_a, 0, frame_of_size(100));
    assert_eq!(cache.bytes_used(), 100);
    cache.insert(media_a, 0, frame_of_size(40));
    assert_eq!(
        cache.bytes_used(),
        40,
        "overwriting the same (media, idx) must not add the two sizes"
    );
}

#[test]
fn shared_cache_covers_only_contiguous_ranges() {
    let (media_a, media_b) = two_media_ids();
    let cache = SharedFrameCache::new();
    for idx in [5, 6, 7, 9] {
        cache.insert(media_a, idx, frame_of_size(4));
    }
    assert!(cache.covers(media_a, 5, 7));
    assert!(!cache.covers(media_a, 5, 9), "hole at 8");
    assert!(!cache.covers(media_b, 5, 5));
}

#[test]
fn shared_cache_cached_ranges_filters_by_media() {
    let (media_a, media_b) = two_media_ids();
    let cache = SharedFrameCache::new();
    cache.insert(media_a, 5, frame_of_size(4));
    cache.insert(media_a, 6, frame_of_size(4));
    cache.insert(media_b, 5, frame_of_size(4));

    assert_eq!(cache.cached_ranges(media_a), vec![(5, 6)]);
    assert_eq!(cache.cached_ranges(media_b), vec![(5, 5)]);
}
