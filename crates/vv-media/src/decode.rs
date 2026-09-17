//! Decodifica sequenziale/seek per singola clip.
//!
//! `Decoder` apre lo stream video di un media e permette di decodificare
//! frame in sequenza (`next_frame`) o dopo un seek (`seek_to_time`).
//! `sws_scale` normalizza *qualunque* formato pixel/profondità/sottocampionamento
//! in ingresso (YUV420P, NV12, YUV422, 10-bit, ecc.) a YUV420P planare 8-bit: la
//! conversione YUV→RGB *finale* resta sulla GPU, nello shader del
//! compositor (REFACTOR_PIPELINE.md B3) — qui si esce con tre piani
//! densi (Y/U/V), ~2.5× meno byte per frame in cache di un RGBA.
//!
//! La matrice di conversione (BT.601/709/2020) e il range (limited/full)
//! vanno letti dal frame *decodificato originale* (prima dello scaling:
//! `sws_scale` per un target YUV420P riformatta i piani ma non
//! reinterpreta la semantica colore, quindi i metadati del sorgente
//! restano validi per l'output) — un file che non li segnala esplicitamente
//! (comune) ricade su un'euristica standard basata sulla risoluzione
//! (vedi `guess_matrix`), la stessa convenzione usata da ffmpeg/dai
//! player più comuni.

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::color;
use ffmpeg_next as ffmpeg;
use std::path::Path;
use vv_core::FrameIdx;

/// Matrice di conversione YUV→RGB da applicare nello shader del
/// compositor (REFACTOR_PIPELINE.md B3) — i coefficienti Kr/Kb vivono
/// lì, qui è solo la selezione. BT2020 è usata *solo* se segnalata
/// esplicitamente dal sorgente, mai indovinata dall'euristica per
/// risoluzione (troppo rara/specifica per un fallback sicuro).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Frame decodificato in YUV420P planare 8-bit: tre piani densi
/// (stride == larghezza del piano, niente padding — vedi
/// `yuv420_from_decoded`), con i metadati colore necessari per
/// convertirlo in RGB correttamente (REFACTOR_PIPELINE.md B3).
pub struct FrameYuv420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    /// Piani U/V sottocampionati 4:2:0: dimensioni `plane_width(1)` x
    /// `plane_height(1)` del frame scalato (ffmpeg arrotonda per
    /// eccesso su dimensioni dispari, non un semplice `width/2`).
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub u_width: u32,
    pub u_height: u32,
    pub matrix: ColorMatrix,
    /// `true` = range JPEG/full (0-255), `false` = range MPEG/limited
    /// (16-235 luma, 16-240 croma) — quest'ultimo è la norma per
    /// contenuti video, l'euristica per `Range::Unspecified` assume
    /// limited (vedi `finish_frame`).
    pub full_range: bool,
}

/// Spazio colore da usare quando il sorgente non lo segnala
/// esplicitamente (`color::Space::Unspecified`, comune: molti encoder
/// non lo scrivono) — soglia per risoluzione, la stessa convenzione
/// usata da ffmpeg e dalla maggior parte dei player: SD (sotto 720
/// righe) è quasi sempre BT.601, HD e oltre BT.709. BT.2020 non è mai
/// indovinata qui, solo se il sorgente la segnala esplicitamente.
fn guess_matrix(space: color::Space, height: u32) -> ColorMatrix {
    match space {
        color::Space::BT709 => ColorMatrix::Bt709,
        color::Space::BT2020NCL | color::Space::BT2020CL => ColorMatrix::Bt2020,
        // SMPTE170M/BT470BG sono la stessa matrice "BT.601" (Kr/Kb
        // identici, solo lo standard di origine NTSC/PAL cambia nome).
        // Qualunque altra matrice segnalata esplicitamente (rara: YCGCO,
        // SMPTE240M, ICTCP...) non ha un equivalente qui — ricade
        // sull'euristica per risoluzione invece di un calcolo palesemente
        // sbagliato con un'altra matrice arbitraria.
        color::Space::SMPTE170M | color::Space::BT470BG => ColorMatrix::Bt601,
        _ => {
            if height >= 720 {
                ColorMatrix::Bt709
            } else {
                ColorMatrix::Bt601
            }
        }
    }
}

/// Decoder aperto su un singolo stream video di un media. Non `Sync`: ogni
/// thread di decode (es. il decode-ahead in `playback`) ne possiede una
/// istanza propria.
pub struct Decoder {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::codec::decoder::Video,
    scaler: Scaler,
    video_stream_index: usize,
    time_base: ffmpeg::Rational,
    fps: vv_core::Rational,
    /// `true` dopo che `send_eof()` è stato chiamato: ffmpeg rifiuta un
    /// secondo `send_eof()` prima di un flush, quindi da qui in poi
    /// `next_frame` deve solo drenare `receive_frame` senza rileggere
    /// pacchetti né richiamare `send_eof` di nuovo.
    eof_sent: bool,
    /// Frame già decodificato durante `seek_to_time` (per verificarne
    /// l'atterraggio, vedi la sua doc) e non ancora restituito al
    /// chiamante — `next_frame` lo consuma per primo invece di leggere
    /// un nuovo pacchetto, così un seek non "perde" il frame su cui ha
    /// già pagato il costo di decodifica per il controllo.
    pending: Option<(FrameIdx, FrameYuv420)>,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self, crate::MediaError> {
        crate::probe::ensure_init();

        let ictx = ffmpeg::format::input(&path)?;
        let video_stream = ictx
            .streams()
            .best(Type::Video)
            .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
        let video_stream_index = video_stream.index();
        let time_base = video_stream.time_base();
        let rate = video_stream.rate();
        let fps = vv_core::Rational::new(rate.numerator(), rate.denominator());

        let mut decoder_ctx =
            ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?.decoder();
        // Multi-threading a livello di frame (auto-detect del numero di
        // thread in base ai core disponibili, `count: 0`): senza questo
        // libavcodec decodifica a un solo thread per default. Il costo di
        // un seek è "decodifica in sequenza dall'ultimo keyframe fino al
        // target" (il GOP può arrivare a centinaia di frame): con più
        // frame indipendenti decodificati in parallelo quell'attraversamento
        // è più volte più veloce, a correttezza invariata — nessun frame
        // saltato o approssimato, solo più thread al lavoro sugli stessi
        // identici frame (bug segnalato: "lo scrubbing non è reattivo... il
        // cambio clip resta in freeze per ~1s").
        decoder_ctx.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Frame,
            count: 0,
            ..Default::default()
        });
        let decoder = decoder_ctx.video()?;

        let scaler = Scaler::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::YUV420P,
            decoder.width(),
            decoder.height(),
            Flags::BILINEAR,
        )?;

        Ok(Self {
            ictx,
            decoder,
            scaler,
            video_stream_index,
            time_base,
            fps,
            eof_sent: false,
            pending: None,
        })
    }

    pub fn width(&self) -> u32 {
        self.decoder.width()
    }

    pub fn height(&self) -> u32 {
        self.decoder.height()
    }

    pub fn fps(&self) -> vv_core::Rational {
        self.fps
    }

    /// Seek al keyframe più vicino con timestamp <= `secs`, *garantito*:
    /// non basta chiedere ad `avformat_seek_file` di restare entro
    /// `max_ts = ts` (provato, vedi sotto) — su un file reale con
    /// B-frame (bug confermato dall'utente su un 1080p60fps: seek a
    /// `target=749` atterrava su idx=751, il keyframe *successivo*, non
    /// su 501 come dovrebbe) il demuxer mov/mp4 lo ignora comunque, non
    /// solo in teoria ma osservato in pratica con un test dedicato.
    /// `position_decoder` (`vv-app`) si aspetta un atterraggio `<=
    /// segment_start` per poter poi decodificare in avanti fino a
    /// raggiungerlo: un atterraggio troppo avanti salta silenziosamente
    /// dei frame che nessuno decodifica più — un buco permanente nella
    /// cache (mai altrimenti raggiunto, né prima né dopo), non solo un
    /// frame sprecato.
    ///
    /// Quindi: decodifica subito il frame su cui il seek atterra (non
    /// lazy come prima — il chiamante lo avrebbe comunque richiesto
    /// súbito dopo con `next_frame`, bufferizzato qui in `pending`) e,
    /// se il suo idx supera il target, riprova un secondo più indietro
    /// (raddoppiando il passo a ogni tentativo, fino a un tetto di
    /// sicurezza): un GOP reale anche lungo (minuti, non secondi) resta
    /// coperto in pochissimi tentativi grazie al raddoppio, senza dover
    /// conoscere in anticipo la vera posizione del keyframe precedente.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<(), crate::MediaError> {
        let target_idx = (secs.max(0.0) * self.fps.as_f64()).round() as FrameIdx;
        let mut ts = (secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
        // Un secondo in unità AV_TIME_BASE: passo iniziale del backoff,
        // raddoppiato ad ogni tentativo — arriva a coprire un GOP di
        // diversi minuti in una decina di tentativi senza doverne
        // conoscere la lunghezza reale in anticipo.
        let mut step = i64::from(ffmpeg::ffi::AV_TIME_BASE);
        const MAX_RETRIES: u32 = 20;
        for _ in 0..MAX_RETRIES {
            self.ictx.seek(ts, ..ts)?;
            self.decoder.flush();
            self.eof_sent = false;
            self.pending = None;
            match self.decode_next_frame()? {
                Some((idx, frame)) if idx > target_idx && ts > 0 => {
                    ts = ts.saturating_sub(step).max(0);
                    step = step.saturating_mul(2);
                    let _ = frame; // scartato, si riprova più indietro
                }
                landed => {
                    self.pending = landed;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Decodifica il prossimo frame video disponibile in ordine di
    /// presentazione. `Ok(None)` a fine stream.
    pub fn next_frame(&mut self) -> Result<Option<(FrameIdx, FrameYuv420)>, crate::MediaError> {
        if let Some(landed) = self.pending.take() {
            return Ok(Some(landed));
        }
        self.decode_next_frame()
    }

    /// Nucleo di `next_frame`, senza passare dal buffer `pending` — solo
    /// `seek_to_time` lo chiama direttamente (per decodificare e
    /// verificare l'atterraggio *prima* di bufferlo in `pending`, vedi la
    /// sua doc); `next_frame` pubblico lo richiama solo dopo aver già
    /// controllato `pending`.
    fn decode_next_frame(&mut self) -> Result<Option<(FrameIdx, FrameYuv420)>, crate::MediaError> {
        let mut decoded = ffmpeg::frame::Video::empty();

        // Stream già esaurito in una chiamata precedente: non si può
        // richiamare `send_eof()` una seconda volta (ffmpeg lo rifiuta
        // prima di un flush), quindi ci limitiamo a drenare eventuali
        // frame bufferizzati residui, uno a chiamata.
        if self.eof_sent {
            return Ok(if self.decoder.receive_frame(&mut decoded).is_ok() {
                Some(self.finish_frame(&decoded)?)
            } else {
                None
            });
        }

        let mut packet = ffmpeg::Packet::empty();

        loop {
            match packet.read(&mut self.ictx) {
                Ok(()) => {
                    if packet.stream() != self.video_stream_index {
                        continue;
                    }
                    self.decoder.send_packet(&packet)?;
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        return Ok(Some(self.finish_frame(&decoded)?));
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof()?;
                    self.eof_sent = true;
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        return Ok(Some(self.finish_frame(&decoded)?));
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn finish_frame(
        &mut self,
        decoded: &ffmpeg::frame::Video,
    ) -> Result<(FrameIdx, FrameYuv420), crate::MediaError> {
        let pts = decoded.pts().unwrap_or(0);
        let secs =
            pts as f64 * self.time_base.numerator() as f64 / self.time_base.denominator() as f64;
        let idx = (secs * self.fps.as_f64()).round() as FrameIdx;
        // Letti dal frame *originale* (prima dello scaling): sws_scale
        // per un target YUV420P riformatta i piani ma non ne reinterpreta
        // la semantica colore, i metadati del sorgente restano validi
        // (vedi doc di modulo).
        let matrix = guess_matrix(decoded.color_space(), decoded.height());
        let full_range = decoded.color_range() == color::Range::JPEG;
        let frame = yuv420_from_decoded(&mut self.scaler, decoded, matrix, full_range)?;
        Ok((idx, frame))
    }
}

fn yuv420_from_decoded(
    scaler: &mut Scaler,
    decoded: &ffmpeg::frame::Video,
    matrix: ColorMatrix,
    full_range: bool,
) -> Result<FrameYuv420, crate::MediaError> {
    let mut scaled = ffmpeg::frame::Video::empty();
    scaler.run(decoded, &mut scaled)?;

    // sws_scale può restituire righe con padding (stride > row_bytes)
    // su ciascun piano: ricompattiamo per avere buffer densi da caricare
    // come texture, un piano alla volta (`plane_width`/`plane_height`
    // riflettono le dimensioni reali allocate da ffmpeg per quel piano,
    // niente calcoli a mano su arrotondamenti di sottocampionamento).
    let pack_plane = |index: usize| -> Vec<u8> {
        let w = scaled.plane_width(index) as usize;
        let h = scaled.plane_height(index) as usize;
        let stride = scaled.stride(index);
        let plane = scaled.data(index);
        let mut out = Vec::with_capacity(w * h);
        for row in 0..h {
            let start = row * stride;
            out.extend_from_slice(&plane[start..start + w]);
        }
        out
    };

    let width = scaled.width();
    let height = scaled.height();
    let u_width = scaled.plane_width(1);
    let u_height = scaled.plane_height(1);
    let y = pack_plane(0);
    let u = pack_plane(1);
    let v = pack_plane(2);

    Ok(FrameYuv420 {
        width,
        height,
        y,
        u,
        v,
        u_width,
        u_height,
        matrix,
        full_range,
    })
}

pub fn decode_first_frame(path: &Path) -> Result<FrameYuv420, crate::MediaError> {
    let mut decoder = Decoder::open(path)?;
    decoder
        .next_frame()?
        .map(|(_, frame)| frame)
        .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn guess_matrix_uses_the_signaled_space_when_present() {
        assert_eq!(guess_matrix(color::Space::BT709, 240), ColorMatrix::Bt709);
        assert_eq!(
            guess_matrix(color::Space::BT2020NCL, 240),
            ColorMatrix::Bt2020
        );
        assert_eq!(
            guess_matrix(color::Space::SMPTE170M, 1080),
            ColorMatrix::Bt601,
            "una matrice SD segnalata esplicitamente vince sull'euristica per risoluzione"
        );
    }

    #[test]
    fn guess_matrix_falls_back_to_a_resolution_heuristic_when_unspecified() {
        assert_eq!(
            guess_matrix(color::Space::Unspecified, 240),
            ColorMatrix::Bt601,
            "SD non segnalato: BT.601"
        );
        assert_eq!(
            guess_matrix(color::Space::Unspecified, 1080),
            ColorMatrix::Bt709,
            "HD non segnalato: BT.709"
        );
    }

    #[test]
    fn guess_matrix_never_guesses_bt2020_from_the_heuristic() {
        // BT.2020 è troppo specifica per essere indovinata: anche a
        // risoluzioni UHD, senza segnalazione esplicita si resta su
        // BT.709, mai BT.2020.
        assert_eq!(
            guess_matrix(color::Space::Unspecified, 2160),
            ColorMatrix::Bt709
        );
    }

    fn make_test_clip(name: &str, duration_secs: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("vv-media-decode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);

        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-g",
                "10", // GOP corto: forza più keyframe, utile per testare il seek
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());
        path
    }

    fn make_test_clip_with_gop_and_bframes(
        name: &str,
        duration_secs: u32,
        gop: u32,
        bframes: u32,
    ) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("vv-media-decode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);

        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-g",
                &gop.to_string(),
                "-keyint_min",
                &gop.to_string(),
                "-bf",
                &bframes.to_string(),
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());
        path
    }

    #[test]
    #[ignore = "misurazione manuale, non una asserzione di correttezza"]
    fn bench_decode_forward_through_a_large_gop() {
        let dir = std::env::temp_dir().join("vv-media-decode-bench");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large_gop.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=1920x1080:rate=25:duration=12",
                "-c:v",
                "libx264",
                "-preset",
                "medium",
                "-g",
                "250",
                "-keyint_min",
                "250",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let mut decoder = Decoder::open(&path).expect("apertura fallita");
        decoder.seek_to_time(0.0).expect("seek fallito");
        let start = std::time::Instant::now();
        let mut count = 0;
        while count < 249 {
            match decoder.next_frame().expect("decode fallito") {
                Some(_) => count += 1,
                None => break,
            }
        }
        eprintln!(
            "decodificati {count} frame (1920x1080) in {:?} ({:.1} fps)",
            start.elapsed(),
            count as f64 / start.elapsed().as_secs_f64()
        );
    }

    #[test]
    fn decode_first_frame_of_x264_reads_correct_dimensions_and_pixels() {
        let path = make_test_clip("sample.mp4", 1);

        let frame = decode_first_frame(&path).expect("decode fallito");
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.y.len(), 320 * 240, "piano Y denso, 1 byte/pixel");
        // 4:2:0: piani croma a metà risoluzione (arrotondata per eccesso,
        // qui esatta perché 320x240 è già pari).
        assert_eq!(frame.u_width, 160);
        assert_eq!(frame.u_height, 120);
        assert_eq!(frame.u.len(), 160 * 120);
        assert_eq!(frame.v.len(), 160 * 120);

        // Il pattern testsrc non è mai uniforme: se troviamo più di un
        // valore distinto nel piano Y, lo stride/formato sono corretti.
        let distinct: std::collections::HashSet<u8> = frame.y.iter().step_by(37).copied().collect();
        assert!(
            distinct.len() > 5,
            "i pixel decodificati sembrano degeneri: {distinct:?}"
        );
    }

    #[test]
    fn decode_first_frame_defaults_to_mpeg_limited_range_and_a_resolution_based_matrix() {
        // make_test_clip non segnala esplicitamente matrice/range (comune
        // per contenuti generati/consumer): 320x240 è sotto la soglia
        // 720p, deve ricadere su BT.601 + limited range.
        let path = make_test_clip("colorspace.mp4", 1);
        let frame = decode_first_frame(&path).expect("decode fallito");
        assert_eq!(frame.matrix, ColorMatrix::Bt601);
        assert!(
            !frame.full_range,
            "il range di default per video deve essere limited (MPEG), non full"
        );
    }

    #[test]
    fn decoder_sequential_frames_have_increasing_index() {
        let path = make_test_clip("sequential.mp4", 1);
        let mut decoder = Decoder::open(&path).unwrap();

        let mut last_idx = -1;
        let mut count = 0;
        while let Some((idx, _)) = decoder.next_frame().unwrap() {
            assert!(idx > last_idx, "gli indici di frame devono crescere");
            last_idx = idx;
            count += 1;
        }
        // ~25fps per 1s: qualche frame di tolleranza sull'ultimo GOP.
        assert!((20..=30).contains(&count), "count={count}");
    }

    #[test]
    fn decoder_seek_lands_near_target_and_decodes_forward() {
        let path = make_test_clip("seek.mp4", 3);
        let mut decoder = Decoder::open(&path).unwrap();

        decoder.seek_to_time(1.5).unwrap();
        let (idx, _) = decoder
            .next_frame()
            .unwrap()
            .expect("doveva esserci un frame dopo il seek");

        // Il seek atterra al keyframe <= target: con GOP=10 a 25fps la
        // distanza dal frame 1.5s*25=37 non supera un GOP.
        assert!(idx <= 37, "idx={idx} dovrebbe essere <= al target");
        assert!(idx >= 37 - 10, "idx={idx} troppo lontano dal target");
    }

    /// Regressione per un bug reale, confermato dall'utente su un file
    /// 1080p60fps con B-frame: `avformat_seek_file`, anche vincolando
    /// `max_ts` al target (provato e verificato inefficace: il demuxer
    /// mov/mp4 lo ignora), può comunque atterrare su un keyframe
    /// *successivo* al target invece che sul precedente, quando il
    /// target cade a un frame o due da un confine di keyframe — il
    /// caso concreto era `target=749` che atterrava su `idx=751` invece
    /// che su un keyframe precedente molto più indietro, lasciando
    /// scoperti per sempre i frame in mezzo (vedi `seek_to_time`, che
    /// ora si autocorregge riprovando più indietro finché non atterra
    /// davvero `<=` al target). Non riprodotto dal contenuto sintetico
    /// qui sotto (il file reale che ha innescato il bug aveva una
    /// struttura B-frame che questo `testsrc` non replica), ma il
    /// contratto (`idx <= target`) va rispettato comunque, vicino a
    /// *ogni* confine di keyframe, non solo lontano da essi come nel
    /// test sopra.
    #[test]
    fn decoder_seek_lands_at_or_before_target_near_every_keyframe_boundary() {
        let path = make_test_clip_with_gop_and_bframes("seek_boundaries.mp4", 4, 25, 3);
        // Keyframe a 0,25,50,75 (fps=25, g=25): un target a 1/2/3 frame
        // prima di ciascuno è il caso limite che ha innescato il bug.
        for keyframe in [25, 50, 75] {
            for offset in [1, 2, 3] {
                let target = keyframe - offset;
                let mut decoder = Decoder::open(&path).unwrap();
                decoder.seek_to_time(target as f64 / 25.0).unwrap();
                let (idx, _) = decoder.next_frame().unwrap().expect("frame atteso");
                assert!(
                    idx <= target,
                    "keyframe={keyframe} offset={offset} target={target}: \
                     atterrato su idx={idx}, oltre il target"
                );
            }
        }
    }
}
