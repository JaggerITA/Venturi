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

    /// Capacità attuale (numero massimo di frame), non necessariamente
    /// quella passata a `new`: vedi `resize`.
    pub fn capacity(&self) -> usize {
        self.inner.lock().unwrap().cap().get()
    }

    /// Cambia la capacità a caldo, sfrattando gli elementi meno usati di
    /// recente se si restringe. Serve perché il budget di memoria per
    /// media è ricalcolato ad ogni ciclo (dipende da quanti media
    /// distinti sono nella finestra corrente, vedi `render_ahead`): senza
    /// questo, una cache creata quando il budget per-media era più
    /// piccolo (es. due media distinti nella finestra) restava bloccata
    /// a quella capacità anche dopo che il budget per-media era tornato
    /// più ampio (es. di nuovo un solo media) — chi decide quanto
    /// bufferizzare in avanti calcolava sulla capacità *nuova* (più
    /// ampia) mentre la cache reale ne aveva ancora una più piccola,
    /// risultando in uno sfratto dei frame più vicini alla testina più
    /// aggressivo del previsto (bug segnalato: il buffer inizia sempre
    /// qualche frame dopo la testina).
    pub fn resize(&self, capacity: usize) {
        self.inner
            .lock()
            .unwrap()
            .resize(NonZeroUsize::new(capacity.max(1)).unwrap());
    }

    /// Intervalli contigui (inclusivi) di frame attualmente in cache,
    /// ordinati per inizio crescente — per un indicatore visivo "buffered"
    /// nella UI (mostrare quali porzioni sono già decodificate durante la
    /// riproduzione). `lru::LruCache::iter` non è ordinato per chiave,
    /// quindi le chiavi vanno raccolte e ordinate prima di unire quelle
    /// adiacenti.
    pub fn cached_ranges(&self) -> Vec<(FrameIdx, FrameIdx)> {
        let cache = self.inner.lock().unwrap();
        let mut indices: Vec<FrameIdx> = cache.iter().map(|(&k, _)| k).collect();
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
    fn cached_ranges_is_empty_for_an_empty_cache() {
        let cache = FrameCache::new(4);
        assert!(cache.cached_ranges().is_empty());
    }

    #[test]
    fn cached_ranges_merges_contiguous_indices_into_one_range() {
        let cache = FrameCache::new(8);
        for idx in [5, 6, 7, 8] {
            cache.insert(idx, dummy_frame());
        }
        assert_eq!(cache.cached_ranges(), vec![(5, 8)]);
    }

    #[test]
    fn cached_ranges_keeps_gaps_as_separate_ranges_in_order() {
        let cache = FrameCache::new(8);
        for idx in [20, 1, 2, 10, 11, 12] {
            cache.insert(idx, dummy_frame());
        }
        assert_eq!(cache.cached_ranges(), vec![(1, 2), (10, 12), (20, 20)]);
    }
}
