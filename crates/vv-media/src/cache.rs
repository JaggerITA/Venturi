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

    /// Il frame a `idx` se già in cache; altrimenti il frame in cache più
    /// vicino a `idx` (in qualunque direzione), se ce n'è almeno uno.
    ///
    /// Dopo un seek/cambio clip il decode-ahead riparte dall'ultimo
    /// keyframe e decodifica in sequenza: raggiungere il frame esatto può
    /// richiedere di attraversare l'intero GOP, fino a ~1s su sorgenti con
    /// keyframe radi (bug segnalato: "lo scrubbing non è reattivo... il
    /// cambio da una clip all'altra resta in freeze sull'ultimo frame
    /// della clip precedente per 1s"). Nel frattempo mostrare un frame
    /// vicino (anche solo il keyframe stesso, il primo disponibile) è
    /// molto meno confuso che restare fermi su un frame di tutt'altro
    /// contesto: il viewer si "avvicina" progressivamente man mano che il
    /// decode-ahead avanza, invece di scattare di colpo all'arrivo
    /// dell'esatto frame target.
    pub fn get_or_nearest(&self, idx: FrameIdx) -> Option<Arc<FrameRgba>> {
        let mut cache = self.inner.lock().unwrap();
        if let Some(frame) = cache.get(&idx) {
            return Some(frame.clone());
        }
        cache
            .iter()
            .min_by_key(|&(&k, _)| (k - idx).abs())
            .map(|(_, frame)| frame.clone())
    }

    pub fn insert(&self, idx: FrameIdx, frame: Arc<FrameRgba>) {
        self.inner.lock().unwrap().put(idx, frame);
    }

    pub fn contains(&self, idx: FrameIdx) -> bool {
        self.inner.lock().unwrap().contains(&idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_frame() -> Arc<FrameRgba> {
        Arc::new(FrameRgba {
            width: 1,
            height: 1,
            data: vec![0, 0, 0, 0],
        })
    }

    #[test]
    fn get_or_nearest_returns_none_on_an_empty_cache() {
        let cache = FrameCache::new(4);
        assert!(cache.get_or_nearest(10).is_none());
    }

    #[test]
    fn get_or_nearest_prefers_the_exact_match_over_a_nearer_neighbor() {
        let cache = FrameCache::new(4);
        let exact = dummy_frame();
        cache.insert(10, exact.clone());
        cache.insert(9, dummy_frame());

        let got = cache.get_or_nearest(10).unwrap();
        assert!(Arc::ptr_eq(&got, &exact));
    }

    #[test]
    fn get_or_nearest_falls_back_to_the_closest_cached_frame_in_either_direction() {
        let cache = FrameCache::new(4);
        let closer = dummy_frame();
        cache.insert(100, dummy_frame()); // distanza 80 da 20
        cache.insert(23, closer.clone()); // distanza 3 da 20
        cache.insert(5, dummy_frame()); // distanza 15 da 20

        let got = cache.get_or_nearest(20).unwrap();
        assert!(Arc::ptr_eq(&got, &closer));
    }
}
