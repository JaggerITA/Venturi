//! Modello dati del progetto. Nessun grafo a nodi: ogni Clip ha uno
//! `EffectStack` fisso. Le trasformazioni sono valutate a runtime a partire
//! dai keyframe, mai "bake-ate" nei frame cachati (vedi ARCHITECTURE.md).

use serde::{Deserialize, Serialize};
use slotmap::{new_key_type, SlotMap};
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
