//! Data model of the project. No node graph: every Clip has a fixed
//! `EffectStack`. The transforms are evaluated at runtime from
//! the keyframes, never "baked" into the cached frames (see ARCHITECTURE.md).

use serde::{Deserialize, Serialize};
use slotmap::{SlotMap, new_key_type};
use std::path::PathBuf;

new_key_type! {
    pub struct MediaId;
    pub struct TimelineId;
}

/// Id of a group of linked clips: a counter, membership is only
/// the `Clip::linked_group` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkGroupId(pub u64);

/// Counter: the clips live in ordered `Vec`s inside the tracks, not in
/// an arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClipId(pub u64);

/// Frame rate as an exact fraction (e.g. 30000/1001 for 29.97fps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

impl Rational {
    pub const fn new(num: i32, den: i32) -> Self {
        Self { num, den }
    }

    pub const fn one() -> Self {
        Self { num: 1, den: 1 }
    }

    /// From a floating point fps (as in OTIO): recognizes the NTSC fps
    /// (`n * 1000/1001`), otherwise approximates to the thousandth.
    pub fn from_fps(fps: f64) -> Self {
        if !(fps.is_finite() && fps > 0.0) {
            return Self::new(30, 1);
        }
        let whole = fps.round();
        if (fps - whole).abs() < 1e-3 {
            return Self::new(whole as i32, 1);
        }
        let ntsc = (fps * 1.001).round();
        if (fps - ntsc * 1000.0 / 1001.0).abs() < 1e-3 {
            return Self::new(ntsc as i32 * 1000, 1001);
        }
        let num = (fps * 1000.0).round() as i64;
        let g = gcd(num, 1000);
        Self::new((num / g) as i32, (1000 / g) as i32)
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    pub fn is_one(self) -> bool {
        self.den != 0 && self.num == self.den
    }

    /// Timeline frames per source frame of a media at `media_fps` on
    /// a timeline at `timeline_fps`, reduced to lowest terms (see
    /// `Clip::rate`).
    pub fn conform_rate(timeline_fps: Rational, media_fps: Rational) -> Self {
        let mut num = timeline_fps.num as i64 * media_fps.den as i64;
        let mut den = timeline_fps.den as i64 * media_fps.num as i64;
        if num <= 0 || den <= 0 {
            return Self::one();
        }
        let g = gcd(num, den);
        num /= g;
        den /= g;
        // An irreducible ratio that does not fit in i32 (exotic fps on
        // both sides) is approximated: better a millionth of
        // error than an overflow.
        if num > i32::MAX as i64 || den > i32::MAX as i64 {
            let approx = (num as f64 / den as f64 * 1_000_000.0).round() as i64;
            return Self::new(approx.clamp(1, i32::MAX as i64) as i32, 1_000_000);
        }
        Self::new(num as i32, den as i32)
    }

    /// `round(frames * self)`, halves up. Exact identity for
    /// `1/1`, so an unconformed clip stays bit-for-bit as before.
    pub fn scale_round(self, frames: FrameIdx) -> FrameIdx {
        if self.is_one() || self.num <= 0 || self.den <= 0 {
            return frames;
        }
        let (num, den) = (self.num as i128, self.den as i128);
        let v = frames as i128;
        ((2 * v * num + den).div_euclid(2 * den)) as FrameIdx
    }

    /// The largest `n` such that `scale_round(n) <= scaled`: the inverse of
    /// `scale_round`, i.e. "which source frame covers this position".
    pub fn unscale_round(self, scaled: FrameIdx) -> FrameIdx {
        if self.is_one() || self.num <= 0 || self.den <= 0 {
            return scaled;
        }
        let (num, den) = (self.num as i128, self.den as i128);
        let a = (2 * scaled as i128 + 1) * den;
        let b = 2 * num;
        (-((-a).div_euclid(b)) - 1) as FrameIdx // ceil(a/b) - 1
    }
}

fn gcd(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

/// YUV→RGB matrix of a decoded frame. BT.2020 only if the source
/// signals it: never guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Frame index, always relative to the context it is used in: source
/// frame of a media (native fps) or Timeline frame (fps of the
/// Timeline containing it). The two spaces must never be confused.
pub type FrameIdx = i64;

/// `duration_frames` of an image: ~463 days at `IMAGE_FPS`, no real
/// duration reaches it, so it acts as an "it is an image" marker without an
/// extra field and does not limit the trim.
pub const IMAGE_DURATION_FRAMES: FrameIdx = 1_000_000_000;

/// Name prefix of the compound clips in the media pool.
pub const COMPOUND_NAME_PREFIX: &str = "Compound Clip ";
pub const TIMELINE_NAME_PREFIX: &str = "Timeline ";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMeta {
    pub duration_frames: FrameIdx,
    pub fps: Rational,
    /// `width`/`height` at zero and nominal `fps` if `false`: an audio-only
    /// media goes only on audio tracks.
    pub width: u32,
    pub height: u32,
    pub has_video: bool,
    pub has_audio: bool,
    pub sample_rate: u32,
    pub channels: u16,
    /// Audio streams in the container. 0 in projects saved before the
    /// field existed: the app recomputes it on opening.
    #[serde(default)]
    pub audio_streams: u16,
}

impl MediaMeta {
    /// Audio streams to use: at least one if the media has audio.
    pub fn audio_stream_count(&self) -> usize {
        if self.has_audio {
            usize::from(self.audio_streams).max(1)
        } else {
            0
        }
    }

    /// A still image imported into the pool: it has video but no audio, and
    /// `duration_frames` is the `IMAGE_DURATION_FRAMES` sentinel (see its
    /// docs on why a dedicated field is not needed).
    pub fn is_image(&self) -> bool {
        self.has_video && !self.has_audio && self.duration_frames == IMAGE_DURATION_FRAMES
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    /// For a compound clip (`compound.is_some()`) it is only the name
    /// shown in the media pool ("Compound Clip N"), not a real file.
    pub path: PathBuf,
    pub meta: MediaMeta,
    /// Stable key for the frame cache and the proxies: hash of the content
    /// (not of the path), so moving/renaming the file invalidates nothing.
    /// For a compound clip, it changes every time its nested timeline
    /// changes (see `Project::touch_compound`): it invalidates the cache
    /// of composited frames without having to compare the whole timeline.
    pub content_hash: u64,
    /// `Some` if this item is a compound clip: its content is
    /// `Project::timelines[_]` instead of a file on disk. `meta` stays
    /// valid anyway (recomputed by `Project::sync_compound_meta`), so
    /// the rest of the program (probe, drag&drop, duration on the timeline) can
    /// treat it like any other media.
    #[serde(default)]
    pub compound: Option<TimelineId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Interpolation {
    Hold,
    Linear,
    EaseInOut,
    EaseIn,
    EaseOut,
    /// Free curve: control points of a cubic bezier normalized
    /// on the segment (from `(0,0)` to `(1,1)`), like CSS's `cubic-bezier`.
    Bezier { c1: [f32; 2], c2: [f32; 2] },
}

impl Interpolation {
    /// The presets offered by the keyframe editor, in bar order.
    pub const PRESETS: [Self; 5] = [
        Self::Hold,
        Self::Linear,
        Self::EaseInOut,
        Self::EaseIn,
        Self::EaseOut,
    ];

    /// Weight of the arrival keyframe at `t` (0 = the starting one).
    pub fn ease(self, t: f32) -> f32 {
        match self {
            Self::Hold => 0.0,
            Self::Linear => t,
            Self::EaseInOut => smoothstep(t),
            Self::EaseIn => t * t,
            Self::EaseOut => t * (2.0 - t),
            Self::Bezier { c1, c2 } => bezier_ease(c1, c2, t),
        }
    }

    /// The equivalent control points, for the editor handles: `Hold`
    /// has no curve to manipulate.
    pub fn control_points(self) -> Option<([f32; 2], [f32; 2])> {
        match self {
            Self::Hold => None,
            Self::Linear => Some(([1.0 / 3.0, 1.0 / 3.0], [2.0 / 3.0, 2.0 / 3.0])),
            Self::EaseInOut => Some(([0.5, 0.0], [0.5, 1.0])),
            Self::EaseIn => Some(([0.42, 0.0], [1.0, 1.0])),
            Self::EaseOut => Some(([0.0, 0.0], [0.58, 1.0])),
            Self::Bezier { c1, c2 } => Some((c1, c2)),
        }
    }
}

/// The `x` of a normalized cubic bezier is not `t`: it is inverted by
/// bisection (monotonic as long as the control abscissas stay in `0..=1`).
fn bezier_ease(c1: [f32; 2], c2: [f32; 2], x: f32) -> f32 {
    let axis = |a: f32, b: f32, t: f32| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        if axis(c1[0], c2[0], mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    axis(c1[1], c2[1], 0.5 * (lo + hi))
}

/// Parameter animatable via keyframes. Always sorted by increasing time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframed<T> {
    /// If empty, the parameter is constant and must be read from `default`.
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

    /// Inserts or replaces the keyframe at `frame`, preserving the order.
    pub fn upsert(&mut self, frame: FrameIdx, value: T, interpolation: Interpolation) {
        match self.keyframes.binary_search_by_key(&frame, |(f, _, _)| *f) {
            Ok(idx) => self.keyframes[idx] = (frame, value, interpolation),
            Err(idx) => self.keyframes.insert(idx, (frame, value, interpolation)),
        }
    }

    /// Removes the keyframe exactly at `frame`, if it exists. Returns the
    /// removed value (useful for the undo).
    pub fn remove_at(&mut self, frame: FrameIdx) -> Option<(T, Interpolation)> {
        let idx = self
            .keyframes
            .binary_search_by_key(&frame, |(f, _, _)| *f)
            .ok()?;
        let (_, value, interp) = self.keyframes.remove(idx);
        Some((value, interp))
    }

    /// Frame of the nearest keyframe before `frame`.
    pub fn keyframe_before(&self, frame: FrameIdx) -> Option<FrameIdx> {
        self.keyframes.iter().rev().map(|k| k.0).find(|&f| f < frame)
    }

    /// Frame of the nearest keyframe after `frame`.
    pub fn keyframe_after(&self, frame: FrameIdx) -> Option<FrameIdx> {
        self.keyframes.iter().map(|k| k.0).find(|&f| f > frame)
    }

    /// Changes the outgoing interpolation of the keyframe at `frame`, if there is one, and
    /// returns the one it had.
    pub fn set_interpolation(
        &mut self,
        frame: FrameIdx,
        interpolation: Interpolation,
    ) -> Option<Interpolation> {
        let idx = self
            .keyframes
            .binary_search_by_key(&frame, |(f, _, _)| *f)
            .ok()?;
        Some(std::mem::replace(&mut self.keyframes[idx].2, interpolation))
    }

    /// The keyframe exactly at `frame`, if it exists.
    pub fn keyframe_at(&self, frame: FrameIdx) -> Option<(T, Interpolation)> {
        let idx = self
            .keyframes
            .binary_search_by_key(&frame, |(f, _, _)| *f)
            .ok()?;
        let (_, value, interp) = &self.keyframes[idx];
        Some((value.clone(), *interp))
    }
}

/// Component-wise linear interpolation between two values of an
/// animatable parameter. `Keyframed::value_at` needs it to compute
/// the value at an arbitrary frame between two keyframes.
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
            crop_softness: f32::lerp(&a.crop_softness, &b.crop_softness, t),
            opacity: f32::lerp(&a.opacity, &b.opacity, t),
            zoom: [
                f32::lerp(&a.zoom[0], &b.zoom[0], t),
                f32::lerp(&a.zoom[1], &b.zoom[1], t),
            ],
            position: [
                f32::lerp(&a.position[0], &b.position[0], t),
                f32::lerp(&a.position[1], &b.position[1], t),
            ],
            rotation: f32::lerp(&a.rotation, &b.rotation, t),
            anchor: [
                f32::lerp(&a.anchor[0], &b.anchor[0], t),
                f32::lerp(&a.anchor[1], &b.anchor[1], t),
            ],
            // A flip has no middle ground: it snaps at half the interpolation.
            flip: if t < 0.5 { a.flip } else { b.flip },
        }
    }
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

impl<T: Lerp + Clone> Keyframed<T> {
    /// `default` without keyframes; before the first and after the last the extreme
    /// value; in between the interpolation of the starting keyframe.
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
                if interp == &Interpolation::Hold {
                    v0.clone()
                } else {
                    T::lerp(v0, v1, interp.ease(t))
                }
            }
        }
    }

    /// Discards the keyframes before `start` preserving the value from `start`
    /// on: a joining keyframe remains only if the animation depends on it.
    pub fn drop_before(&mut self, start: FrameIdx)
    where
        T: PartialEq,
    {
        let Some(lead) = self.keyframes.iter().rev().find(|(f, _, _)| *f < start) else {
            return;
        };
        let lead_interp = lead.2;
        let at_start = self.value_at(start);
        self.keyframes.retain(|(f, _, _)| *f >= start);
        if self.keyframes.is_empty() {
            self.default = at_start;
        } else if self.value_at(start) != at_start {
            self.upsert(start, at_start, lead_interp);
        }
    }

    /// Mirror of `drop_before`: keeps only the keyframes before `end`.
    pub fn drop_from(&mut self, end: FrameIdx)
    where
        T: PartialEq,
    {
        if self.keyframes.last().is_none_or(|(f, _, _)| *f < end) {
            return;
        }
        let last = end - 1;
        let at_last = self.value_at(last);
        self.keyframes.retain(|(f, _, _)| *f < end);
        if self.keyframes.is_empty() {
            self.default = at_last;
        } else if self.value_at(last) != at_last {
            self.upsert(last, at_last, Interpolation::Linear);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Transform {
    /// Pixels cut per side (left, top, right, bottom) at the native
    /// resolution of the media, not of the proxy. The rest is not recentered.
    pub crop: [f32; 4], // left, top, right, bottom
    /// Softness of the crop edge in media pixels: negative towards the inside,
    /// positive towards the outside, 0 hard.
    pub crop_softness: f32,
    /// Magnification per axis around the `anchor`, relative to the output frame.
    pub zoom: [f32; 2],
    /// Displacement in timeline pixels, Y upwards.
    pub position: [f32; 2],
    /// Rotation in degrees, clockwise, around the `anchor`.
    pub rotation: f32,
    /// Zoom and rotation pivot, in timeline pixels from the center
    /// of the clip (`[0, 0]` = its center), with the same directions as
    /// `position` (Y positive upwards).
    pub anchor: [f32; 2],
    /// Horizontal (X) and vertical (Y) mirroring.
    pub flip: [bool; 2],
    /// Layer opacity as a percentage, 0-100.
    pub opacity: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            crop: [0.0; 4],
            crop_softness: 0.0,
            zoom: [1.0, 1.0],
            position: [0.0, 0.0],
            rotation: 0.0,
            anchor: [0.0, 0.0],
            flip: [false, false],
            opacity: 100.0,
        }
    }
}

/// A transform parameter, each with its own keyframes. `flip` is not one: it
/// does not interpolate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransformParam {
    ZoomX,
    ZoomY,
    PositionX,
    PositionY,
    Rotation,
    AnchorX,
    AnchorY,
    CropLeft,
    CropTop,
    CropRight,
    CropBottom,
    CropSoftness,
    Opacity,
}

impl TransformParam {
    pub const ALL: [Self; 13] = [
        Self::ZoomX,
        Self::ZoomY,
        Self::PositionX,
        Self::PositionY,
        Self::Rotation,
        Self::AnchorX,
        Self::AnchorY,
        Self::CropLeft,
        Self::CropTop,
        Self::CropRight,
        Self::CropBottom,
        Self::CropSoftness,
        Self::Opacity,
    ];

    /// Position in `TransformTracks::params` — the order of `ALL`, which is
    /// the declaration order.
    pub fn index(self) -> usize {
        self as usize
    }

    /// The value it has in an already evaluated `Transform`.
    pub fn of(self, t: &Transform) -> f32 {
        match self {
            Self::ZoomX => t.zoom[0],
            Self::ZoomY => t.zoom[1],
            Self::PositionX => t.position[0],
            Self::PositionY => t.position[1],
            Self::Rotation => t.rotation,
            Self::AnchorX => t.anchor[0],
            Self::AnchorY => t.anchor[1],
            Self::CropLeft => t.crop[0],
            Self::CropTop => t.crop[1],
            Self::CropRight => t.crop[2],
            Self::CropBottom => t.crop[3],
            Self::CropSoftness => t.crop_softness,
            Self::Opacity => t.opacity,
        }
    }
}

/// The transform of a clip: one `Keyframed<f32>` per parameter, so every
/// parameter animates on its own, plus the flip (not animatable).
#[derive(Debug, Clone, Serialize)]
pub struct TransformTracks {
    /// One per `TransformParam`, in the order of `TransformParam::ALL`.
    params: Vec<Keyframed<f32>>,
    pub flip: [bool; 2],
}

/// Projects saved before a parameter existed have fewer tracks than
/// `TransformParam::ALL`: the missing tail takes the default value.
impl<'de> Deserialize<'de> for TransformTracks {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Repr {
            params: Vec<Keyframed<f32>>,
            flip: [bool; 2],
        }
        let mut repr = Repr::deserialize(deserializer)?;
        let default = Transform::default();
        for p in TransformParam::ALL.iter().skip(repr.params.len()) {
            repr.params.push(Keyframed::constant(p.of(&default)));
        }
        Ok(Self {
            params: repr.params,
            flip: repr.flip,
        })
    }
}

impl Default for TransformTracks {
    fn default() -> Self {
        Self::constant(Transform::default())
    }
}

impl TransformTracks {
    pub fn constant(t: Transform) -> Self {
        Self {
            params: TransformParam::ALL
                .iter()
                .map(|p| Keyframed::constant(p.of(&t)))
                .collect(),
            flip: t.flip,
        }
    }

    pub fn track(&self, param: TransformParam) -> &Keyframed<f32> {
        &self.params[param.index()]
    }

    pub fn track_mut(&mut self, param: TransformParam) -> &mut Keyframed<f32> {
        &mut self.params[param.index()]
    }

    /// `true` if no parameter has keyframes.
    pub fn is_constant(&self) -> bool {
        self.params.iter().all(|k| k.is_constant())
    }

    /// `true` if the transform is still the default one, keyframes included.
    pub fn is_pristine(&self) -> bool {
        let d = Transform::default();
        self.flip == d.flip
            && TransformParam::ALL
                .iter()
                .all(|p| self.track(*p).is_constant() && self.track(*p).default == p.of(&d))
    }

    pub fn value_at(&self, frame: FrameIdx) -> Transform {
        let v = |p: TransformParam| self.track(p).value_at(frame);
        Transform {
            crop: [
                v(TransformParam::CropLeft),
                v(TransformParam::CropTop),
                v(TransformParam::CropRight),
                v(TransformParam::CropBottom),
            ],
            crop_softness: v(TransformParam::CropSoftness),
            opacity: v(TransformParam::Opacity),
            zoom: [v(TransformParam::ZoomX), v(TransformParam::ZoomY)],
            position: [v(TransformParam::PositionX), v(TransformParam::PositionY)],
            rotation: v(TransformParam::Rotation),
            anchor: [v(TransformParam::AnchorX), v(TransformParam::AnchorY)],
            flip: self.flip,
        }
    }

    /// The keyframe nearest to `frame` *before* it, among those of the
    /// given parameters: used by the panel's navigation arrows.
    pub fn previous_keyframe(&self, params: &[TransformParam], frame: FrameIdx) -> Option<FrameIdx> {
        params
            .iter()
            .filter_map(|p| self.track(*p).keyframe_before(frame))
            .max()
    }

    /// Mirror of `previous_keyframe`, forwards.
    pub fn next_keyframe(&self, params: &[TransformParam], frame: FrameIdx) -> Option<FrameIdx> {
        params
            .iter()
            .filter_map(|p| self.track(*p).keyframe_after(frame))
            .min()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const BLACK: Self = Self::gray(0.0);
    pub const WHITE: Self = Self::gray(1.0);

    /// Opaque grey.
    pub const fn gray(v: f32) -> Self {
        Self {
            r: v,
            g: v,
            b: v,
            a: 1.0,
        }
    }
}

impl From<[f32; 4]> for Rgba {
    fn from([r, g, b, a]: [f32; 4]) -> Self {
        Self { r, g, b, a }
    }
}

impl From<Rgba> for [f32; 4] {
    fn from(c: Rgba) -> Self {
        [c.r, c.g, c.b, c.a]
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextAlign {
    Left,
    Center,
    Right,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HAnchor {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VAnchor {
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FontCase {
    Mixed,
    Upper,
    Lower,
    Title,
}

/// Parameters of a `ClipSource::Text` clip. The measures are in timeline
/// pixels, like those of the `Transform`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleParams {
    pub content: String,
    /// Empty = system sans-serif.
    pub font_family: String,
    /// 100-900, as in CSS.
    pub font_weight: u16,
    pub italic: bool,
    pub color: Rgba,
    pub size: f32,
    /// Letter spacing, in thousandths of an em.
    pub tracking: f32,
    /// Extra space between lines, in pixels.
    pub line_spacing: f32,
    pub underline: bool,
    pub strikethrough: bool,
    pub case: FontCase,
    pub align: TextAlign,
    /// Which point of the text block falls on `position`.
    pub anchor: (HAnchor, VAnchor),
    /// From the center of the frame, Y upwards.
    pub position: [f32; 2],
    #[serde(default)]
    pub shadow: TitleShadow,
    #[serde(default)]
    pub background: TitleBackground,
}

/// Shadow of the text alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleShadow {
    pub enabled: bool,
    pub color: Rgba,
    /// In timeline pixels, Y upwards.
    pub offset: [f32; 2],
    /// Blur radius, in timeline pixels.
    pub blur: f32,
    /// 0-100.
    pub opacity: f32,
}

impl Default for TitleShadow {
    fn default() -> Self {
        Self {
            enabled: false,
            color: Rgba::BLACK,
            offset: [8.0, -8.0],
            blur: 6.0,
            opacity: 75.0,
        }
    }
}

/// Rectangle behind the text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleBackground {
    pub enabled: bool,
    pub color: Rgba,
    pub outline_color: Rgba,
    /// In timeline pixels, towards the inside of the rectangle.
    pub outline_width: f32,
    /// Fraction of the width/height of the frame; 0 = around the text.
    pub width: f32,
    pub height: f32,
    /// Fraction of the shorter side of the rectangle, up to 0.5.
    pub corner_radius: f32,
    /// Displacement from the center of the text, in timeline pixels, Y upwards.
    pub center: [f32; 2],
    /// 0-100.
    pub opacity: f32,
}

impl Default for TitleBackground {
    fn default() -> Self {
        Self {
            enabled: false,
            color: Rgba::BLACK,
            outline_color: Rgba::WHITE,
            outline_width: 0.0,
            width: 0.0,
            height: 0.0,
            corner_radius: 0.1,
            center: [0.0, 0.0],
            opacity: 100.0,
        }
    }
}

impl Default for TitleParams {
    fn default() -> Self {
        Self {
            content: "Basic Title".into(),
            font_family: String::new(),
            font_weight: 400,
            italic: false,
            color: Rgba::WHITE,
            size: 96.0,
            tracking: 0.0,
            line_spacing: 0.0,
            underline: false,
            strikethrough: false,
            case: FontCase::Mixed,
            align: TextAlign::Center,
            anchor: (HAnchor::Center, VAnchor::Middle),
            position: [0.0, 0.0],
            shadow: TitleShadow::default(),
            background: TitleBackground::default(),
        }
    }
}

impl TitleParams {
    /// The text to draw, with `case` already applied.
    pub fn display_text(&self) -> String {
        match self.case {
            FontCase::Mixed => self.content.clone(),
            FontCase::Upper => self.content.to_uppercase(),
            FontCase::Lower => self.content.to_lowercase(),
            FontCase::Title => {
                let mut out = String::with_capacity(self.content.len());
                let mut word_start = true;
                for c in self.content.chars() {
                    if word_start {
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                    word_start = c.is_whitespace();
                }
                out
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipSource {
    Media(MediaId),
    SolidColor,
    /// Parameters in `EffectStack::title`.
    Text,
}

/// Bounds of `gain_db`: shared between the properties panel slider and the
/// volume line on the timeline, so they always stay a single one.
pub const GAIN_DB_MIN: f32 = -100.0;
pub const GAIN_DB_MAX: f32 = 30.0;

/// A filter of the Effects panel: the variety is open (new variants for
/// new filters), the rendering translates it into a shader id — see
/// `vv_render`, which does not know the meaning of each one, only its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterKind {
    Grayscale,
}

/// A filter applied to a clip. The order in the `Vec` of
/// `EffectStack::filters` is the order of application, configurable
/// by the user (several filters on the same clip, in a sequence they choose);
/// `enabled` suspends it without removing it from the sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipFilter {
    pub kind: FilterKind,
    pub enabled: bool,
}

/// A transition of the Effects panel, "Transitions" section: like
/// `FilterKind`, an open variety for future transitions. Unlike the
/// filters, it applies only to one edge of the clip (`EffectStack::transition_in`
/// or `transition_out`), not to the whole clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionKind {
    Push,
}

/// Slide direction of a Push transition. Independent of the clip edge
/// it is attached to (`In`/`Out`): it describes only the direction of the
/// movement on screen during the transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushDirection {
    Left,
    Right,
    Up,
    Down,
}

impl PushDirection {
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    /// Direction of the movement in `Transform.position` units (X, Y): `Y`
    /// positive upwards, like the rest of the transform.
    fn vector(self) -> [f32; 2] {
        match self {
            Self::Left => [-1.0, 0.0],
            Self::Right => [1.0, 0.0],
            Self::Up => [0.0, 1.0],
            Self::Down => [0.0, -1.0],
        }
    }
}

/// What "off screen" really means for a clip zoomed `zoom` times
/// on the axis of `vec` (`direction` is always axial, so only one of the
/// two components matters). In the shader the zoom is applied *after* the
/// position (see `transform.wgsl`): the center of the image moves by
/// `position` on screen whatever the zoom, but its real edge is
/// `zoom` times farther from the center — at `push_clearance == 1` (zoom 1)
/// one unit of push is enough to clear the whole screen, at a higher zoom
/// more is needed or one would still see the inside of the image instead of the
/// transparent underneath for the whole transition. It does not account for
/// anchor/rotation: a case rare enough not to justify the exact
/// computation, here clearing the screen for the zoom (the common case) is enough.
fn push_clearance(vec: [f32; 2], zoom: [f32; 2]) -> f32 {
    let z = vec[0].abs() * zoom[0] + vec[1].abs() * zoom[1];
    0.5 * (z + 1.0)
}

/// Acceleration curve of a transition, applied to the 0..1 progression
/// before translating it into an offset. The same four options as an NLE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ease {
    None,
    In,
    Out,
    InOut,
}

impl Ease {
    pub const ALL: [Self; 4] = [Self::None, Self::In, Self::Out, Self::InOut];
}

/// `t` (0..1) reshaped according to `ease`; `curve` (0..1, "Transition Curve"
/// in the inspector) controls its intensity: 0 nearly linear, 1 more
/// pronounced. No claim to match the exact curve of a specific
/// NLE, only a monotonic progression, symmetric in InOut.
fn eased(t: f32, ease: Ease, curve: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let exponent = 1.0 + curve.clamp(0.0, 1.0) * 4.0;
    match ease {
        Ease::None => t,
        Ease::In => t.powf(exponent),
        Ease::Out => 1.0 - (1.0 - t).powf(exponent),
        Ease::InOut => {
            if t < 0.5 {
                0.5 * (2.0 * t).powf(exponent)
            } else {
                1.0 - 0.5 * (2.0 * (1.0 - t)).powf(exponent)
            }
        }
    }
}

/// A transition applied to one edge of a clip (see
/// `EffectStack::transition_in`/`transition_out`). `duration` in timeline
/// frames from the edge, like `Clip::fade_in`/`fade_out`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration: FrameIdx,
    pub direction: PushDirection,
    pub ease: Ease,
    /// "Transition Curve" in the inspector, 0..1.
    pub curve: f32,
}

/// Compositing method of a layer onto those below ("Composite Mode"
/// in the inspector). Separable modes only, computed channel by channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BlendMode {
    #[default]
    Normal,
    Add,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    ColorDodge,
    ColorBurn,
    HardLight,
    SoftLight,
    Difference,
    Exclusion,
    Subtract,
    Divide,
}

impl BlendMode {
    pub const ALL: [Self; 15] = [
        Self::Normal,
        Self::Add,
        Self::Multiply,
        Self::Screen,
        Self::Overlay,
        Self::Darken,
        Self::Lighten,
        Self::ColorDodge,
        Self::ColorBurn,
        Self::HardLight,
        Self::SoftLight,
        Self::Difference,
        Self::Exclusion,
        Self::Subtract,
        Self::Divide,
    ];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectStack {
    pub transform: TransformTracks,
    pub speed: Keyframed<f32>,
    pub gain_db: Keyframed<f32>,
    pub color: Option<Keyframed<Rgba>>,
    #[serde(default)]
    pub title: Option<TitleParams>,
    #[serde(default)]
    pub filters: Vec<ClipFilter>,
    /// Transition attached to the starting/ending edge of the clip, from the
    /// Effects panel. A pair of fields like `Clip::fade_in`/`fade_out`,
    /// not a map: at most one per edge.
    #[serde(default)]
    pub transition_in: Option<Transition>,
    #[serde(default)]
    pub transition_out: Option<Transition>,
    #[serde(default)]
    pub blend_mode: BlendMode,
}

impl Default for EffectStack {
    fn default() -> Self {
        Self {
            transform: TransformTracks::default(),
            speed: Keyframed::constant(1.0),
            gain_db: Keyframed::constant(0.0),
            color: None,
            title: None,
            filters: Vec::new(),
            transition_in: None,
            transition_out: None,
            blend_mode: BlendMode::default(),
        }
    }
}

impl EffectStack {
    /// See `Keyframed::drop_before`, on every animatable parameter.
    pub fn drop_keyframes_before(&mut self, start: FrameIdx) {
        self.for_each_f32_track(|k| k.drop_before(start));
        if let Some(c) = &mut self.color {
            c.drop_before(start);
        }
    }

    /// See `Keyframed::drop_from`, on every animatable parameter.
    pub fn drop_keyframes_from(&mut self, end: FrameIdx) {
        self.for_each_f32_track(|k| k.drop_from(end));
        if let Some(c) = &mut self.color {
            c.drop_from(end);
        }
    }

    fn for_each_f32_track(&mut self, mut f: impl FnMut(&mut Keyframed<f32>)) {
        for p in TransformParam::ALL {
            f(self.transform.track_mut(p));
        }
        f(&mut self.speed);
        f(&mut self.gain_db);
    }

    /// `true` if no property was touched relative to the default: the
    /// timeline draws the clips for which it is `false` darker. The title does not
    /// count: it is the content of the clip, not an effect.
    pub fn is_pristine(&self) -> bool {
        self.transform.is_pristine()
            && self.speed.is_constant()
            && self.speed.default == 1.0
            && self.gain_db.is_constant()
            && self.gain_db.default == 0.0
            && self.color.is_none()
            && self.blend_mode == BlendMode::Normal
    }
}

/// Names and shades follow Resolve's clip colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipColor {
    Orange,
    Apricot,
    Yellow,
    Lime,
    Olive,
    Green,
    Teal,
    Navy,
    Blue,
    Purple,
    Violet,
    Pink,
    Tan,
    Beige,
    Brown,
    Chocolate,
}

impl ClipColor {
    pub const ALL: [ClipColor; 16] = [
        ClipColor::Orange,
        ClipColor::Apricot,
        ClipColor::Yellow,
        ClipColor::Lime,
        ClipColor::Olive,
        ClipColor::Green,
        ClipColor::Teal,
        ClipColor::Navy,
        ClipColor::Blue,
        ClipColor::Purple,
        ClipColor::Violet,
        ClipColor::Pink,
        ClipColor::Tan,
        ClipColor::Beige,
        ClipColor::Brown,
        ClipColor::Chocolate,
    ];

    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            ClipColor::Orange => (223, 129, 48),
            ClipColor::Apricot => (232, 176, 101),
            ClipColor::Yellow => (222, 202, 84),
            ClipColor::Lime => (168, 203, 86),
            ClipColor::Olive => (118, 160, 76),
            ClipColor::Green => (86, 175, 128),
            ClipColor::Teal => (74, 175, 175),
            ClipColor::Navy => (58, 112, 186),
            ClipColor::Blue => (76, 160, 214),
            ClipColor::Purple => (146, 122, 200),
            ClipColor::Violet => (178, 116, 190),
            ClipColor::Pink => (224, 134, 178),
            ClipColor::Tan => (196, 168, 142),
            ClipColor::Beige => (201, 185, 160),
            ClipColor::Brown => (150, 110, 80),
            ClipColor::Chocolate => (118, 86, 68),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub source: ClipSource,
    /// Start in the media in *timeline* frames from source frame 0: a conformed
    /// clip can start in the middle of a source frame.
    pub source_offset: FrameIdx,
    /// Position in the space of the Timeline containing this clip.
    pub timeline_start: FrameIdx,
    pub timeline_len: FrameIdx,
    pub effects: EffectStack,
    /// Linked group (usually the video and all the audio streams of one
    /// import): selection, drag and deletion treat it as a unit.
    #[serde(default)]
    pub linked_group: Option<LinkGroupId>,
    /// Audio clip: which stream of the container (order of
    /// `vv_media::audio_streams`).
    #[serde(default)]
    pub audio_stream_index: usize,
    /// Timeline frames per source frame (`Rational::conform_rate`).
    #[serde(default = "Rational::one")]
    pub rate: Rational,
    /// Excluded from compositing and mixing, but stays on the timeline.
    #[serde(default)]
    pub disabled: bool,
    /// Duration of the fade in, in timeline frames from the start
    /// of the clip. 0 = none.
    #[serde(default)]
    pub fade_in: FrameIdx,
    /// Duration of the fade out, in timeline frames from the end
    /// of the clip. 0 = none.
    #[serde(default)]
    pub fade_out: FrameIdx,
    /// Hand-picked timeline color; `None` = the one derived from the source kind.
    #[serde(default)]
    pub display_color: Option<ClipColor>,
}

impl Clip {
    /// Clip showing `source_in..source_out()` of the source starting from
    /// `timeline_start`, without effects or links.
    pub fn from_source_range(
        id: ClipId,
        source: ClipSource,
        source_in: FrameIdx,
        source_out: FrameIdx,
        timeline_start: FrameIdx,
        rate: Rational,
    ) -> Self {
        let source_offset = rate.scale_round(source_in);
        Self {
            id,
            source,
            source_offset,
            timeline_start,
            timeline_len: rate.scale_round(source_out) - source_offset,
            effects: EffectStack::default(),
            linked_group: None,
            audio_stream_index: 0,
            rate,
            disabled: false,
            fade_in: 0,
            fade_out: 0,
            display_color: None,
        }
    }

    /// First source frame shown.
    pub fn source_in(&self) -> FrameIdx {
        self.rate.unscale_round(self.source_offset)
    }

    /// Exclusive: the last source frame shown, plus one.
    pub fn source_out(&self) -> FrameIdx {
        self.rate.unscale_round(self.source_offset + self.timeline_len - 1) + 1
    }

    /// Duration in *source* frames, for the computations living in that
    /// space (how many frames of the media need decoding).
    pub fn source_len(&self) -> FrameIdx {
        self.source_out() - self.source_in()
    }

    pub fn timeline_end(&self) -> FrameIdx {
        self.timeline_start + self.timeline_len
    }

    /// `frame` (of the timeline) falls inside the clip.
    pub fn contains(&self, frame: FrameIdx) -> bool {
        frame >= self.timeline_start && frame < self.timeline_end()
    }

    /// Source frame shown at timeline position `timeline_frame`
    /// (inside the clip). The single point of the mapping, shared by preview
    /// and export: a future time-remap must be applied here.
    pub fn source_frame_at(&self, timeline_frame: FrameIdx) -> FrameIdx {
        self.rate
            .unscale_round(timeline_frame - self.timeline_start + self.source_offset)
    }

    /// Inverse of `source_frame_at`, even outside the trim (needed by the trim
    /// limits).
    pub fn timeline_frame_at(&self, source_frame: FrameIdx) -> FrameIdx {
        self.timeline_start + self.rate.scale_round(source_frame) - self.source_offset
    }

    /// Seconds from the start of the media at `timeline_frame`, without going through the
    /// source frames: the audio follows the cut to the sample.
    pub fn media_secs_at(&self, timeline_frame: FrameIdx, timeline_fps: f64) -> f64 {
        (timeline_frame - self.timeline_start + self.source_offset) as f64 / timeline_fps
    }

    /// Moves the clip from a timeline at `from` fps to one at `to` fps,
    /// preserving the seconds; `rate` is the clip's one on the new
    /// timeline.
    pub fn retime(&mut self, from: Rational, to: Rational, rate: Rational) {
        let end = convert_frames(self.timeline_end(), from, to);
        self.timeline_start = convert_frames(self.timeline_start, from, to);
        self.timeline_len = (end - self.timeline_start).max(1);
        self.source_offset = convert_frames(self.source_offset, from, to);
        self.rate = rate;
    }

    /// Opacity/volume multiplier at `timeline_frame` for the
    /// fades: linear ramp 0→1 over `fade_in` frames from the start,
    /// ramp 1→0 over `fade_out` frames from the end, product of the two (if they
    /// overlap they subtract from each other, as in any NLE).
    pub fn fade_multiplier_at(&self, timeline_frame: FrameIdx) -> f32 {
        let len = self.timeline_len.max(1);
        let fade_in = self.fade_in.clamp(0, len);
        let fade_out = self.fade_out.clamp(0, len);
        let pos = (timeline_frame - self.timeline_start).clamp(0, len);
        let in_ramp = if fade_in > 0 {
            (pos as f32 / fade_in as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let out_ramp = if fade_out > 0 {
            ((len - pos) as f32 / fade_out as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        in_ramp * out_ramp
    }

    /// Position offset (same pixel units as `Transform.position`,
    /// `frame_size` = resolution of the timeline) due to the
    /// push transitions in/out at `timeline_frame`. Added to the `position` already
    /// sampled from the transform keyframes, it does not replace it: this way a
    /// push transition coexists with a manual pan on the same clip. Outside
    /// the transition window the offset is zero; at its extremes it
    /// always coincides with "off screen" or "in place", independently
    /// of `direction`/`ease`, because the release gives alpha 0 outside the edges
    /// of the source_uv (see `transform.wgsl`): using it also to reveal
    /// what is below (lower third, overlay) is the same mechanism.
    /// `zoom` is the already sampled one of `effects.transform` at the same
    /// frame (see `push_clearance`): without it, a zoomed clip would be seen
    /// popping in halfway through the transition instead of sliding in from off
    /// screen, because its real edge stays past the displacement
    /// computed for zoom 1.
    pub fn transition_offset_at(&self, timeline_frame: FrameIdx, frame_size: (f32, f32), zoom: [f32; 2]) -> [f32; 2] {
        let len = self.timeline_len.max(1);
        let pos = timeline_frame - self.timeline_start;
        let mut offset = [0.0f32; 2];
        if let Some(t) = &self.effects.transition_in {
            let d = t.duration.clamp(1, len);
            if pos >= 0 && pos < d {
                let progress = eased(pos as f32 / d as f32, t.ease, t.curve);
                let vec = t.direction.vector();
                let amount = -(1.0 - progress) * push_clearance(vec, zoom);
                offset[0] += vec[0] * frame_size.0 * amount;
                offset[1] += vec[1] * frame_size.1 * amount;
            }
        }
        if let Some(t) = &self.effects.transition_out {
            let d = t.duration.clamp(1, len);
            let from_end = len - pos;
            if from_end > 0 && from_end <= d {
                let progress = eased(1.0 - from_end as f32 / d as f32, t.ease, t.curve);
                let vec = t.direction.vector();
                let amount = progress * push_clearance(vec, zoom);
                offset[0] += vec[0] * frame_size.0 * amount;
                offset[1] += vec[1] * frame_size.1 * amount;
            }
        }
        offset
    }
}

/// The attribute part of a clip: everything the "paste attributes" of
/// the NLEs copies, i.e. all but source, position and links.
#[derive(Debug, Clone)]
pub struct ClipAttributes {
    pub effects: EffectStack,
    pub fade_in: FrameIdx,
    pub fade_out: FrameIdx,
}

impl ClipAttributes {
    pub fn of(clip: &Clip) -> Self {
        Self {
            effects: clip.effects.clone(),
            fade_in: clip.fade_in,
            fade_out: clip.fade_out,
        }
    }

    pub fn apply_to(self, clip: &mut Clip) {
        clip.effects = self.effects;
        clip.fade_in = self.fade_in;
        clip.fade_out = self.fade_out;
    }
}

/// `frames` at `from` fps expressed at `to` fps, rounded.
pub fn convert_frames(frames: FrameIdx, from: Rational, to: Rational) -> FrameIdx {
    if from == to || from.num <= 0 || to.den <= 0 {
        return frames;
    }
    let num = frames as i128 * to.num as i128 * from.den as i128;
    let den = to.den as i128 * from.num as i128;
    (2 * num + den).div_euclid(2 * den) as FrameIdx
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    /// Always sorted by `timeline_start`, never overlapping.
    pub clips: Vec<Clip>,
    /// On a video track: excluded from compositing.
    pub muted: bool,
    /// Audio only: if at least one track is soloed, only those play.
    #[serde(default)]
    pub solo: bool,
    /// Its clips cannot be selected or edited, and nothing can
    /// land on it.
    #[serde(default)]
    pub locked: bool,
    /// Transitions straddling two adjacent clips. It never touches
    /// `timeline_start`/`timeline_len` of the clips involved (they stay non-
    /// overlapping, invariant intact): it is the rendering that "lends" for
    /// its window the tail of one and the head of the other, see
    /// `Track::crossing_at`. Whoever splits a clip, deletes it or moves it to
    /// another track must call `take_crossings_for` on its id,
    /// otherwise the entry stays in the data as a dangling reference (see
    /// `crossing_from`/`crossing_into`, which unlike `crossing_at`
    /// do not check that the clips still exist); moving it on the same
    /// track does not, it simply stays inert until it becomes
    /// adjacent again (`SplitClip`, `LiftDelete`, `MoveClips` in `command.rs`
    /// are the places to imitate for a new command touching the clips).
    #[serde(default)]
    pub crossings: Vec<CrossTransition>,
}

impl Track {
    pub fn new(kind: TrackKind) -> Self {
        Self {
            kind,
            clips: Vec::new(),
            muted: false,
            solo: false,
            locked: false,
            crossings: Vec::new(),
        }
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == id)
    }

    pub fn clip_mut(&mut self, id: ClipId) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == id)
    }

    pub fn remove_clip(&mut self, id: ClipId) -> Option<Clip> {
        let pos = self.clips.iter().position(|c| c.id == id)?;
        Some(self.clips.remove(pos))
    }

    /// Inserts preserving the order by `timeline_start`.
    pub fn insert_sorted(&mut self, clip: Clip) {
        let pos = self
            .clips
            .partition_point(|c| c.timeline_start < clip.timeline_start);
        self.clips.insert(pos, clip);
    }

    /// The crossing transition whose pair is still valid (both
    /// clips exist and are still adjacent) and whose window covers
    /// `frame`, with the two clips involved.
    pub fn crossing_at(&self, frame: FrameIdx) -> Option<(&Clip, &Clip, &CrossTransition)> {
        self.crossings.iter().find_map(|c| {
            let left = self.clip(c.left_clip)?;
            let right = self.clip(c.right_clip)?;
            if left.timeline_end() != right.timeline_start {
                return None;
            }
            c.window(left, right).contains(&frame).then_some((left, right, c))
        })
    }

    /// The crossing transition (valid or not) whose `left_clip` is `id`.
    pub fn crossing_from(&self, left_clip: ClipId) -> Option<&CrossTransition> {
        self.crossings.iter().find(|c| c.left_clip == left_clip)
    }

    /// The crossing transition (valid or not) whose `right_clip` is `id`.
    pub fn crossing_into(&self, right_clip: ClipId) -> Option<&CrossTransition> {
        self.crossings.iter().find(|c| c.right_clip == right_clip)
    }

    /// Removes and returns all the crossing transitions (of any
    /// kind, present or future: it does not filter on `transition.kind`)
    /// involving `id` as `left_clip` or `right_clip`. To be called from
    /// every command that splits or deletes a clip, so as not to leave
    /// dangling references in `crossings`.
    pub fn take_crossings_for(&mut self, id: ClipId) -> Vec<CrossTransition> {
        let mut removed = Vec::new();
        self.crossings.retain(|c| {
            if c.left_clip == id || c.right_clip == id {
                removed.push(c.clone());
                false
            } else {
                true
            }
        });
        removed
    }
}

/// A transition straddling two adjacent clips on the same track:
/// it "eats" the last `duration/2` frames of `left_clip` and the first
/// `duration/2` of `right_clip`, showing them overlapped instead of in
/// sequence. Unlike `EffectStack::transition_in`/`transition_out`
/// (one edge only, against transparency) here the sides are always two and
/// real: neither clip changes `timeline_start`/`timeline_len`,
/// the window is always computed from their current position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossTransition {
    pub left_clip: ClipId,
    pub right_clip: ClipId,
    pub transition: Transition,
}

impl CrossTransition {
    /// How many frames of the window fall before the cut (inside
    /// `left_clip`) and how many after (inside `right_clip`): always halved,
    /// the odd frame, if any, goes to the left side. Dragging the duration
    /// is symmetric (see timeline_ui), so the rounding choice
    /// matters only for odd durations.
    pub fn split(&self) -> (FrameIdx, FrameIdx) {
        Self::split_duration(self.transition.duration)
    }

    /// Like `split`, but for a given duration instead of `self.transition.duration`
    /// — needed by the live preview of a resize drag, where
    /// the shown duration is not the saved one yet.
    pub fn split_duration(duration: FrameIdx) -> (FrameIdx, FrameIdx) {
        let left = duration - duration / 2;
        (left, duration - left)
    }

    /// The window (in timeline frames) of the transition, given the current
    /// edges of `left`/`right` — never stored, always recomputed: if
    /// a clip moves the window follows it.
    pub fn window(&self, left: &Clip, right: &Clip) -> std::ops::Range<FrameIdx> {
        let (split_left, split_right) = self.split();
        (left.timeline_end() - split_left)..(right.timeline_start + split_right)
    }

    /// Progress 0..1 of `frame` in the window: 0 at the start (left
    /// still entirely in place, right entirely out), 1 at the end
    /// (the opposite). It already applies `ease`/`curve`.
    pub fn eased_progress_at(&self, frame: FrameIdx, left: &Clip, right: &Clip) -> f32 {
        let window = self.window(left, right);
        let len = (window.end - window.start).max(1);
        let raw = (frame - window.start) as f32 / len as f32;
        eased(raw.clamp(0.0, 1.0), self.transition.ease, self.transition.curve)
    }

    /// Position offset (same pixel units as `Transform.position`) of the
    /// left and right sides at the already "eased" progress `progress`: the
    /// left one exits, the right one enters, in the same direction — same
    /// mathematics as `Clip::transition_offset_at`, applied here to a
    /// progress shared by the whole window instead of local to the edge
    /// of a single clip.
    /// `left_zoom`/`right_zoom` are the already sampled ones of the transform of
    /// each clip at the same frame (see `push_clearance` and the docs of
    /// `Clip::transition_offset_at`): each can have its own, the push
    /// of the more zoomed one must reach farther to really
    /// clear the screen.
    pub fn offsets(&self, progress: f32, frame_size: (f32, f32), left_zoom: [f32; 2], right_zoom: [f32; 2]) -> ([f32; 2], [f32; 2]) {
        let vec = self.transition.direction.vector();
        let left_amount = progress * push_clearance(vec, left_zoom);
        let right_amount = -(1.0 - progress) * push_clearance(vec, right_zoom);
        let left = [vec[0] * frame_size.0 * left_amount, vec[1] * frame_size.1 * left_amount];
        let right = [vec[0] * frame_size.0 * right_amount, vec[1] * frame_size.1 * right_amount];
        (left, right)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub name: String,
    pub fps: Rational,
    pub resolution: (u32, u32),
    /// Compositing order: bottom -> top.
    pub tracks: Vec<Track>,
}

impl Timeline {
    pub fn clip(&self, track_index: usize, id: ClipId) -> Option<&Clip> {
        self.tracks.get(track_index)?.clip(id)
    }

    pub fn clip_mut(&mut self, track_index: usize, id: ClipId) -> Option<&mut Clip> {
        self.tracks.get_mut(track_index)?.clip_mut(id)
    }

    /// Last frame (exclusive) covered by any clip of the
    /// timeline, on any track.
    pub fn total_frames(&self) -> FrameIdx {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(Clip::timeline_end)
            .max()
            .unwrap_or(0)
    }

    /// Track name as in an NLE: V1, V2… A1, A2…, per kind.
    pub fn track_label(&self, track_index: usize) -> String {
        let number = self.track_number(track_index);
        match self.tracks[track_index].kind {
            TrackKind::Video => format!("V{number}"),
            TrackKind::Audio => format!("A{number}"),
        }
    }

    /// Position of the track among those of its kind, from 1 (the V/A of
    /// `track_label`).
    pub fn track_number(&self, track_index: usize) -> usize {
        let kind = self.tracks[track_index].kind;
        self.tracks[..=track_index].iter().filter(|t| t.kind == kind).count()
    }

    /// Absolute index of the `number`-th track (from 1) of kind `kind`.
    pub fn track_of_kind_numbered(&self, kind: TrackKind, number: usize) -> Option<usize> {
        self.tracks_of_kind(kind).map(|(i, _)| i).nth(number.checked_sub(1)?)
    }

    /// The tracks of kind `kind`, with their absolute index in `tracks`.
    pub fn tracks_of_kind(
        &self,
        kind: TrackKind,
    ) -> impl DoubleEndedIterator<Item = (usize, &Track)> {
        self.tracks
            .iter()
            .enumerate()
            .filter(move |(_, t)| t.kind == kind)
    }

    /// First track of kind `kind`.
    pub fn first_track_index(&self, kind: TrackKind) -> Option<usize> {
        self.tracks_of_kind(kind).map(|(i, _)| i).next()
    }

    /// The video clips covering `frame`, one per track, from bottom to
    /// top (compositing order). Excludes disabled clips and tracks.
    pub fn active_video_clips_at(&self, frame: FrameIdx) -> Vec<(usize, &Clip)> {
        self.tracks_of_kind(TrackKind::Video)
            .filter(|(_, t)| !t.muted)
            .filter_map(|(i, t)| {
                t.clips
                    .iter()
                    .find(|c| c.contains(frame))
                    .filter(|c| !c.disabled)
                    .map(|c| (i, c))
            })
            .collect()
    }

    /// The audio tracks ending up in the mix: not muted and, if any is
    /// soloed, only those.
    pub fn audible_tracks(&self) -> impl Iterator<Item = (usize, &Track)> {
        let any_solo = self.tracks_of_kind(TrackKind::Audio).any(|(_, t)| t.solo);
        self.tracks_of_kind(TrackKind::Audio)
            .filter(move |(_, t)| !t.muted && (!any_solo || t.solo))
    }

    pub fn is_locked(&self, track_index: usize) -> bool {
        self.tracks.get(track_index).is_some_and(|t| t.locked)
    }

    /// The first unlocked track of kind `kind`.
    pub fn first_unlocked_track_index(&self, kind: TrackKind) -> Option<usize> {
        self.tracks_of_kind(kind).find(|(_, t)| !t.locked).map(|(i, _)| i)
    }

    pub fn active_video_clip_at(&self, frame: FrameIdx) -> Option<(usize, &Clip)> {
        self.tracks_of_kind(TrackKind::Video)
            .rev()
            .find_map(|(i, t)| {
                t.clips
                    .iter()
                    .find(|c| c.contains(frame))
                    .map(|c| (i, c))
            })
    }

    /// The clips of `group` on all the tracks. One scan: it is called only on
    /// user interactions.
    pub fn clips_in_group(&self, group: LinkGroupId) -> Vec<(usize, ClipId)> {
        self.tracks
            .iter()
            .enumerate()
            .flat_map(|(i, t)| {
                t.clips
                    .iter()
                    .filter(move |c| c.linked_group == Some(group))
                    .map(move |c| (i, c.id))
            })
            .collect()
    }

    /// The other members of the linked group of a clip, excluding it.
    pub fn linked_members(&self, track_index: usize, clip_id: ClipId) -> Vec<(usize, ClipId)> {
        let Some(group) = self.clip(track_index, clip_id).and_then(|c| c.linked_group) else {
            return Vec::new();
        };
        self.clips_in_group(group)
            .into_iter()
            .filter(|&(_, id)| id != clip_id)
            .collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    pub media_pool: SlotMap<MediaId, MediaItem>,
    pub timelines: SlotMap<TimelineId, Timeline>,
    next_clip_id: u64,
    #[serde(default)]
    next_link_group_id: u64,
    /// Highest number ever assigned to a compound clip: kept only for
    /// compatibility with saved projects, the new name is chosen by
    /// `alloc_compound_name`.
    #[serde(default)]
    next_compound_id: u64,
    /// Counter for `MediaItem::content_hash` of the compound clips: see
    /// `Project::touch_compound`.
    #[serde(default)]
    next_compound_generation: u64,
}

impl Project {
    pub fn alloc_clip_id(&mut self) -> ClipId {
        let id = ClipId(self.next_clip_id);
        self.next_clip_id += 1;
        id
    }

    pub fn alloc_link_group_id(&mut self) -> LinkGroupId {
        let id = LinkGroupId(self.next_link_group_id);
        self.next_link_group_id += 1;
        id
    }

    /// Name for a new compound clip in the media pool: the lowest free
    /// number, so deleting one frees its name.
    pub fn alloc_compound_name(&mut self) -> String {
        let used: std::collections::HashSet<u64> = self
            .media_pool
            .values()
            .filter(|item| item.compound.is_some())
            .filter_map(|item| {
                item.path.to_str()?.strip_prefix(COMPOUND_NAME_PREFIX)?.parse().ok()
            })
            .collect();
        let number = (1..).find(|n| !used.contains(n)).unwrap_or(1);
        self.next_compound_id = self.next_compound_id.max(number);
        format!("{COMPOUND_NAME_PREFIX}{number}")
    }

    /// Name for a new timeline: the lowest free "Timeline N", so deleting
    /// one frees its name.
    pub fn alloc_timeline_name(&self) -> String {
        let used: std::collections::HashSet<u64> = self
            .timelines
            .values()
            .filter_map(|tl| tl.name.strip_prefix(TIMELINE_NAME_PREFIX)?.parse().ok())
            .collect();
        let number = (1..).find(|n| !used.contains(n)).unwrap_or(1);
        format!("{TIMELINE_NAME_PREFIX}{number}")
    }

    /// New value for `MediaItem::content_hash` of a compound clip: to be
    /// assigned on creation and every time its nested timeline
    /// changes, to invalidate the cache of composited frames depending
    /// on its content.
    pub fn alloc_compound_generation(&mut self) -> u64 {
        self.next_compound_generation += 1;
        self.next_compound_generation
    }

    /// Recomputes `meta`/`content_hash` of a compound clip from its
    /// current nested timeline. To be called after every command touching
    /// that timeline, not only on creation: `meta.duration_frames`
    /// (hence the `Clip::timeline_len` available in a drag from the media pool)
    /// must reflect the real content, not the one at the time of
    /// creation.
    pub fn sync_compound_meta(&mut self, media_id: MediaId) {
        let Some(item) = self.media_pool.get(media_id) else { return };
        let Some(timeline_id) = item.compound else { return };
        let Some(timeline) = self.timelines.get(timeline_id) else { return };
        let has_video = timeline
            .tracks_of_kind(TrackKind::Video)
            .any(|(_, t)| !t.clips.is_empty());
        let has_audio = timeline
            .tracks_of_kind(TrackKind::Audio)
            .any(|(_, t)| !t.clips.is_empty());
        let meta = MediaMeta {
            duration_frames: timeline.total_frames(),
            fps: timeline.fps,
            width: timeline.resolution.0,
            height: timeline.resolution.1,
            has_video,
            has_audio,
            sample_rate: 48_000,
            channels: 2,
            audio_streams: 1,
        };
        let generation = self.alloc_compound_generation();
        let item = self.media_pool.get_mut(media_id).expect("checked above");
        item.meta = meta;
        item.content_hash = generation;
    }

    /// `true` if a clip referencing `media_id` (a compound clip, or the
    /// project timeline itself: see `MediaItem::compound`) cannot
    /// land on `destination` without closing a cycle — that is, if
    /// `destination` is reachable from the nested timeline of
    /// `media_id`, following in turn the compound clips it contains, at
    /// any depth (direct import of a timeline inside itself,
    /// or indirect through one of its compound clips). The rendering
    /// has a depth limit anyway as a safety net (see
    /// `MAX_COMPOUND_DEPTH` in `vv_app::render_ahead`), but a cycle must
    /// not be creatable in the first place.
    pub fn would_create_a_cycle(&self, media_id: MediaId, destination: TimelineId) -> bool {
        let Some(start) = self.media_pool.get(media_id).and_then(|m| m.compound) else {
            return false;
        };
        if start == destination {
            return true;
        }
        let mut visited: std::collections::HashSet<TimelineId> = std::collections::HashSet::new();
        let mut stack = vec![start];
        while let Some(current) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            let Some(timeline) = self.timelines.get(current) else {
                continue;
            };
            for clip in timeline.tracks.iter().flat_map(|t| t.clips.iter()) {
                let ClipSource::Media(id) = &clip.source else {
                    continue;
                };
                let Some(nested) = self.media_pool.get(*id).and_then(|m| m.compound) else {
                    continue;
                };
                if nested == destination {
                    return true;
                }
                stack.push(nested);
            }
        }
        false
    }

    /// Recomputes `Clip::rate` from the fps. `source_offset`/`timeline_len` are
    /// in timeline frames and do not change.
    pub fn refresh_clip_rates(&mut self) {
        let media_pool = &self.media_pool;
        for timeline in self.timelines.values_mut() {
            let timeline_fps = timeline.fps;
            for clip in timeline.tracks.iter_mut().flat_map(|t| t.clips.iter_mut()) {
                let ClipSource::Media(media_id) = &clip.source else {
                    continue;
                };
                if let Some(item) = media_pool.get(*media_id) {
                    clip.rate = Rational::conform_rate(timeline_fps, item.meta.fps);
                }
            }
        }
    }
}

#[cfg(test)]
mod keyframe_tests {
    use super::*;

    #[test]
    fn ease_in_and_ease_out_are_slow_then_fast_and_viceversa() {
        assert!(Interpolation::EaseIn.ease(0.5) < 0.5);
        assert!(Interpolation::EaseOut.ease(0.5) > 0.5);
        for interp in Interpolation::PRESETS {
            assert_eq!(interp.ease(1.0), 1.0, "{interp:?} deve arrivare a destinazione");
        }
    }

    #[test]
    fn a_bezier_matches_the_linear_curve_when_its_controls_are_on_the_diagonal() {
        let linear = Interpolation::Bezier {
            c1: [1.0 / 3.0, 1.0 / 3.0],
            c2: [2.0 / 3.0, 2.0 / 3.0],
        };
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((linear.ease(t) - t).abs() < 1e-3, "t={t}");
        }
    }

    #[test]
    fn a_bezier_keyframe_interpolates_along_its_curve() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        // Controls squashed low: at half the segment the value is
        // still below half.
        k.upsert(0, 0.0, Interpolation::Bezier { c1: [0.5, 0.0], c2: [1.0, 0.0] });
        k.upsert(10, 100.0, Interpolation::Linear);
        assert!(k.value_at(5) < 50.0);
        assert_eq!(k.value_at(0), 0.0);
        assert_eq!(k.value_at(10), 100.0);
    }

    #[test]
    fn set_interpolation_returns_the_previous_one_and_ignores_empty_frames() {
        let mut k: Keyframed<f32> = Keyframed::constant(0.0);
        k.upsert(4, 1.0, Interpolation::Linear);
        assert_eq!(k.set_interpolation(4, Interpolation::Hold), Some(Interpolation::Linear));
        assert_eq!(k.keyframe_at(4), Some((1.0, Interpolation::Hold)));
        assert_eq!(k.set_interpolation(7, Interpolation::Hold), None);
    }

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
        // Monotonicity: values increasing with time.
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
    fn transform_param_index_follows_all() {
        for (i, p) in TransformParam::ALL.iter().enumerate() {
            assert_eq!(p.index(), i);
        }
    }

    #[test]
    fn transform_tracks_animate_each_param_on_its_own() {
        let mut tracks = TransformTracks::default();
        tracks
            .track_mut(TransformParam::PositionX)
            .upsert(0, 0.0, Interpolation::Linear);
        tracks
            .track_mut(TransformParam::PositionX)
            .upsert(10, 100.0, Interpolation::Linear);

        assert_eq!(tracks.value_at(5).position, [50.0, 0.0]);
        assert_eq!(
            tracks.value_at(5).zoom,
            [1.0, 1.0],
            "gli altri parametri restano al loro default"
        );
        assert!(!tracks.is_constant());
    }

    #[test]
    fn transform_tracks_find_the_nearest_keyframe_in_each_direction() {
        let mut tracks = TransformTracks::default();
        tracks
            .track_mut(TransformParam::Rotation)
            .upsert(10, 0.0, Interpolation::Linear);
        tracks
            .track_mut(TransformParam::ZoomX)
            .upsert(30, 2.0, Interpolation::Linear);

        let both = [TransformParam::Rotation, TransformParam::ZoomX];
        assert_eq!(tracks.previous_keyframe(&both, 20), Some(10));
        assert_eq!(tracks.next_keyframe(&both, 20), Some(30));
        assert_eq!(tracks.previous_keyframe(&both, 10), None, "non se stesso");
        assert_eq!(
            tracks.next_keyframe(&[TransformParam::Rotation], 20),
            None,
            "solo i keyframe dei parametri chiesti"
        );
    }

    #[test]
    fn transform_lerp_interpolates_each_field() {
        let a = Transform::default();
        let b = Transform {
            crop: [0.2, 0.2, 0.8, 0.8],
            crop_softness: 0.4,
            zoom: [3.0, 5.0],
            position: [1.0, -1.0],
            rotation: 90.0,
            anchor: [0.2, 0.4],
            flip: [true, true],
            opacity: 0.0,
        };
        let mid = Transform::lerp(&a, &b, 0.5);
        assert_eq!(mid.crop, [0.1, 0.1, 0.4, 0.4]);
        assert_eq!(mid.crop_softness, 0.2);
        assert_eq!(mid.zoom, [2.0, 3.0]);
        assert_eq!(mid.position, [0.5, -0.5]);
        assert_eq!(mid.rotation, 45.0);
        assert_eq!(mid.anchor, [0.1, 0.2]);
        assert_eq!(mid.opacity, 50.0);
        assert_eq!(mid.flip, [true, true], "il flip scatta a metà, non sfuma");
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

#[cfg(test)]
mod timeline_tests {
    use super::*;

    fn clip_at(timeline_start: FrameIdx, len: FrameIdx, id: u64) -> Clip {
        Clip::from_source_range(
            ClipId(id),
            ClipSource::SolidColor,
            0,
            len,
            timeline_start,
            Rational::one(),
        )
    }

    #[test]
    fn active_video_clip_at_prefers_the_topmost_video_track_where_it_has_a_clip() {
        // Two video tracks: the first (index 0, "bottom") covers [0, 30), the
        // second (index 1, "top") only [10, 20) — a shorter layer
        // overlapping a longer one, the base case of blend-over
        // (REFACTOR_PIPELINE.md B4).
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(0, 30, 1)],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(10, 10, 2)],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
            ],
        };
        assert_eq!(
            tl.active_video_clip_at(5).map(|(t, c)| (t, c.id)),
            Some((0, ClipId(1))),
            "solo la track bottom ha qualcosa qui"
        );
        assert_eq!(
            tl.active_video_clip_at(15).map(|(t, c)| (t, c.id)),
            Some((1, ClipId(2))),
            "la track top copre questo punto: vince lei"
        );
        assert_eq!(
            tl.active_video_clip_at(25).map(|(t, c)| (t, c.id)),
            Some((0, ClipId(1))),
            "tornata scoperta la track top, si rivede la bottom sotto"
        );
        assert!(tl.active_video_clip_at(35).is_none());
    }

    #[test]
    fn active_video_clips_at_returns_every_covering_track_bottom_to_top() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(0, 30, 1)],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(10, 10, 2)],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
            ],
        };
        assert_eq!(
            tl.active_video_clips_at(15)
                .iter()
                .map(|(t, c)| (*t, c.id))
                .collect::<Vec<_>>(),
            vec![(0, ClipId(1)), (1, ClipId(2))],
            "bottom prima, top per ultima: è l'ordine di compositing"
        );
        assert_eq!(
            tl.active_video_clips_at(5)
                .iter()
                .map(|(_, c)| c.id)
                .collect::<Vec<_>>(),
            vec![ClipId(1)],
            "qui solo la track bottom ha una clip"
        );
        assert!(tl.active_video_clips_at(35).is_empty());
    }

    #[test]
    fn active_video_clip_at_ignores_audio_tracks() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(0, 10, 1)],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![clip_at(0, 10, 2)],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
            ],
        };
        assert_eq!(
            tl.active_video_clip_at(5).map(|(_, c)| c.id),
            Some(ClipId(1))
        );
    }

    #[test]
    fn first_track_index_finds_the_bottom_most_track_of_a_kind() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track::new(TrackKind::Video),
                Track::new(TrackKind::Audio),
                Track::new(TrackKind::Video),
            ],
        };
        assert_eq!(tl.first_track_index(TrackKind::Video), Some(0));
        assert_eq!(tl.first_track_index(TrackKind::Audio), Some(1));
    }

    #[test]
    fn source_frame_at_maps_a_trimmed_clip_correctly() {
        let clip = Clip::from_source_range(
            ClipId(1),
            ClipSource::SolidColor,
            200,
            300,
            60,
            Rational::one(),
        );
        assert_eq!(clip.source_frame_at(60), 200, "primo frame della clip");
        assert_eq!(clip.source_frame_at(75), 215);
    }

    #[test]
    fn total_frames_is_the_furthest_clip_end_across_tracks() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(0, 10, 1)],
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![clip_at(15, 10, 2)], // ends at 25, later than the video one
                    muted: false,
                    solo: false,
                    locked: false,
                    crossings: Vec::new(),
                },
            ],
        };
        assert_eq!(tl.total_frames(), 25);
    }

    fn media_clip_at(
        rate: Rational,
        source_in: FrameIdx,
        source_out: FrameIdx,
        timeline_start: FrameIdx,
    ) -> Clip {
        Clip::from_source_range(
            ClipId(1),
            ClipSource::SolidColor,
            source_in,
            source_out,
            timeline_start,
            rate,
        )
    }

    /// 59.94 fps on a 60 fps timeline: the clip lasts on the timeline as long as
    /// it really lasts (0.1% more frames), not 1:1 like the source frames.
    #[test]
    fn a_clip_slower_than_the_timeline_lasts_longer_in_timeline_frames() {
        let rate = Rational::conform_rate(Rational::new(60, 1), Rational::new(60000, 1001));
        assert_eq!(rate, Rational::new(1001, 1000));

        // 15 minutes of source at 59.94 fps.
        let source_len = 53_946;
        let clip = media_clip_at(rate, 0, source_len, 0);
        assert_eq!(clip.source_len(), source_len);
        assert_eq!(clip.timeline_len, 54_000, "15 minuti esatti a 60 fps");
    }

    #[test]
    fn source_frame_at_maps_the_edges_and_never_drifts_over_fifteen_minutes() {
        let rate = Rational::conform_rate(Rational::new(60, 1), Rational::new(60000, 1001));
        let clip = media_clip_at(rate, 0, 53_946, 120);

        assert_eq!(clip.source_frame_at(120), 0, "primo frame della clip");
        assert_eq!(
            clip.source_frame_at(clip.timeline_end() - 1),
            53_945,
            "ultimo frame sorgente sull'ultimo frame di timeline"
        );

        // No accumulated drift: at every instant the source frame
        // shown stays the one belonging to the real elapsed time
        // (half a frame of tolerance, the unavoidable rounding).
        for t in (0..clip.timeline_len).step_by(137) {
            let secs = t as f64 / 60.0;
            let expected = secs * (60000.0 / 1001.0);
            let got = clip.source_frame_at(120 + t) as f64;
            assert!(
                (got - expected).abs() <= 0.5,
                "a {secs}s: atteso ~{expected}, ottenuto {got}"
            );
        }
    }

    #[test]
    fn source_frame_at_maps_a_faster_media_by_skipping_frames() {
        // 50 fps on a 25 fps timeline: two source frames per timeline frame.
        let rate = Rational::conform_rate(Rational::new(25, 1), Rational::new(50, 1));
        assert_eq!(rate, Rational::new(1, 2));
        let clip = media_clip_at(rate, 0, 100, 0);
        assert_eq!(clip.timeline_len, 50);
        assert_eq!(clip.source_frame_at(0), 0);
        assert_eq!(clip.source_frame_at(1), 2);
        assert_eq!(clip.source_frame_at(49), 98);
    }

    #[test]
    fn source_frame_at_maps_a_slower_media_by_repeating_frames() {
        // 25 fps on a 30 fps timeline: 6 timeline frames every 5 source ones.
        let rate = Rational::conform_rate(Rational::new(30, 1), Rational::new(25, 1));
        assert_eq!(rate, Rational::new(6, 5));
        let clip = media_clip_at(rate, 0, 25, 0);
        assert_eq!(clip.timeline_len, 30, "1s a 25 fps dura 1s a 30 fps");
        let sources: Vec<FrameIdx> = (0..6).map(|t| clip.source_frame_at(t)).collect();
        assert_eq!(sources, vec![0, 1, 2, 2, 3, 4], "un frame ripetuto su sei");
    }

    #[test]
    fn rate_one_behaves_exactly_like_before() {
        let clip = media_clip_at(Rational::one(), 200, 300, 60);
        assert_eq!(clip.timeline_len, 100);
        assert_eq!(clip.source_frame_at(60), 200);
        assert_eq!(clip.source_frame_at(75), 215);
        assert_eq!(clip.timeline_frame_at(215), 75);
    }

    #[test]
    fn timeline_frame_at_is_the_inverse_of_source_frame_at() {
        let rate = Rational::conform_rate(Rational::new(60, 1), Rational::new(60000, 1001));
        let clip = media_clip_at(rate, 1_000, 5_000, 300);
        for s in (clip.source_in()..clip.source_out()).step_by(7) {
            assert_eq!(clip.source_frame_at(clip.timeline_frame_at(s)), s);
        }
        assert_eq!(clip.timeline_frame_at(clip.source_in()), clip.timeline_start);
        assert_eq!(clip.timeline_frame_at(clip.source_out()), clip.timeline_end());
    }

    #[test]
    fn refresh_clip_rates_conforms_a_clip_loaded_without_a_rate() {
        let mut project = Project::default();
        let media_id = project.media_pool.insert(MediaItem {
            path: "/tmp/x.mp4".into(),
            meta: MediaMeta {
                duration_frames: 1000,
                fps: Rational::new(60000, 1001),
                width: 1920,
                height: 1080,
                has_video: true,
                has_audio: false,
                sample_rate: 0,
                channels: 0,
                audio_streams: 0,
            },
            content_hash: 0,
            compound: None,
        });
        let mut clip = media_clip_at(Rational::one(), 0, 1000, 0);
        clip.source = ClipSource::Media(media_id);
        let timeline_id = project.timelines.insert(Timeline {
            name: "T".into(),
            fps: Rational::new(60, 1),
            resolution: (1920, 1080),
            tracks: vec![Track {
                kind: TrackKind::Video,
                clips: vec![clip, media_clip_at(Rational::one(), 0, 10, 2000)],
                muted: false,
                solo: false,
                locked: false,
                crossings: Vec::new(),
            }],
        });

        project.refresh_clip_rates();

        let clips = &project.timelines[timeline_id].tracks[0].clips;
        assert_eq!(clips[0].rate, Rational::new(1001, 1000));
        assert_eq!(
            clips[1].rate,
            Rational::one(),
            "una SolidColor non si conforma a nulla"
        );
        assert_eq!(clips[0].timeline_len, 1000, "la durata di timeline non cambia");
    }

    #[test]
    fn retime_keeps_seconds_and_round_trips() {
        let rate_30 = Rational::conform_rate(Rational::new(30, 1), Rational::new(30000, 1001));
        let mut clip = media_clip_at(rate_30, 300, 900, 150);
        let original = (clip.timeline_start, clip.source_offset, clip.timeline_len);

        let rate_25 = Rational::conform_rate(Rational::new(25, 1), Rational::new(30000, 1001));
        clip.retime(Rational::new(30, 1), Rational::new(25, 1), rate_25);
        assert_eq!(clip.timeline_start, 125, "5 secondi");
        assert_eq!(clip.source_offset, 250, "10 secondi nel media");
        assert_eq!(clip.timeline_end(), 626, "fine a 751 frame di 30 fps");
        assert_eq!(clip.rate, rate_25);

        clip.retime(Rational::new(25, 1), Rational::new(30, 1), rate_30);
        assert_eq!((clip.timeline_start, clip.source_offset, clip.timeline_len), original);
    }

    #[test]
    fn total_frames_is_zero_for_an_empty_timeline() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track::new(TrackKind::Video)],
        };
        assert_eq!(tl.total_frames(), 0);
    }

    /// A project saved before a new parameter has fewer tracks: the
    /// missing parameter must go back to its default, not make the
    /// loading fail.
    #[test]
    fn transform_tracks_saved_without_a_newer_param_load_with_its_default() {
        let older = "(params: [], flip: (false, false))";
        let tracks: TransformTracks = ron::from_str(older).expect("caricamento");
        let t = tracks.value_at(0);
        assert_eq!(t.opacity, 100.0);
        assert_eq!(t.zoom, [1.0, 1.0]);
    }

}
