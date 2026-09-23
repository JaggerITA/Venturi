//! `SharedFrameCache`: cache of the decoded frames of every media in the
//! window, on a global budget, see plans/REFACTOR_PIPELINE.md §2.

use crate::decode::FrameYuv420;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vv_core::{FrameIdx, MediaId};

/// Stretch of source frames wanted by the current window, with its
/// position on the timeline: used to measure the distance from the playhead.
#[derive(Debug, Clone, Copy)]
pub struct WantedRange {
    pub media_id: MediaId,
    pub source_start: FrameIdx,
    pub source_end: FrameIdx,
    /// Where `source_start` falls in timeline space.
    pub timeline_start: FrameIdx,
    /// The clip's `Clip::rate`: timeline frames per source frame.
    pub rate: vv_core::Rational,
}

impl WantedRange {
    fn contains(&self, idx: FrameIdx) -> bool {
        idx >= self.source_start && idx <= self.source_end
    }

    fn timeline_position_of(&self, idx: FrameIdx) -> FrameIdx {
        self.timeline_start + self.rate.scale_round(idx) - self.rate.scale_round(self.source_start)
    }
}

struct SharedInner {
    entries: HashMap<(MediaId, FrameIdx), Arc<FrameYuv420>>,
    bytes_used: usize,
}

/// Cache of the frames of every media in the window, keyed by
/// `(MediaId, source frame)` with a budget in bytes (different resolutions
/// weigh differently). What to keep is decided by `reconcile` alone.
pub struct SharedFrameCache {
    inner: Mutex<SharedInner>,
}

impl Default for SharedFrameCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedFrameCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SharedInner {
                entries: HashMap::new(),
                bytes_used: 0,
            }),
        }
    }

    pub fn get(&self, media_id: MediaId, idx: FrameIdx) -> Option<Arc<FrameYuv420>> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&(media_id, idx))
            .cloned()
    }

    pub fn contains(&self, media_id: MediaId, idx: FrameIdx) -> bool {
        self.inner
            .lock()
            .unwrap()
            .entries
            .contains_key(&(media_id, idx))
    }

    /// Bytes used, on the real size of each frame.
    pub fn bytes_used(&self) -> usize {
        self.inner.lock().unwrap().bytes_used
    }

    /// Does not enforce the budget: the caller inserts in priority order and
    /// stops on its own.
    pub fn insert(&self, media_id: MediaId, idx: FrameIdx, frame: Arc<FrameYuv420>) {
        let mut inner = self.inner.lock().unwrap();
        let bytes = frame.byte_len();
        if let Some(old) = inner.entries.insert((media_id, idx), frame) {
            inner.bytes_used -= old.byte_len();
        }
        inner.bytes_used += bytes;
    }

    /// Empties everything: for when the frames come from a wrong source (e.g.
    /// the proxy toggle changes), which `reconcile` would not be able to tell.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        inner.bytes_used = 0;
    }

    /// The only eviction policy, run every cycle before the fill.
    /// Tier A: out goes whatever falls in no interval of `window`.
    /// Tier B: past `budget_bytes`, out go the frames farthest from the playhead
    /// in timeline frames. Never by recency: during the fill the frame at the
    /// playhead is the least recent one.
    pub fn reconcile(&self, playhead: FrameIdx, window: &[WantedRange], budget_bytes: usize) {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;

        // Tier A.
        let entries = &mut inner.entries;
        let bytes_used = &mut inner.bytes_used;
        entries.retain(|&(media_id, idx), frame| {
            let keep = window
                .iter()
                .any(|w| w.media_id == media_id && w.contains(idx));
            if !keep {
                *bytes_used -= frame.byte_len();
            }
            keep
        });

        // Tier B.
        if inner.bytes_used > budget_bytes {
            let mut scored: Vec<((MediaId, FrameIdx), FrameIdx)> = inner
                .entries
                .keys()
                .map(|&(media_id, idx)| {
                    ((media_id, idx), distance(media_id, idx, playhead, window))
                })
                .collect();
            // Farthest first: eviction works from the tail.
            scored.sort_unstable_by_key(|&(_, dist)| std::cmp::Reverse(dist));
            for (key, _) in scored {
                if inner.bytes_used <= budget_bytes {
                    break;
                }
                if let Some(frame) = inner.entries.remove(&key) {
                    inner.bytes_used -= frame.byte_len();
                }
            }
        }
    }

    /// All the frames `start..=end` of the media are cached. Stops at the
    /// first missing one, without building the intervals.
    pub fn covers(&self, media_id: MediaId, start: FrameIdx, end: FrameIdx) -> bool {
        let inner = self.inner.lock().unwrap();
        (start..=end).all(|idx| inner.entries.contains_key(&(media_id, idx)))
    }

    /// Contiguous (inclusive) intervals currently cached for a media — for
    /// the "buffered" indicator in the UI.
    pub fn cached_ranges(&self, media_id: MediaId) -> Vec<(FrameIdx, FrameIdx)> {
        let inner = self.inner.lock().unwrap();
        let mut indices: Vec<FrameIdx> = inner
            .entries
            .keys()
            .filter(|&&(m, _)| m == media_id)
            .map(|&(_, idx)| idx)
            .collect();
        indices.sort_unstable();

        let mut ranges: Vec<(FrameIdx, FrameIdx)> = Vec::new();
        for idx in indices {
            match ranges.last_mut() {
                Some((_, end)) if idx == *end + 1 => *end = idx,
                _ => ranges.push((idx, idx)),
            }
        }
        ranges
    }
}

/// Distance in timeline frames from the playhead; `MAX` if outside the window.
fn distance(
    media_id: MediaId,
    idx: FrameIdx,
    playhead: FrameIdx,
    window: &[WantedRange],
) -> FrameIdx {
    window
        .iter()
        .filter(|w| w.media_id == media_id && w.contains(idx))
        .map(|w| (w.timeline_position_of(idx) - playhead).abs())
        .min()
        .unwrap_or(FrameIdx::MAX)
}

#[cfg(test)]
mod tests {
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
            "il frame alla testina non deve mai essere sfrattato per fare spazio a uno più lontano"
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
            "sovrascrivere lo stesso (media, idx) non deve sommare le due dimensioni"
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
        assert!(!cache.covers(media_a, 5, 9), "buco a 8");
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
}
