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

/// Id di un gruppo di clip collegate: un contatore, l'appartenenza è solo
/// il campo `Clip::linked_group`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkGroupId(pub u64);

/// Contatore: le clip vivono in `Vec` ordinati dentro le track, non in
/// un'arena.
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

    pub const fn one() -> Self {
        Self { num: 1, den: 1 }
    }

    /// Da un fps in virgola mobile (come in OTIO): riconosce gli fps NTSC
    /// (`n * 1000/1001`), altrimenti approssima al millesimo.
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

    /// Frame di timeline per frame sorgente di un media a `media_fps` su
    /// una timeline a `timeline_fps`, ridotto ai minimi termini (vedi
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
        // Un rapporto irriducibile che non entra in i32 (fps esotici su
        // entrambi i lati) viene approssimato: meglio un milionesimo di
        // errore che un overflow.
        if num > i32::MAX as i64 || den > i32::MAX as i64 {
            let approx = (num as f64 / den as f64 * 1_000_000.0).round() as i64;
            return Self::new(approx.clamp(1, i32::MAX as i64) as i32, 1_000_000);
        }
        Self::new(num as i32, den as i32)
    }

    /// `round(frames * self)`, mezzi verso l'alto. Identità esatta per
    /// `1/1`, così una clip non conformata resta bit-per-bit come prima.
    pub fn scale_round(self, frames: FrameIdx) -> FrameIdx {
        if self.is_one() || self.num <= 0 || self.den <= 0 {
            return frames;
        }
        let (num, den) = (self.num as i128, self.den as i128);
        let v = frames as i128;
        ((2 * v * num + den).div_euclid(2 * den)) as FrameIdx
    }

    /// Il più grande `n` tale che `scale_round(n) <= scaled`: l'inverso di
    /// `scale_round`, cioè "quale frame sorgente copre questa posizione".
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

/// Matrice YUV→RGB di un frame decodificato. BT.2020 solo se il sorgente
/// la segnala: mai indovinata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Indice di frame, sempre relativo al contesto in cui è usato: frame
/// sorgente di un media (fps nativo) oppure frame di Timeline (fps della
/// Timeline che lo contiene). I due spazi non vanno mai confusi.
pub type FrameIdx = i64;

/// `duration_frames` di un'immagine: ~463 giorni a `IMAGE_FPS`, nessuna
/// durata reale lo raggiunge, così fa da segno "è un'immagine" senza un
/// campo in più e non limita il trim.
pub const IMAGE_DURATION_FRAMES: FrameIdx = 1_000_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMeta {
    pub duration_frames: FrameIdx,
    pub fps: Rational,
    /// `width`/`height` a zero e `fps` nominale se `false`: un media solo
    /// audio va solo su track audio.
    pub width: u32,
    pub height: u32,
    pub has_video: bool,
    pub has_audio: bool,
    pub sample_rate: u32,
    pub channels: u16,
    /// Stream audio nel contenitore. 0 nei progetti salvati prima che il
    /// campo esistesse: l'app lo ricalcola all'apertura.
    #[serde(default)]
    pub audio_streams: u16,
}

impl MediaMeta {
    /// Stream audio da usare: almeno uno se il media ha audio.
    pub fn audio_stream_count(&self) -> usize {
        if self.has_audio {
            usize::from(self.audio_streams).max(1)
        } else {
            0
        }
    }

    /// Un'immagine ferma importata nel pool: ha video ma non audio, e
    /// `duration_frames` è il sentinel `IMAGE_DURATION_FRAMES` (vedi la
    /// sua doc sul perché non serve un campo dedicato).
    pub fn is_image(&self) -> bool {
        self.has_video && !self.has_audio && self.duration_frames == IMAGE_DURATION_FRAMES
    }
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

    /// Frame del keyframe più vicino prima di `frame`.
    pub fn keyframe_before(&self, frame: FrameIdx) -> Option<FrameIdx> {
        self.keyframes.iter().rev().map(|k| k.0).find(|&f| f < frame)
    }

    /// Frame del keyframe più vicino dopo `frame`.
    pub fn keyframe_after(&self, frame: FrameIdx) -> Option<FrameIdx> {
        self.keyframes.iter().map(|k| k.0).find(|&f| f > frame)
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
            crop_softness: f32::lerp(&a.crop_softness, &b.crop_softness, t),
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
            // Un flip non ha vie di mezzo: scatta a metà interpolazione.
            flip: if t < 0.5 { a.flip } else { b.flip },
        }
    }
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

impl<T: Lerp + Clone> Keyframed<T> {
    /// `default` senza keyframe; prima del primo e dopo l'ultimo il valore
    /// estremo; in mezzo l'interpolazione del keyframe di partenza.
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

    /// Scarta i keyframe prima di `start` preservando il valore da `start`
    /// in poi: resta un keyframe di raccordo solo se l'animazione ne dipende.
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

    /// Simmetrica di `drop_before`: tiene solo i keyframe prima di `end`.
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
    /// Pixel tagliati per lato (sinistra, alto, destra, basso) alla risoluzione
    /// nativa del media, non del proxy. Il resto non si ricentra.
    pub crop: [f32; 4], // left, top, right, bottom
    /// Sfumatura del bordo di crop in pixel del media: negativa verso l'interno,
    /// positiva verso l'esterno, 0 netto.
    pub crop_softness: f32,
    /// Ingrandimento per asse attorno all'`anchor`, rispetto al frame di output.
    pub zoom: [f32; 2],
    /// Spostamento in pixel di timeline, Y verso l'alto.
    pub position: [f32; 2],
    /// Rotazione in gradi, oraria, attorno all'`anchor`.
    pub rotation: f32,
    /// Pivot di zoom e rotazione, in pixel di timeline a partire dal centro
    /// della clip (`[0, 0]` = il suo centro), con gli stessi versi di
    /// `position` (Y positivo verso l'alto).
    pub anchor: [f32; 2],
    /// Specchiatura orizzontale (X) e verticale (Y).
    pub flip: [bool; 2],
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
        }
    }
}

/// Un parametro del transform, ognuno con i suoi keyframe. `flip` no: non
/// si interpola.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl TransformParam {
    pub const ALL: [Self; 12] = [
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
    ];

    /// Posizione in `TransformTracks::params` — l'ordine di `ALL`, che è
    /// quello di dichiarazione.
    pub fn index(self) -> usize {
        self as usize
    }

    /// Il valore che ha in un `Transform` già valutato.
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
        }
    }
}

/// Il transform di una clip: un `Keyframed<f32>` per parametro, così ogni
/// parametro si anima per conto suo, più il flip (non animabile).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformTracks {
    /// Uno per `TransformParam`, nell'ordine di `TransformParam::ALL`.
    params: Vec<Keyframed<f32>>,
    pub flip: [bool; 2],
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

    /// `true` se nessun parametro ha keyframe.
    pub fn is_constant(&self) -> bool {
        self.params.iter().all(|k| k.is_constant())
    }

    /// `true` se il transform è ancora quello di default, keyframe compresi.
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
            zoom: [v(TransformParam::ZoomX), v(TransformParam::ZoomY)],
            position: [v(TransformParam::PositionX), v(TransformParam::PositionY)],
            rotation: v(TransformParam::Rotation),
            anchor: [v(TransformParam::AnchorX), v(TransformParam::AnchorY)],
            flip: self.flip,
        }
    }

    /// Il keyframe più vicino a `frame` *prima* di esso, tra quelli dei
    /// parametri dati: serve alle frecce di navigazione del pannello.
    pub fn previous_keyframe(&self, params: &[TransformParam], frame: FrameIdx) -> Option<FrameIdx> {
        params
            .iter()
            .filter_map(|p| self.track(*p).keyframe_before(frame))
            .max()
    }

    /// Simmetrica di `previous_keyframe`, in avanti.
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

    /// Grigio opaco.
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

/// Parametri di una clip `ClipSource::Text`. Le misure sono in pixel di
/// timeline, come quelle del `Transform`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleParams {
    pub content: String,
    /// Vuota = sans-serif di sistema.
    pub font_family: String,
    /// 100-900, come in CSS.
    pub font_weight: u16,
    pub italic: bool,
    pub color: Rgba,
    pub size: f32,
    /// Spaziatura fra le lettere, in millesimi di em.
    pub tracking: f32,
    /// Spazio extra fra le righe, in pixel.
    pub line_spacing: f32,
    pub underline: bool,
    pub strikethrough: bool,
    pub case: FontCase,
    pub align: TextAlign,
    /// Quale punto del blocco di testo cade su `position`.
    pub anchor: (HAnchor, VAnchor),
    /// Dal centro del frame, Y verso l'alto.
    pub position: [f32; 2],
    #[serde(default)]
    pub shadow: TitleShadow,
    #[serde(default)]
    pub background: TitleBackground,
}

/// Ombra del solo testo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleShadow {
    pub enabled: bool,
    pub color: Rgba,
    /// In pixel di timeline, Y verso l'alto.
    pub offset: [f32; 2],
    /// Raggio della sfocatura, in pixel di timeline.
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

/// Rettangolo dietro al testo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TitleBackground {
    pub enabled: bool,
    pub color: Rgba,
    pub outline_color: Rgba,
    /// In pixel di timeline, verso l'interno del rettangolo.
    pub outline_width: f32,
    /// Frazione della larghezza/altezza del frame; 0 = attorno al testo.
    pub width: f32,
    pub height: f32,
    /// Frazione del lato più corto del rettangolo, fino a 0.5.
    pub corner_radius: f32,
    /// Spostamento dal centro del testo, in pixel di timeline, Y verso l'alto.
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
            opacity: 50.0,
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
    /// Il testo da disegnare, con `case` già applicato.
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
    /// Parametri in `EffectStack::title`.
    Text,
}

/// Estremi di `gain_db`: condivisi fra lo slider del pannello proprietà e la
/// riga del volume in timeline, così restano sempre uno solo.
pub const GAIN_DB_MIN: f32 = -100.0;
pub const GAIN_DB_MAX: f32 = 30.0;

/// Un filtro del pannello Effects: la varietà è aperta (nuove varianti per
/// nuovi filtri), il rendering la traduce in un id per lo shader — vedi
/// `vv_render`, che non conosce il significato di ciascuna, solo il suo id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterKind {
    Grayscale,
}

/// Un filtro applicato a una clip. L'ordine nel `Vec` di
/// `EffectStack::filters` è l'ordine di applicazione, configurabile
/// dall'utente (più filtri sulla stessa clip, in una sequenza scelta da
/// lui); `enabled` lo sospende senza toglierlo dalla sequenza.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipFilter {
    pub kind: FilterKind,
    pub enabled: bool,
}

/// Una transizione del pannello Effects, sezione "Transizioni": come
/// `FilterKind`, varietà aperta per transizioni future. A differenza dei
/// filtri, si applica solo a un bordo della clip (`EffectStack::transition_in`
/// o `transition_out`), non alla clip intera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionKind {
    Push,
}

/// Direzione di scorrimento di una transizione Push. Indipendente dal bordo
/// della clip a cui è agganciata (`In`/`Out`): descrive solo il verso del
/// movimento sullo schermo durante la transizione.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushDirection {
    Left,
    Right,
    Up,
    Down,
}

impl PushDirection {
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    /// Verso del movimento in unità di `Transform.position` (X, Y): `Y`
    /// positivo verso l'alto, come il resto del transform.
    fn vector(self) -> [f32; 2] {
        match self {
            Self::Left => [-1.0, 0.0],
            Self::Right => [1.0, 0.0],
            Self::Up => [0.0, 1.0],
            Self::Down => [0.0, -1.0],
        }
    }
}

/// Curva di accelerazione di una transizione, applicata alla progressione
/// 0..1 prima di tradurla in offset. Le stesse quattro opzioni di un NLE.
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

/// `t` (0..1) rimodulato secondo `ease`; `curve` (0..1, "Transition Curve"
/// nell'inspector) ne controlla l'intensità: 0 quasi lineare, 1 più
/// pronunciata. Nessuna pretesa di uguagliare la curva esatta di un NLE
/// specifico, solo una progressione monotona e simmetrica in InOut.
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

/// Una transizione applicata a un bordo di una clip (vedi
/// `EffectStack::transition_in`/`transition_out`). `duration` in frame di
/// timeline dal bordo, come `Clip::fade_in`/`fade_out`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub kind: TransitionKind,
    pub duration: FrameIdx,
    pub direction: PushDirection,
    pub ease: Ease,
    /// "Transition Curve" nell'inspector, 0..1.
    pub curve: f32,
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
    /// Transizione agganciata al bordo iniziale/finale della clip, dal
    /// pannello Effects. Coppia di campi come `Clip::fade_in`/`fade_out`,
    /// non una mappa: al più una per bordo.
    #[serde(default)]
    pub transition_in: Option<Transition>,
    #[serde(default)]
    pub transition_out: Option<Transition>,
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
        }
    }
}

impl EffectStack {
    /// Vedi `Keyframed::drop_before`, su ogni parametro animabile.
    pub fn drop_keyframes_before(&mut self, start: FrameIdx) {
        self.for_each_f32_track(|k| k.drop_before(start));
        if let Some(c) = &mut self.color {
            c.drop_before(start);
        }
    }

    /// Vedi `Keyframed::drop_from`, su ogni parametro animabile.
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

    /// `true` se nessuna proprietà è stata toccata rispetto al default: la
    /// timeline disegna più scure le clip per cui è `false`. Il titolo non
    /// conta: è il contenuto della clip, non un effetto.
    pub fn is_pristine(&self) -> bool {
        self.transform.is_pristine()
            && self.speed.is_constant()
            && self.speed.default == 1.0
            && self.gain_db.is_constant()
            && self.gain_db.default == 0.0
            && self.color.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub source: ClipSource,
    /// Inizio nel media in frame di *timeline* dal frame sorgente 0: una clip
    /// conformata può cominciare a metà di un frame sorgente.
    pub source_offset: FrameIdx,
    /// Posizione nello spazio della Timeline che contiene questa clip.
    pub timeline_start: FrameIdx,
    pub timeline_len: FrameIdx,
    pub effects: EffectStack,
    /// Gruppo collegato (di solito video e tutti gli stream audio di un
    /// import): selezione, drag e cancellazione lo trattano come un'unità.
    #[serde(default)]
    pub linked_group: Option<LinkGroupId>,
    /// Clip audio: quale stream del contenitore (ordine di
    /// `vv_media::audio_streams`).
    #[serde(default)]
    pub audio_stream_index: usize,
    /// Frame di timeline per frame sorgente (`Rational::conform_rate`).
    #[serde(default = "Rational::one")]
    pub rate: Rational,
    /// Esclusa da compositing e mix, ma resta in timeline.
    #[serde(default)]
    pub disabled: bool,
    /// Durata della dissolvenza in entrata, in frame di timeline dall'inizio
    /// della clip. 0 = nessuna.
    #[serde(default)]
    pub fade_in: FrameIdx,
    /// Durata della dissolvenza in uscita, in frame di timeline dalla fine
    /// della clip. 0 = nessuna.
    #[serde(default)]
    pub fade_out: FrameIdx,
}

impl Clip {
    /// Clip che mostra `source_in..source_out()` del sorgente a partire da
    /// `timeline_start`, senza effetti né collegamenti.
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
        }
    }

    /// Primo frame sorgente mostrato.
    pub fn source_in(&self) -> FrameIdx {
        self.rate.unscale_round(self.source_offset)
    }

    /// Esclusivo: l'ultimo frame sorgente mostrato, più uno.
    pub fn source_out(&self) -> FrameIdx {
        self.rate.unscale_round(self.source_offset + self.timeline_len - 1) + 1
    }

    /// Durata in frame *sorgente*, per i conti che vivono in quello
    /// spazio (quanti frame del media serve decodificare).
    pub fn source_len(&self) -> FrameIdx {
        self.source_out() - self.source_in()
    }

    pub fn timeline_end(&self) -> FrameIdx {
        self.timeline_start + self.timeline_len
    }

    /// `frame` (di timeline) cade dentro la clip.
    pub fn contains(&self, frame: FrameIdx) -> bool {
        frame >= self.timeline_start && frame < self.timeline_end()
    }

    /// Frame sorgente mostrato alla posizione di timeline `timeline_frame`
    /// (dentro la clip). Unico punto della mappatura, condiviso da anteprima
    /// ed export: un futuro time-remap va applicato qui.
    pub fn source_frame_at(&self, timeline_frame: FrameIdx) -> FrameIdx {
        self.rate
            .unscale_round(timeline_frame - self.timeline_start + self.source_offset)
    }

    /// Inverso di `source_frame_at`, anche fuori dal trim (serve ai limiti di
    /// trim).
    pub fn timeline_frame_at(&self, source_frame: FrameIdx) -> FrameIdx {
        self.timeline_start + self.rate.scale_round(source_frame) - self.source_offset
    }

    /// Secondi dall'inizio del media a `timeline_frame`, senza passare dai
    /// frame sorgente: l'audio segue il taglio al campione.
    pub fn media_secs_at(&self, timeline_frame: FrameIdx, timeline_fps: f64) -> f64 {
        (timeline_frame - self.timeline_start + self.source_offset) as f64 / timeline_fps
    }

    /// Porta la clip da una timeline a `from` fps a una a `to` fps,
    /// conservando i secondi; `rate` è quello della clip sulla nuova
    /// timeline.
    pub fn retime(&mut self, from: Rational, to: Rational, rate: Rational) {
        let end = convert_frames(self.timeline_end(), from, to);
        self.timeline_start = convert_frames(self.timeline_start, from, to);
        self.timeline_len = (end - self.timeline_start).max(1);
        self.source_offset = convert_frames(self.source_offset, from, to);
        self.rate = rate;
    }

    /// Moltiplicatore di opacità/volume a `timeline_frame` per le
    /// dissolvenze: rampa lineare 0→1 su `fade_in` frame dall'inizio,
    /// rampa 1→0 su `fade_out` frame dalla fine, prodotto delle due (se si
    /// sovrappongono si sottraggono a vicenda, come in qualsiasi NLE).
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

    /// Offset di posizione (stesse unità pixel di `Transform.position`,
    /// `frame_size` = risoluzione della timeline) dovuto alle transizioni
    /// push in entrata/uscita a `timeline_frame`. Sommato al `position` già
    /// campionato dai keyframe del transform, non lo sostituisce: così una
    /// transizione push convive con un pan manuale sulla stessa clip. Fuori
    /// dalla finestra della transizione l'offset è zero; alle sue estremità
    /// coincide sempre con "fuori schermo" o "a posto", indipendentemente
    /// da `direction`/`ease`, perché il rilascio dà alpha 0 fuori dai bordi
    /// del source_uv (vedi `transform.wgsl`): usarlo anche per rivelare
    /// quel che sta sotto (lower third, overlay) è lo stesso meccanismo.
    pub fn transition_offset_at(&self, timeline_frame: FrameIdx, frame_size: (f32, f32)) -> [f32; 2] {
        let len = self.timeline_len.max(1);
        let pos = timeline_frame - self.timeline_start;
        let mut offset = [0.0f32; 2];
        if let Some(t) = &self.effects.transition_in {
            let d = t.duration.clamp(1, len);
            if pos >= 0 && pos < d {
                let progress = eased(pos as f32 / d as f32, t.ease, t.curve);
                let vec = t.direction.vector();
                let amount = -(1.0 - progress);
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
                offset[0] += vec[0] * frame_size.0 * progress;
                offset[1] += vec[1] * frame_size.1 * progress;
            }
        }
        offset
    }
}

/// `frames` a `from` fps espressi a `to` fps, arrotondati.
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
    /// Sempre ordinate per `timeline_start`, mai sovrapposte.
    pub clips: Vec<Clip>,
    /// Su una track video: esclusa dal compositing.
    pub muted: bool,
    /// Solo audio: se almeno una track è in solo, suonano solo quelle.
    #[serde(default)]
    pub solo: bool,
    /// Le sue clip non si possono selezionare né modificare, e nulla ci
    /// può atterrare sopra.
    #[serde(default)]
    pub locked: bool,
    /// Transizioni a cavallo tra due clip adiacenti. Non tocca mai
    /// `timeline_start`/`timeline_len` delle clip coinvolte (restano non
    /// sovrapposte, invariante intatto): è il rendering a "prestare" per
    /// la sua finestra la coda di una e la testa dell'altra, vedi
    /// `Track::crossing_at`. Una voce la cui coppia non è più adiacente
    /// (una delle due spostata, tagliata, cancellata) resta nei dati ma
    /// è inerte — non serve ripulirla attivamente.
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

    /// Inserisce mantenendo l'ordine per `timeline_start`.
    pub fn insert_sorted(&mut self, clip: Clip) {
        let pos = self
            .clips
            .partition_point(|c| c.timeline_start < clip.timeline_start);
        self.clips.insert(pos, clip);
    }

    /// La crossing transition la cui coppia è ancora valida (entrambe le
    /// clip esistono e sono ancora adiacenti) e la cui finestra copre
    /// `frame`, con le due clip coinvolte.
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

    /// La crossing transition (valida o no) il cui `left_clip` è `id`.
    pub fn crossing_from(&self, left_clip: ClipId) -> Option<&CrossTransition> {
        self.crossings.iter().find(|c| c.left_clip == left_clip)
    }

    /// La crossing transition (valida o no) il cui `right_clip` è `id`.
    pub fn crossing_into(&self, right_clip: ClipId) -> Option<&CrossTransition> {
        self.crossings.iter().find(|c| c.right_clip == right_clip)
    }
}

/// Una transizione a cavallo tra due clip adiacenti sulla stessa track:
/// "mangia" gli ultimi `duration/2` frame di `left_clip` e i primi
/// `duration/2` di `right_clip`, mostrandoli sovrapposti invece che in
/// sequenza. A differenza di `EffectStack::transition_in`/`transition_out`
/// (un bordo solo, contro il trasparente) qui i lati sono sempre due e
/// reali: nessuna delle due clip cambia `timeline_start`/`timeline_len`,
/// la finestra si calcola sempre dalla loro posizione attuale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossTransition {
    pub left_clip: ClipId,
    pub right_clip: ClipId,
    pub transition: Transition,
}

impl CrossTransition {
    /// Quanti frame della finestra cadono prima del taglio (dentro
    /// `left_clip`) e quanti dopo (dentro `right_clip`): sempre a metà,
    /// l'eventuale frame dispari va al lato sinistro. Il drag della durata
    /// è simmetrico (vedi timeline_ui), quindi la scelta di arrotondamento
    /// conta solo per durate dispari.
    pub fn split(&self) -> (FrameIdx, FrameIdx) {
        Self::split_duration(self.transition.duration)
    }

    /// Come `split`, ma per una durata data invece di `self.transition.duration`
    /// — serve all'anteprima dal vivo di un drag di ridimensionamento, dove
    /// la durata mostrata non è ancora quella salvata.
    pub fn split_duration(duration: FrameIdx) -> (FrameIdx, FrameIdx) {
        let left = duration - duration / 2;
        (left, duration - left)
    }

    /// La finestra (in frame di timeline) della transizione, dati i bordi
    /// attuali di `left`/`right` — mai memorizzata, sempre ricalcolata: se
    /// una clip si sposta la finestra la segue.
    pub fn window(&self, left: &Clip, right: &Clip) -> std::ops::Range<FrameIdx> {
        let (split_left, split_right) = self.split();
        (left.timeline_end() - split_left)..(right.timeline_start + split_right)
    }

    /// Progresso 0..1 di `frame` nella finestra: 0 all'inizio (sinistra
    /// ancora del tutto a posto, destra del tutto fuori), 1 alla fine
    /// (opposto). Applica già `ease`/`curve`.
    pub fn eased_progress_at(&self, frame: FrameIdx, left: &Clip, right: &Clip) -> f32 {
        let window = self.window(left, right);
        let len = (window.end - window.start).max(1);
        let raw = (frame - window.start) as f32 / len as f32;
        eased(raw.clamp(0.0, 1.0), self.transition.ease, self.transition.curve)
    }

    /// Offset di posizione (stesse unità pixel di `Transform.position`) dei
    /// lati sinistro e destro al progresso già "easato" `progress`: il
    /// sinistro esce, il destro entra, nello stesso verso — stessa
    /// matematica di `Clip::transition_offset_at`, qui applicata a un
    /// progresso condiviso dall'intera finestra invece che locale al bordo
    /// di una singola clip.
    pub fn offsets(&self, progress: f32, frame_size: (f32, f32)) -> ([f32; 2], [f32; 2]) {
        let vec = self.transition.direction.vector();
        let left_amount = progress;
        let right_amount = -(1.0 - progress);
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
    /// Ordine di compositing: bottom -> top.
    pub tracks: Vec<Track>,
}

impl Timeline {
    pub fn clip(&self, track_index: usize, id: ClipId) -> Option<&Clip> {
        self.tracks.get(track_index)?.clip(id)
    }

    pub fn clip_mut(&mut self, track_index: usize, id: ClipId) -> Option<&mut Clip> {
        self.tracks.get_mut(track_index)?.clip_mut(id)
    }

    /// Ultimo frame (esclusivo) coperto da una qualunque clip della
    /// timeline, su qualunque track.
    pub fn total_frames(&self) -> FrameIdx {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(Clip::timeline_end)
            .max()
            .unwrap_or(0)
    }

    /// Nome della track come in un NLE: V1, V2… A1, A2…, per tipo.
    pub fn track_label(&self, track_index: usize) -> String {
        let kind = self.tracks[track_index].kind;
        let number = self.tracks[..=track_index].iter().filter(|t| t.kind == kind).count();
        match kind {
            TrackKind::Video => format!("V{number}"),
            TrackKind::Audio => format!("A{number}"),
        }
    }

    /// Le track di tipo `kind`, con il loro indice assoluto in `tracks`.
    pub fn tracks_of_kind(
        &self,
        kind: TrackKind,
    ) -> impl DoubleEndedIterator<Item = (usize, &Track)> {
        self.tracks
            .iter()
            .enumerate()
            .filter(move |(_, t)| t.kind == kind)
    }

    /// Prima track di tipo `kind`.
    pub fn first_track_index(&self, kind: TrackKind) -> Option<usize> {
        self.tracks_of_kind(kind).map(|(i, _)| i).next()
    }

    /// Le clip video che coprono `frame`, una per track, dal basso verso
    /// l'alto (ordine di compositing). Esclude clip e track disattivate.
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

    /// Le track audio che finiscono nel mix: non mute e, se qualcuna è in
    /// solo, solo quelle.
    pub fn audible_tracks(&self) -> impl Iterator<Item = (usize, &Track)> {
        let any_solo = self.tracks_of_kind(TrackKind::Audio).any(|(_, t)| t.solo);
        self.tracks_of_kind(TrackKind::Audio)
            .filter(move |(_, t)| !t.muted && (!any_solo || t.solo))
    }

    pub fn is_locked(&self, track_index: usize) -> bool {
        self.tracks.get(track_index).is_some_and(|t| t.locked)
    }

    /// La prima track di tipo `kind` non bloccata.
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

    /// Le clip di `group` su tutte le track. Uno scan: si chiama solo su
    /// interazioni utente.
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

    /// Gli altri membri del gruppo collegato di una clip, lei esclusa.
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

    /// Ricalcola `Clip::rate` dagli fps. `source_offset`/`timeline_len` sono
    /// in frame di timeline e non cambiano.
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
        };
        let mid = Transform::lerp(&a, &b, 0.5);
        assert_eq!(mid.crop, [0.1, 0.1, 0.4, 0.4]);
        assert_eq!(mid.crop_softness, 0.2);
        assert_eq!(mid.zoom, [2.0, 3.0]);
        assert_eq!(mid.position, [0.5, -0.5]);
        assert_eq!(mid.rotation, 45.0);
        assert_eq!(mid.anchor, [0.1, 0.2]);
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
        // Due track video: la prima (indice 0, "bottom") copre [0, 30), la
        // seconda (indice 1, "top") solo [10, 20) — un layer più corto
        // sovrapposto a uno più lungo, il caso base del blend-over
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
                    clips: vec![clip_at(15, 10, 2)], // finisce a 25, più avanti della video
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

    /// 59,94 fps su una timeline a 60: la clip dura in timeline quanto
    /// dura davvero (0,1% in più di frame), non 1:1 come i frame sorgente.
    #[test]
    fn a_clip_slower_than_the_timeline_lasts_longer_in_timeline_frames() {
        let rate = Rational::conform_rate(Rational::new(60, 1), Rational::new(60000, 1001));
        assert_eq!(rate, Rational::new(1001, 1000));

        // 15 minuti di sorgente a 59,94 fps.
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

        // Nessuna deriva accumulata: a ogni istante il frame sorgente
        // mostrato resta quello che compete al tempo reale trascorso
        // (tolleranza di mezzo frame, l'arrotondamento inevitabile).
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
        // 50 fps su timeline a 25: due frame sorgente per frame timeline.
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
        // 25 fps su timeline a 30: 6 frame di timeline ogni 5 sorgente.
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
}
