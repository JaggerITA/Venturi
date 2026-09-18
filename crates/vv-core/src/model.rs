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

/// Id di un gruppo di clip collegate (vedi `Clip::linked_group`): un
/// contatore semplice come `ClipId`, non una chiave slotmap (nessuna arena
/// dedicata — l'appartenenza al gruppo è solo questo campo su ogni `Clip`,
/// niente registro separato da tenere sincronizzato).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkGroupId(pub u64);

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
    /// Quanto tagliare da ciascun lato (sinistra, alto, destra, basso), in
    /// pixel del media alla sua risoluzione *nativa* (non del proxy): `0` =
    /// quel lato non è tagliato. Il crop
    /// taglia e basta — quel che resta non viene ricentrato né ingrandito,
    /// continua a cadere dov'era nel frame di output (dietro la parte
    /// tagliata si vede il layer sotto).
    pub crop: [f32; 4], // left, top, right, bottom
    /// Sfumatura del bordo di crop, in pixel del media:
    /// negativa la sfuma verso l'interno del crop, positiva verso l'esterno
    /// (quindi si vede solo dove qualcosa è stato tagliato), `0` taglio
    /// netto.
    pub crop_softness: f32,
    /// Ingrandimento della clip *rispetto al frame di output*, per asse
    /// (X, Y), attorno all'`anchor`: con abbastanza zoom una clip di aspect
    /// ratio diverso da quello della timeline arriva a coprirlo tutto,
    /// bande comprese.
    pub zoom: [f32; 2],
    /// Spostamento della clip dentro il frame di output, in pixel di
    /// timeline (X positivo = verso destra, Y positivo = verso l'alto): su
    /// una timeline 1920x1080, `X = 960` la sposta di mezzo frame. Non
    /// sposta il contenuto dentro la clip.
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

/// Un parametro scalare del transform: ognuno ha i propri keyframe, come
/// nell'inspector di un NLE (il `Transform` a un dato frame è la
/// valutazione di tutti insieme, vedi [`TransformTracks::value_at`]).
/// `flip` non è qui: non ha valori intermedi da interpolare.
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

    /// Posizione in `TransformTracks::params` — l'ordine di `ALL`.
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).expect("ALL li elenca tutti")
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
            .filter_map(|p| {
                self.track(*p)
                    .keyframes()
                    .iter()
                    .rev()
                    .find(|(f, _, _)| *f < frame)
                    .map(|(f, _, _)| *f)
            })
            .max()
    }

    /// Simmetrica di `previous_keyframe`, in avanti.
    pub fn next_keyframe(&self, params: &[TransformParam], frame: FrameIdx) -> Option<FrameIdx> {
        params
            .iter()
            .filter_map(|p| {
                self.track(*p)
                    .keyframes()
                    .iter()
                    .find(|(f, _, _)| *f > frame)
                    .map(|(f, _, _)| *f)
            })
            .min()
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
    pub transform: TransformTracks,
    pub speed: Keyframed<f32>,
    pub gain_db: Keyframed<f32>,
    pub text: Vec<TextOverlay>,
    pub color: Option<Keyframed<Rgba>>,
}

impl Default for EffectStack {
    fn default() -> Self {
        Self {
            transform: TransformTracks::default(),
            speed: Keyframed::constant(1.0),
            gain_db: Keyframed::constant(0.0),
            text: Vec::new(),
            color: None,
        }
    }
}

impl EffectStack {
    /// `true` se nessuna proprietà è stata toccata rispetto al default: la
    /// timeline disegna più scure le clip per cui è `false`.
    pub fn is_pristine(&self) -> bool {
        self.transform.is_pristine()
            && self.speed.is_constant()
            && self.speed.default == 1.0
            && self.gain_db.is_constant()
            && self.gain_db.default == 0.0
            && self.text.is_empty()
            && self.color.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub source: ClipSource,
    /// Inizio della clip nel media, in frame di *timeline* contati dal
    /// frame sorgente 0 (lo spazio di `Rational::scale_round`). Non un
    /// frame sorgente: una clip conformata può cominciare a metà di un
    /// frame sorgente (dopo uno split o un trim), e un indice di frame
    /// sorgente perderebbe quella fase.
    pub source_offset: FrameIdx,
    /// Posizione nello spazio della Timeline che contiene questa clip.
    pub timeline_start: FrameIdx,
    pub timeline_len: FrameIdx,
    pub effects: EffectStack,
    /// Gruppo di clip collegate (tipicamente video + tutti gli stream
    /// audio dello stesso media, collegate di default all'import — un
    /// media può averne più di uno, vedi `audio_stream_index`): selezione,
    /// drag e cancellazione trattano l'intero gruppo come un'unità, non
    /// solo una coppia. `None` per una clip indipendente. "Scollega"
    /// scioglie l'intero gruppo (tutti i membri a `None`), non solo la
    /// clip su cui è stato invocato (vedi `UnlinkClip`).
    /// `#[serde(default)]`: i progetti salvati prima di questo campo
    /// (quando il collegamento era una coppia `Option<ClipId>`) caricano
    /// tutte le clip come indipendenti — nessun modo di ricostruire i
    /// vecchi collegamenti da un formato diverso, ma comunque un
    /// downgrade "sicuro" (l'utente può ricollegarle a mano).
    #[serde(default)]
    pub linked_group: Option<LinkGroupId>,
    /// Per una clip audio (`source: ClipSource::Media`, su una track
    /// `TrackKind::Audio`): indice dello stream audio nel contenitore del
    /// media (0 = primo stream audio, stesso ordine di
    /// `vv_media::probe::audio_streams`). Ignorato per le clip video. Un
    /// media con un solo stream audio (il caso comune) usa sempre 0; un
    /// media con più stream audio (es. mix stereo *e* 5.1 separato) viene
    /// importato con una clip per stream, ciascuna col proprio indice —
    /// vedi `VibeVideoApp::insert_media_clip`. Default 0 per i progetti
    /// salvati prima che questo campo esistesse (un solo stream audio per
    /// clip, stesso comportamento di oggi).
    #[serde(default)]
    pub audio_stream_index: usize,
    /// Frame di timeline per frame sorgente: `fps timeline / fps media`
    /// (`Rational::conform_rate`). `1/1` per SolidColor e per un media
    /// allo stesso fps della timeline. Serve solo a tradurre
    /// `source_offset`/`timeline_len` in frame sorgente.
    #[serde(default = "Rational::one")]
    pub rate: Rational,
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

    /// Mappa una posizione di *timeline* (deve cadere dentro l'intervallo
    /// di questa clip, `timeline_start..timeline_end()` — il chiamante lo
    /// garantisce risolvendo `Timeline::active_clip_at` prima di
    /// chiamare) al frame *sorgente* corrispondente (fps nativo del
    /// media). Unica funzione per questa mappatura: prima era scritta
    /// separatamente nel walker di prefetch dell'anteprima
    /// (`vv-app::render_ahead::collect_media_segments`) e nel loop di
    /// export (`vv-app::export::render_video_frame`), identiche solo
    /// perché coincidono quando `effects.speed == 1` — al primo
    /// time-remap reale (milestone 7, `EffectStack::speed` non ancora
    /// applicato qui) sarebbero divergenti senza un posto unico da
    /// cambiare. Quel cambio va qui, non ai due chiamanti.
    pub fn source_frame_at(&self, timeline_frame: FrameIdx) -> FrameIdx {
        self.rate
            .unscale_round(timeline_frame - self.timeline_start + self.source_offset)
    }

    /// Inverso di `source_frame_at`: dove cade `source_frame` sulla
    /// timeline. Accetta anche frame fuori dal trim della clip (serve ai
    /// limiti di trim: dove cadrebbe il frame 0, o l'ultimo frame reale
    /// del media, se la clip fosse allungata fin lì).
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

impl Timeline {
    /// La clip attiva su `track_index` al frame `frame`, se c'è.
    pub fn active_clip_at(&self, track_index: usize, frame: FrameIdx) -> Option<&Clip> {
        self.tracks
            .get(track_index)?
            .clips
            .iter()
            .find(|c| frame >= c.timeline_start && frame < c.timeline_end())
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

    /// Le track di tipo `kind`, con il loro indice in `tracks` — l'indice
    /// è quello che il resto dell'API (`active_clip_at`, i comandi) si
    /// aspetta, non una posizione "solo tra le track di questo tipo".
    pub fn tracks_of_kind(
        &self,
        kind: TrackKind,
    ) -> impl DoubleEndedIterator<Item = (usize, &Track)> {
        self.tracks
            .iter()
            .enumerate()
            .filter(move |(_, t)| t.kind == kind)
    }

    /// Indice della prima (bottom-most) track di tipo `kind`, o `None` se
    /// non ce n'è nessuna — dove atterra di default una clip nuova (media
    /// trascinato dal pool, SolidColor), finché l'utente non aggiunge altre
    /// track a mano (REFACTOR_PIPELINE.md B4).
    pub fn first_track_index(&self, kind: TrackKind) -> Option<usize> {
        self.tracks_of_kind(kind).map(|(i, _)| i).next()
    }

    /// La clip Video attiva al frame `frame`, sulla track video più in alto
    /// (indice più alto: l'ordine di `tracks` è bottom->top) tra quelle che
    /// ne hanno una in quel punto. Con una sola track video (caso comune)
    /// coincide con `active_clip_at` su quella track; con più track video
    /// generalizza "quale si vede": senza un'opacità per-clip (non ancora
    /// modellata in `EffectStack`), un vero accumulo alpha su più layer
    /// opachi collassa esattamente in "il più in alto che c'è in quel
    /// punto" — REFACTOR_PIPELINE.md B4, vedi anche
    /// `vv_render::Compositor` (ancora un solo frame in ingresso: qui è
    /// dove si sceglie *quale*, non nel compositor).
    /// Tutte le clip video che coprono `frame`, una per track, dal basso
    /// verso l'alto: l'ordine di compositing (l'ultima è quella in cima).
    /// Con una clip che non riempie il frame di output — aspect ratio
    /// diverso da quello della timeline, vedi il letterbox in
    /// `vv_render` — sotto le sue bande si vedono i layer precedenti.
    pub fn active_video_clips_at(&self, frame: FrameIdx) -> Vec<(usize, &Clip)> {
        self.tracks_of_kind(TrackKind::Video)
            .filter_map(|(i, t)| {
                t.clips
                    .iter()
                    .find(|c| frame >= c.timeline_start && frame < c.timeline_end())
                    .map(|c| (i, c))
            })
            .collect()
    }

    pub fn active_video_clip_at(&self, frame: FrameIdx) -> Option<(usize, &Clip)> {
        self.tracks_of_kind(TrackKind::Video)
            .rev()
            .find_map(|(i, t)| {
                t.clips
                    .iter()
                    .find(|c| frame >= c.timeline_start && frame < c.timeline_end())
                    .map(|c| (i, c))
            })
    }

    /// Il `timeline_start` più vicino, a `frame` o dopo, tra tutte le clip
    /// di tutte le track Video — "quando ricompare qualcosa da mostrare"
    /// dopo un vuoto, a prescindere da su quale track video sia.
    pub fn next_video_clip_start_from(&self, frame: FrameIdx) -> Option<FrameIdx> {
        self.tracks_of_kind(TrackKind::Video)
            .flat_map(|(_, t)| t.clips.iter())
            .filter(|c| c.timeline_start >= frame)
            .map(|c| c.timeline_start)
            .min()
    }

    /// Il `timeline_end()` più vicino, a `frame` o prima, tra tutte le
    /// clip di tutte le track Video — simmetrico a
    /// `next_video_clip_start_from`, usato per camminare *all'indietro*
    /// (buffer dietro la testina, REFACTOR_PIPELINE.md, vedi
    /// `render_ahead::collect_media_segments_behind`): "dov'è la fine
    /// della clip più vicina saltando all'indietro un vuoto".
    pub fn previous_video_clip_end_before(&self, frame: FrameIdx) -> Option<FrameIdx> {
        self.tracks_of_kind(TrackKind::Video)
            .flat_map(|(_, t)| t.clips.iter())
            .map(Clip::timeline_end)
            .filter(|&end| end <= frame)
            .max()
    }

    /// La clip con questo id, su una qualunque track, insieme all'indice
    /// della sua track — permette di ritrovare una clip nota per id senza
    /// dover tenere traccia (letteralmente) di quale track la contiene, che
    /// con più track video può cambiare da un frame all'altro
    /// (`active_video_clip_at`).
    pub fn find_clip(&self, clip_id: ClipId) -> Option<(usize, &Clip)> {
        self.tracks
            .iter()
            .enumerate()
            .find_map(|(i, t)| t.clips.iter().find(|c| c.id == clip_id).map(|c| (i, c)))
    }

    /// Tutte le clip (con il loro indice di track) che condividono
    /// `group`, su qualunque track — un semplice scan, non un indice
    /// secondario da tenere sincronizzato (vedi doc di `LinkGroupId`): la
    /// timeline è dell'ordine di decine/centinaia di clip, e questo si
    /// chiama solo da un'interazione utente (click, inizio drag), mai per
    /// frame.
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

    /// Ricalcola `Clip::rate` di ogni clip Media dall'fps del suo media e
    /// da quello della timeline che la contiene. Chiamata al caricamento
    /// (`load_project`). `source_offset`/`timeline_len` restano: sono in
    /// frame di timeline, cioè secondi, e non dipendono dall'fps del media.
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
    fn active_clip_at_finds_the_covering_clip_and_none_in_a_gap() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track {
                kind: TrackKind::Video,
                clips: vec![clip_at(0, 10, 1), clip_at(20, 5, 2)],
                muted: false,
            }],
        };
        assert_eq!(tl.active_clip_at(0, 5).map(|c| c.id), Some(ClipId(1)));
        assert!(tl.active_clip_at(0, 15).is_none(), "buco tra le due clip");
        assert_eq!(tl.active_clip_at(0, 20).map(|c| c.id), Some(ClipId(2)));
        assert!(
            tl.active_clip_at(0, 25).is_none(),
            "oltre la fine dell'ultima clip"
        );
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
                },
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(10, 10, 2)],
                    muted: false,
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
                },
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(10, 10, 2)],
                    muted: false,
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
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![clip_at(0, 10, 2)],
                    muted: false,
                },
            ],
        };
        assert_eq!(
            tl.active_video_clip_at(5).map(|(_, c)| c.id),
            Some(ClipId(1))
        );
    }

    #[test]
    fn next_video_clip_start_from_finds_the_closest_across_video_tracks() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(50, 10, 1)],
                    muted: false,
                },
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(20, 10, 2)],
                    muted: false,
                },
            ],
        };
        assert_eq!(tl.next_video_clip_start_from(0), Some(20));
        assert_eq!(tl.next_video_clip_start_from(25), Some(50));
        assert_eq!(tl.next_video_clip_start_from(61), None);
    }

    #[test]
    fn previous_video_clip_end_before_finds_the_closest_across_video_tracks() {
        // clip_at(20,10,2) -> [20,30), clip_at(50,10,1) -> [50,60).
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(50, 10, 1)],
                    muted: false,
                },
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(20, 10, 2)],
                    muted: false,
                },
            ],
        };
        assert_eq!(tl.previous_video_clip_end_before(100), Some(60));
        assert_eq!(tl.previous_video_clip_end_before(60), Some(60));
        assert_eq!(tl.previous_video_clip_end_before(59), Some(30));
        assert_eq!(tl.previous_video_clip_end_before(19), None);
    }

    #[test]
    fn find_clip_locates_a_clip_by_id_on_any_track() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![
                Track {
                    kind: TrackKind::Video,
                    clips: vec![clip_at(0, 10, 1)],
                    muted: false,
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![clip_at(0, 10, 2)],
                    muted: false,
                },
            ],
        };
        assert_eq!(tl.find_clip(ClipId(1)).map(|(t, _)| t), Some(0));
        assert_eq!(tl.find_clip(ClipId(2)).map(|(t, _)| t), Some(1));
        assert!(tl.find_clip(ClipId(99)).is_none());
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
    fn active_clip_at_ignores_other_tracks_and_out_of_range_indices() {
        let tl = Timeline {
            name: "T".into(),
            fps: Rational::new(25, 1),
            resolution: (1920, 1080),
            tracks: vec![Track {
                kind: TrackKind::Video,
                clips: vec![clip_at(0, 10, 1)],
                muted: false,
            }],
        };
        assert!(tl.active_clip_at(1, 5).is_none(), "track inesistente");
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
                },
                Track {
                    kind: TrackKind::Audio,
                    clips: vec![clip_at(15, 10, 2)], // finisce a 25, più avanti della video
                    muted: false,
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
                has_audio: false,
                sample_rate: 0,
                channels: 0,
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
