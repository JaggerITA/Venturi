//! Frame cache LRU.
//!
//! Per ora la chiave è solo `FrameIdx`: il player opera su un singolo
//! media alla volta (milestone 2). La chiave completa `(MediaId,
//! SourceFrameIdx)` prevista in ARCHITECTURE.md arriva quando il player si
//! integra con il `Project`/`MediaPool` di vv-core (milestone 3+), per
//! condividere la cache tra clip diverse che referenziano lo stesso file.

use crate::decode::FrameRgba;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use vv_core::FrameIdx;

pub struct FrameCache {
    inner: Mutex<LruCache<FrameIdx, Arc<FrameRgba>>>,
}

impl FrameCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(NonZeroUsize::new(capacity.max(1)).unwrap())),
        }
    }

    pub fn get(&self, idx: FrameIdx) -> Option<Arc<FrameRgba>> {
        self.inner.lock().unwrap().get(&idx).cloned()
    }

    pub fn insert(&self, idx: FrameIdx, frame: Arc<FrameRgba>) {
        self.inner.lock().unwrap().put(idx, frame);
    }

    pub fn contains(&self, idx: FrameIdx) -> bool {
        self.inner.lock().unwrap().contains(&idx)
    }
}
