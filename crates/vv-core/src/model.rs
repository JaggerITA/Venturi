//! Modello dati del progetto. Nessun grafo a nodi: ogni Clip ha uno
//! `EffectStack` fisso. Le trasformazioni sono valutate a runtime a partire
//! dai keyframe, mai "bake-ate" nei frame cachati (vedi ARCHITECTURE.md).

use serde::{Deserialize, Serialize};
use slotmap::{SlotMap, new_key_type};
use std::path::PathBuf;

new_key_type! {
    pub struct MediaId;
    pub struct TimelineId;
}

/// Le Clip vivono in `Vec<Clip>` dentro ogni Track (non in un'arena
/// slotmap): l'ordine è significativo (tempo sulla track) e l'iterazione
/// sequenziale per il compositing beneficia della località in memoria.
/// L'ID è quindi un contatore semplice, non una chiave slotmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClipId(pub u64);

/// Frame rate come frazione esatta (es. 30000/1001 per 29.97fps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub const fn new(num: i32, den: i32) -> Self {
        Self { num, den }
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }
}

/// Indice di frame, sempre relativo al contesto in cui è usato: frame
/// sorgente di un media (fps nativo) oppure frame di Timeline (fps della
/// Timeline che lo contiene). I due spazi non vanno mai confusi.
pub type FrameIdx = i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMeta {
    pub duration_frames: FrameIdx,
    pub fps: Rational,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub path: PathBuf,
    pub meta: MediaMeta,
    /// Chiave stabile per cache dei frame e proxy: hash del contenuto
    /// (non del path), così spostare/rinominare il file non invalida nulla.
    pub content_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Interpolation {
    Hold,
    Linear,
    EaseInOut,
}

/// Parametro animabile via keyframe. Sempre ordinato per tempo crescente.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframed<T> {
    /// Se vuoto, il parametro è costante e va letto da `default`.
    keyframes: Vec<(FrameIdx, T, Interpolation)>,
    pub default: T,
}

impl<T: Clone> Keyframed<T> {
    pub fn constant(value: T) -> Self {
        Self {
            keyframes: Vec::new(),
            default: value,
        }
    }

    pub fn is_constant(&self) -> bool {
        self.keyframes.is_empty()
    }

    pub fn keyframes(&self) -> &[(FrameIdx, T, Interpolation)] {
        &self.keyframes
    }

    /// Inserisce o sostituisce il keyframe a `frame`, mantenendo l'ordine.
    pub fn upsert(&mut self, frame: FrameIdx, value: T, interpolation: Interpolation) {
        match self.keyframes.binary_search_by_key(&frame, |(f, _, _)| *f) {
            Ok(idx) => self.keyframes[idx] = (frame, value, interpolation),
            Err(idx) => self.keyframes.insert(idx, (frame, value, interpolation)),
        }
    }

    /// Rimuove il keyframe esattamente a `frame`, se esiste. Restituisce il
    /// valore rimosso (utile per l'undo).
    pub fn remove_at(&mut self, frame: FrameIdx) -> Option<(T, Interpolation)> {
        let idx = self
            .keyframes
            .binary_search_by_key(&frame, |(f, _, _)| *f)
            .ok()?;
        let (_, value, interp) = self.keyframes.remove(idx);
        Some((value, interp))
    }

    /// Il keyframe esattamente a `frame`, se esiste.
    pub fn keyframe_at(&self, frame: FrameIdx) -> Option<(T, Interpolation)> {
        let idx = self
            .keyframes
            .binary_search_by_key(&frame, |(f, _, _)| *f)
            .ok()?;
        let (_, value, interp) = &self.keyframes[idx];
        Some((value.clone(), *interp))
    }
}

/// Interpolazione lineare componente-per-componente tra due valori di un
/// parametro animabile. `Keyframed::value_at` ne ha bisogno per calcolare
/// il valore a un frame arbitrario tra due keyframe.
pub trait Lerp {
    fn lerp(a: &Self, b: &Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(a: &Self, b: &Self, t: f32) -> Self {
        a + (b - a) * t
    }
}

impl Lerp for Transform {
    fn lerp(a: &Self, b: &Self, t: f32) -> Self {
        let mut crop = [0.0; 4];
        for ((c, ca), cb) in crop.iter_mut().zip(a.crop).zip(b.crop) {
            *c = f32::lerp(&ca, &cb, t);
        }
        Self {
            crop,
            zoom: f32::lerp(&a.zoom, &b.zoom, t),
            position: [
                f32::lerp(&a.position[0], &b.position[0], t),
                f32::lerp(&a.position[1], &b.position[1], t),
            ],
        }
    }
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

impl<T: Lerp + Clone> Keyframed<T> {
    /// Valore del parametro al frame dato: `default` se non ci sono
    /// keyframe; prima del primo/dopo l'ultimo tiene il valore estremo;
    /// altrimenti interpola tra i due keyframe che lo racchiudono secondo
    /// l'`Interpolation` del keyframe di partenza.
    pub fn value_at(&self, frame: FrameIdx) -> T {
        if self.keyframes.is_empty() {
            return self.default.clone();
        }
        match self.keyframes.binary_search_by_key(&frame, |(f, _, _)| *f) {
            Ok(idx) => self.keyframes[idx].1.clone(),
            Err(0) => self.keyframes[0].1.clone(),
            Err(idx) if idx == self.keyframes.len() => {
                self.keyframes[self.keyframes.len() - 1].1.clone()
            }
            Err(idx) => {
                let (f0, v0, interp) = &self.keyframes[idx - 1];
                let (f1, v1, _) = &self.keyframes[idx];
                let t = (frame - f0) as f32 / (f1 - f0) as f32;
                match interp {
                    Interpolation::Hold => v0.clone(),
                    Interpolation::Linear => T::lerp(v0, v1, t),
                    Interpolation::EaseInOut => T::lerp(v0, v1, smoothstep(t)),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Transform {
    /// Rettangolo di crop in coordinate normalizzate [0,1] sul frame sorgente.
    pub crop: [f32; 4], // left, top, right, bottom
    pub zoom: f32,
    pub position: [f32; 2],
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            crop: [0.0, 0.0, 1.0, 1.0],
            zoom: 1.0,
            position: [0.0, 0.0],
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Lerp for Rgba {
    fn lerp(a: &Self, b: &Self, t: f32) -> Self {
        Self {
            r: f32::lerp(&a.r, &b.r, t),
            g: f32::lerp(&a.g, &b.g, t),
            b: f32::lerp(&a.b, &b.b, t),
            a: f32::lerp(&a.a, &b.a, t),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextOverlay {
    pub content: String,
    pub font_size: f32,
    pub color: Rgba,
    pub transform: Keyframed<Transform>,
    pub opacity: Keyframed<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipSource {
    Media(MediaId),
    SolidColor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectStack {
    pub transform: Keyframed<Transform>,
    pub speed: Keyframed<f32>,
    pub gain_db: Keyframed<f32>,
    pub text: Vec<TextOverlay>,
    pub color: Option<Keyframed<Rgba>>,
}

impl Default for EffectStack {
    fn default() -> Self {
        Self {
            transform: Keyframed::constant(Transform::default()),
            speed: Keyframed::constant(1.0),
            gain_db: Keyframed::constant(0.0),
            text: Vec::new(),
            color: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub source: ClipSource,
    /// In/out nello spazio del frame sorgente (fps nativo del media).
    pub source_in: FrameIdx,
    pub source_out: FrameIdx,
    /// Posizione nello spazio della Timeline che contiene questa clip.
    pub timeline_start: FrameIdx,
    pub effects: EffectStack,
    /// Clip "gemella" (tipicamente audio<->video dello stesso media,
    /// collegate di default all'import): un drag nella timeline le muove
    /// insieme. Il collegamento è simmetrico: se `a.linked == Some(b)`
    /// allora `b.linked == Some(a)`. `None` per una clip indipendente.
    pub linked: Option<ClipId>,
}

impl Clip {
    pub fn timeline_len(&self) -> FrameIdx {
        self.source_out - self.source_in
    }

    pub fn timeline_end(&self) -> FrameIdx {
        self.timeline_start + self.timeline_len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    /// Sempre ordinate per `timeline_start`, mai sovrapposte.
    pub clips: Vec<Clip>,
    pub muted: bool,
}

impl Track {
    pub fn new(kind: TrackKind) -> Self {
        Self {
            kind,
            clips: Vec::new(),
            muted: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub name: String,
    pub fps: Rational,
    pub resolution: (u32, u32),
    /// Ordine di compositing: bottom -> top.
    pub tracks: Vec<Track>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Project {
    pub media_pool: SlotMap<MediaId, MediaItem>,
    pub timelines: SlotMap<TimelineId, Timeline>,
    next_clip_id: u64,
}

impl Project {
    pub fn alloc_clip_id(&mut self) -> ClipId {
        let id = ClipId(self.next_clip_id);
        self.next_clip_id += 1;
        id
    }
}

#[cfg(test)]
mod keyframe_tests {
    use super::*;

    #[test]
    fn value_at_returns_default_when_no_keyframes() {
        let k: Keyframed<f32> = Keyframed::constant(5.0);
        assert_eq!(k.value_at(0), 5.0);
        assert_eq!(k.value_at(1000), 5.0);
    }

    #[test]
    fn value_at_holds_extremes_before_first_and_after_last() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        k.upsert(10, 100.0, Interpolation::Linear);
        k.upsert(20, 200.0, Interpolation::Linear);

        assert_eq!(k.value_at(0), 100.0, "prima del primo: valore del primo");
        assert_eq!(k.value_at(30), 200.0, "dopo l'ultimo: valore dell'ultimo");
    }

    #[test]
    fn value_at_interpolates_linearly_between_two_keyframes() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        k.upsert(0, 0.0, Interpolation::Linear);
        k.upsert(10, 100.0, Interpolation::Linear);

        assert_eq!(k.value_at(0), 0.0);
        assert_eq!(k.value_at(5), 50.0);
        assert_eq!(k.value_at(10), 100.0);
    }

    #[test]
    fn value_at_hold_steps_instead_of_interpolating() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        k.upsert(0, 0.0, Interpolation::Hold);
        k.upsert(10, 100.0, Interpolation::Hold);

        assert_eq!(k.value_at(5), 0.0, "Hold mantiene il valore di partenza");
        assert_eq!(k.value_at(9), 0.0);
        assert_eq!(
            k.value_at(10),
            100.0,
            "sul keyframe stesso vale il suo valore"
        );
    }

    #[test]
    fn value_at_ease_in_out_matches_endpoints_and_stays_monotonic() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        k.upsert(0, 0.0, Interpolation::EaseInOut);
        k.upsert(10, 100.0, Interpolation::EaseInOut);

        assert_eq!(k.value_at(0), 0.0);
        assert_eq!(k.value_at(10), 100.0);
        let mid = k.value_at(5);
        assert!((0.0..=100.0).contains(&mid));
        // Monotonicità: valori crescenti col tempo.
        let mut last = k.value_at(0);
        for f in 1..=10 {
            let v = k.value_at(f);
            assert!(v >= last, "value_at deve crescere monotonamente");
            last = v;
        }
    }

    #[test]
    fn upsert_replaces_existing_keyframe_at_same_frame() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        k.upsert(5, 1.0, Interpolation::Linear);
        k.upsert(5, 2.0, Interpolation::Hold);

        assert_eq!(k.keyframes().len(), 1);
        assert_eq!(k.keyframe_at(5), Some((2.0, Interpolation::Hold)));
    }

    #[test]
    fn remove_at_deletes_and_returns_the_keyframe() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        k.upsert(5, 42.0, Interpolation::Linear);

        let removed = k.remove_at(5);
        assert_eq!(removed, Some((42.0, Interpolation::Linear)));
        assert!(k.is_constant());
        assert_eq!(
            k.remove_at(5),
            None,
            "rimuovere due volte non deve fare nulla"
        );
    }

    #[test]
    fn transform_lerp_interpolates_each_field() {
        let a = Transform {
            crop: [0.0, 0.0, 1.0, 1.0],
            zoom: 1.0,
            position: [0.0, 0.0],
        };
        let b = Transform {
            crop: [0.2, 0.2, 0.8, 0.8],
            zoom: 3.0,
            position: [1.0, -1.0],
        };
        let mid = Transform::lerp(&a, &b, 0.5);
        assert_eq!(mid.crop, [0.1, 0.1, 0.9, 0.9]);
        assert_eq!(mid.zoom, 2.0);
        assert_eq!(mid.position, [0.5, -0.5]);
    }

    #[test]
    fn rgba_lerp_interpolates_each_channel() {
        let a = Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        };
        let b = Rgba {
            r: 1.0,
            g: 0.5,
            b: 0.2,
            a: 0.0,
        };
        let mid = Rgba::lerp(&a, &b, 0.5);
        assert_eq!((mid.r, mid.g, mid.b, mid.a), (0.5, 0.25, 0.1, 0.5));
    }
}
