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

    /// The part `source_start..=source_end` of this range.
    pub fn sub_range(&self, source_start: FrameIdx, source_end: FrameIdx) -> Self {
        Self {
            source_start,
            source_end,
            timeline_start: self.timeline_position_of(source_start),
            ..*self
        }
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

    /// Drops every frame of `media_id`: for when the id now names another
    /// file, which `reconcile` cannot tell.
    pub fn remove_media(&self, media_id: MediaId) {
        let mut inner = self.inner.lock().unwrap();
        let inner = &mut *inner;
        let bytes_used = &mut inner.bytes_used;
        inner.entries.retain(|&(m, _), frame| {
            let keep = m != media_id;
            if !keep {
                *bytes_used -= frame.byte_len();
            }
            keep
        });
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
#[path = "tests/cache.rs"]
mod tests;
