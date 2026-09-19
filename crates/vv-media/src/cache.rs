//! `SharedFrameCache`: cache dei frame decodificati di tutti i media della
//! finestra, a budget globale, vedi REFACTOR_PIPELINE.md §2.

use crate::decode::FrameYuv420;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vv_core::{FrameIdx, MediaId};

/// Tratto di frame sorgente voluto dalla finestra corrente, con la sua
/// posizione in timeline: serve a misurare la distanza dalla testina.
#[derive(Debug, Clone, Copy)]
pub struct WantedRange {
    pub media_id: MediaId,
    pub source_start: FrameIdx,
    pub source_end: FrameIdx,
    /// Dove `source_start` cade in spazio timeline.
    pub timeline_start: FrameIdx,
    /// `Clip::rate` della clip: frame di timeline per frame sorgente.
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

/// Cache dei frame di tutti i media della finestra, a chiave
/// `(MediaId, frame sorgente)` e budget in byte (risoluzioni diverse
/// pesano diversamente). Cosa tenere lo decide solo `reconcile`.
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

    /// Byte occupati, sulla dimensione reale di ogni frame.
    pub fn bytes_used(&self) -> usize {
        self.inner.lock().unwrap().bytes_used
    }

    /// Non applica il budget: il chiamante inserisce in ordine di priorità e
    /// si ferma da sé.
    pub fn insert(&self, media_id: MediaId, idx: FrameIdx, frame: Arc<FrameYuv420>) {
        let mut inner = self.inner.lock().unwrap();
        let bytes = frame.byte_len();
        if let Some(old) = inner.entries.insert((media_id, idx), frame) {
            inner.bytes_used -= old.byte_len();
        }
        inner.bytes_used += bytes;
    }

    /// Svuota tutto: per quando i frame vengono da una sorgente sbagliata (es.
    /// cambia il toggle proxy), che `reconcile` non saprebbe riconoscere.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        inner.bytes_used = 0;
    }

    /// Unica politica di sfratto, a ogni ciclo prima del fill.
    /// Tier A: via ciò che non cade in nessun intervallo di `window`.
    /// Tier B: oltre `budget_bytes`, via i frame più lontani dalla testina in
    /// frame di timeline. Mai per recency: durante il fill il frame alla
    /// testina è il meno recente.
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
            // Più lontano prima: si sfratta dalla coda.
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

    /// Tutti i frame `start..=end` del media sono in cache. Si ferma al
    /// primo mancante, senza costruire gli intervalli.
    pub fn covers(&self, media_id: MediaId, start: FrameIdx, end: FrameIdx) -> bool {
        let inner = self.inner.lock().unwrap();
        (start..=end).all(|idx| inner.entries.contains_key(&(media_id, idx)))
    }

    /// Intervalli contigui (inclusivi) attualmente in cache per un
    /// media — per l'indicatore "buffered" nella UI.
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

/// Distanza in frame di timeline dalla testina; `MAX` se fuori finestra.
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
        };
        let a = project.media_pool.insert(item(320, 240));
        let b = project.media_pool.insert(item(320, 240));
        (a, b)
    }

    /// Frame con `bytes` byte totali, tutti nel piano Y (i test che lo
    /// usano verificano solo la contabilità byte/eviction, non pixel
    /// reali — dove finiscono i byte tra i tre piani non conta).
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
        })
    }

    #[test]
    fn shared_cache_reconcile_tier_a_drops_frames_outside_every_current_range() {
        let (media_a, _) = two_media_ids();
        let cache = SharedFrameCache::new();
        for idx in [5, 50, 100] {
            cache.insert(media_a, idx, frame_of_size(4));
        }
        // Solo 50 è dentro l'unico intervallo voluto in questo ciclo.
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

        // Solo media_a compare nella finestra di questo ciclo.
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

    /// Il cuore della sottigliezza in REFACTOR_PIPELINE.md §2: durante un
    /// fill in avanti il frame *alla testina* è il primo inserito — il
    /// "meno recente" per qualunque LRU classica. Una LRU-per-recency lo
    /// sfratterebbe per primo quando il budget non basta; il Tier B deve
    /// invece sfrattare il frame più LONTANO dalla testina, tenendo
    /// quello più vicino anche se è il più vecchio.
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
        // Inserito per primo (il "meno recente"), ma è il frame alla
        // testina: deve sopravvivere.
        cache.insert(media_a, 0, frame_of_size(4));
        // Inserito per ultimo (il "più recente"), ma è il più lontano
        // dalla testina: deve essere il primo a saltare.
        cache.insert(media_a, 90, frame_of_size(4));

        // Budget per un solo frame: forza una scelta tra i due.
        cache.reconcile(0, &window, 4);

        assert!(
            cache.contains(media_a, 0),
            "il frame alla testina non deve mai essere sfrattato per fare spazio a uno più lontano"
        );
        assert!(!cache.contains(media_a, 90));
    }

    /// Su una clip conformata (25 fps su timeline a 50) un frame sorgente
    /// vale due frame di timeline: la distanza va misurata lì.
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
        // Frame 30 di A = timeline 60; frame 50 di B = timeline 50.
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
