//! Frame cache LRU (`FrameCache`, singolo media — usata da
//! `playback::DecodeAhead` per l'anteprima "grezza" dal media pool) e
//! cache condivisa multi-media a budget globale (`SharedFrameCache`,
//! usata da `render_ahead::RenderAhead` per il buffer a livello di
//! timeline — vedi REFACTOR_PIPELINE.md §2).

use crate::decode::FrameRgba;
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use vv_core::{FrameIdx, MediaId};

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

    /// Rimuove ogni frame con indice `< idx`: non più raggiungibile
    /// tornando indietro dalla testina corrente, quindi non più utile a
    /// bufferizzare in avanti. Senza questo, lo sfratto della LRU standard
    /// avviene solo quando si *inserisce* un nuovo frame oltre la
    /// capacità — proporzionale a quanti frame nuovi arrivano, non a
    /// quanto la testina si è mossa. Se la testina avanza a piccoli
    /// passi (mai abbastanza per un seek reale, vedi
    /// `SEEK_THRESHOLD_FRAMES` in `render_ahead`) e il decoder resta
    /// comodamente avanti, il "fronte" del buffer può restare bloccato
    /// molto indietro rispetto alla testina per un tempo indefinito,
    /// mentre la coda si allunga di pochi frame ad ogni ciclo — lo
    /// scarto fisso tra testina e inizio del buffer segnalato
    /// dall'utente. Con questo, ad ogni ciclo il fronte è sempre la
    /// testina corrente (o il primo frame disponibile dopo di essa).
    pub fn evict_before(&self, idx: FrameIdx) {
        let mut cache = self.inner.lock().unwrap();
        let stale: Vec<FrameIdx> = cache.iter().map(|(&k, _)| k).filter(|&k| k < idx).collect();
        for k in stale {
            cache.pop(&k);
        }
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

/// Un intervallo di frame *sorgente* di un media attualmente "voluto"
/// dalla finestra di lookahead corrente, con la posizione in spazio
/// *timeline* del suo primo frame — necessaria per calcolare quanto un
/// frame in cache è distante dalla testina (vedi
/// `SharedFrameCache::reconcile`). Costruito da
/// `render_ahead::collect_media_segments` a ogni ciclo di poll, un
/// `WantedRange` per `MediaSegment`.
#[derive(Debug, Clone, Copy)]
pub struct WantedRange {
    pub media_id: MediaId,
    pub source_start: FrameIdx,
    pub source_end: FrameIdx,
    /// Dove `source_start` cade in spazio timeline: un frame sorgente
    /// `idx` dentro questo intervallo corrisponde alla posizione timeline
    /// `timeline_start + (idx - source_start)` (mappatura lineare, valida
    /// finché non esiste time-remap/speed ≠ 1 — vedi REFACTOR_PIPELINE.md
    /// B1).
    pub timeline_start: FrameIdx,
}

impl WantedRange {
    fn contains(&self, idx: FrameIdx) -> bool {
        idx >= self.source_start && idx <= self.source_end
    }

    fn timeline_position_of(&self, idx: FrameIdx) -> FrameIdx {
        self.timeline_start + (idx - self.source_start)
    }
}

struct SharedInner {
    entries: HashMap<(MediaId, FrameIdx), Arc<FrameRgba>>,
    bytes_used: usize,
}

/// Cache dei frame decodificati condivisa tra TUTTI i media della
/// finestra di lookahead corrente, a chiave composita `(MediaId,
/// FrameIdx sorgente)` e budget in **byte** (non in numero di frame:
/// media di risoluzioni diverse nella stessa timeline pesano
/// diversamente sulla stessa RAM). Sostituisce N `FrameCache`
/// indipendenti (una per media, con budget diviso a monte tra media
/// distinti e poi di nuovo tra segmenti dello stesso media) — quella
/// divisione arbitraria e il suo sfratto per capacità, indipendente da
/// dove fosse la testina, erano la radice di due bug distinti (vedi
/// REFACTOR_PIPELINE.md §1, A1/A2): qui la domanda "cosa tenere in
/// cache" ha una sola risposta (`reconcile`, sotto), non due che possono
/// contraddirsi.
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

    pub fn get(&self, media_id: MediaId, idx: FrameIdx) -> Option<Arc<FrameRgba>> {
        self.inner.lock().unwrap().entries.get(&(media_id, idx)).cloned()
    }

    pub fn contains(&self, media_id: MediaId, idx: FrameIdx) -> bool {
        self.inner
            .lock()
            .unwrap()
            .entries
            .contains_key(&(media_id, idx))
    }

    /// Byte totali attualmente occupati da frame decodificati, sommati
    /// sulla dimensione *reale* di ciascun frame (`data.len()`, non una
    /// stima uniforme): media di risoluzioni diverse pesano quanto
    /// pesano davvero. Il chiamante (il loop di fill in `render_ahead`)
    /// lo confronta con il budget per sapere quando fermarsi.
    pub fn bytes_used(&self) -> usize {
        self.inner.lock().unwrap().bytes_used
    }

    /// Inserisce un frame appena decodificato. Non applica da solo il
    /// budget — sta al chiamante fermarsi quando `bytes_used()` raggiunge
    /// il budget: inserendo sempre in ordine di priorità (più vicino alla
    /// testina prima, vedi doc di `reconcile`) questo basta, senza
    /// bisogno di sfrattare durante il fill.
    pub fn insert(&self, media_id: MediaId, idx: FrameIdx, frame: Arc<FrameRgba>) {
        let mut inner = self.inner.lock().unwrap();
        let bytes = frame.data.len();
        if let Some(old) = inner.entries.insert((media_id, idx), frame) {
            inner.bytes_used -= old.data.len();
        }
        inner.bytes_used += bytes;
    }

    /// Un solo pass di riconciliazione, chiamato una volta a ogni ciclo
    /// di poll (prima del fill) dopo aver aggiornato playhead e i
    /// segmenti della finestra corrente. Sostituisce con un'unica
    /// politica ciò che prima erano tre meccanismi separati capaci di
    /// contraddirsi (`retain` dei media usciti dalla finestra,
    /// `evict_before` posizionale, sfratto LRU per capacità).
    ///
    /// **Tier A — appartenenza alla finestra.** Scarta ogni frame il cui
    /// `(media_id, idx)` non cade in *nessuno* degli intervalli di
    /// `window` per quel media: copre insieme "dietro la testina",
    /// "media uscito dalla finestra" e "oltre l'orizzonte di lookahead".
    ///
    /// **Tier B — budget globale.** Se il totale supera `budget_bytes`,
    /// sfratta i frame *ancora nella finestra* più LONTANI dalla testina
    /// finché si rientra nel budget. **Mai per recency**: durante un
    /// fill in avanti il frame proprio alla testina è il primo inserito —
    /// il "meno recente" — e una LRU classica lo sfratterebbe per primo,
    /// l'esatto opposto di quel che serve. La distanza è calcolata in
    /// spazio *timeline* via `WantedRange::timeline_position_of`, non in
    /// spazio sorgente: due frame ugualmente lontani in indice sorgente
    /// possono corrispondere a distanze timeline molto diverse.
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
                *bytes_used -= frame.data.len();
            }
            keep
        });

        // Tier B.
        if inner.bytes_used > budget_bytes {
            let mut scored: Vec<((MediaId, FrameIdx), FrameIdx)> = inner
                .entries
                .keys()
                .map(|&(media_id, idx)| ((media_id, idx), distance(media_id, idx, playhead, window)))
                .collect();
            // Più lontano prima: si sfratta dalla coda.
            scored.sort_unstable_by_key(|&(_, dist)| std::cmp::Reverse(dist));
            for (key, _) in scored {
                if inner.bytes_used <= budget_bytes {
                    break;
                }
                if let Some(frame) = inner.entries.remove(&key) {
                    inner.bytes_used -= frame.data.len();
                }
            }
        }
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

/// Distanza (in frame di *timeline*) di `(media_id, idx)` dalla testina,
/// mappando via il primo `WantedRange` di `window` che lo contiene —
/// `FrameIdx::MAX` se nessuno lo contiene (non dovrebbe succedere per un
/// frame sopravvissuto al Tier A nello stesso pass, ma resta un
/// fallback sicuro anziché un panic).
fn distance(media_id: MediaId, idx: FrameIdx, playhead: FrameIdx, window: &[WantedRange]) -> FrameIdx {
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
            },
            content_hash: 0,
        };
        let a = project.media_pool.insert(item(320, 240));
        let b = project.media_pool.insert(item(320, 240));
        (a, b)
    }

    fn frame_of_size(bytes: usize) -> Arc<FrameRgba> {
        Arc::new(FrameRgba {
            width: 1,
            height: 1,
            data: vec![0; bytes],
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
        }];
        cache.reconcile(50, &window, 1_000_000);

        assert_eq!(cache.cached_ranges(media_a), vec![(50, 50)]);
        assert_eq!(cache.bytes_used(), 4);
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
