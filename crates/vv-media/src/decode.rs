//! Decode di un media: `next_frame` in sequenza o dopo `seek_to_time`.
//! Ogni formato pixel diventa YUV420P 8 bit denso; la conversione a RGB la
//! fa lo shader. Matrice e range si leggono dal frame originale (lo
//! scaling non li cambia), con un'euristica per risoluzione se mancano.

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::color;
use ffmpeg_next as ffmpeg;
use std::path::Path;
use vv_core::FrameIdx;

pub use vv_core::ColorMatrix;

/// Frame YUV420P 8 bit, piani densi (niente padding di riga).
#[derive(Clone)]
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
    /// `true` = full range (0-255), `false` = limited (16-235/240), la norma.
    pub full_range: bool,
}

impl FrameYuv420 {
    /// Byte occupati dai tre piani.
    pub fn byte_len(&self) -> usize {
        self.y.len() + self.u.len() + self.v.len()
    }
}

/// Byte di un frame YUV420 8 bit `width`x`height`, per stimare budget di
/// cache prima di averlo decodificato.
pub fn yuv420_frame_bytes(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

/// Matrice quando il sorgente non la segnala: BT.601 sotto 720 righe,
/// BT.709 sopra, come ffmpeg. BT.2020 mai indovinata.
fn guess_matrix(space: color::Space, height: u32) -> ColorMatrix {
    match space {
        color::Space::BT709 => ColorMatrix::Bt709,
        color::Space::BT2020NCL | color::Space::BT2020CL => ColorMatrix::Bt2020,
        // Stessa matrice BT.601. Le altre (rare) ricadono sull'euristica.
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
    /// Dopo `send_eof` ffmpeg ne rifiuta un altro prima di un flush: si drena
    /// solo.
    eof_sent: bool,
    /// Frame decodificato da `seek_to_time` per controllare l'atterraggio:
    /// `next_frame` lo restituisce per primo.
    pending: Option<(FrameIdx, FrameYuv420)>,
    /// Unico frame di un'immagine: seek e `next_frame` lo restituiscono sempre,
    /// il resto del decode tratterebbe l'EOF dopo un frame come un errore.
    still_image: Option<FrameYuv420>,
    /// Indice che cresce a ogni `next_frame` su un'immagine, come in un video.
    synthetic_idx: FrameIdx,
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
        // Decode multithread: il seek decodifica dall'ultimo keyframe fino al
        // target, in parallelo è molto più veloce.
        decoder_ctx.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Frame,
            count: 0,
            ..Default::default()
        });
        let decoder = decoder_ctx.video()?;

        let scaler = Scaler::get(
            without_deprecated_range(decoder.format()),
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
            still_image: None,
            synthetic_idx: 0,
        })
    }

    /// Come `open`, per un'immagine ferma: decodifica subito l'unico frame.
    pub fn open_image(path: &Path) -> Result<Self, crate::MediaError> {
        let mut decoder = Self::open(path)?;
        // Lo stesso fps di `probe_image`, non quello (fittizio) del demuxer, o
        // i secondi dei seek non corrisponderebbero ai frame del media.
        decoder.fps = crate::probe::IMAGE_FPS;
        let frame = decoder
            .decode_next_frame()?
            .map(|(_, f)| f)
            .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
        decoder.still_image = Some(frame);
        Ok(decoder)
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

    /// Seek al keyframe `<= secs`, garantito: su mp4 con B-frame
    /// `avformat_seek_file` atterra a volte sul keyframe *dopo*, e i frame in
    /// mezzo non verrebbero mai decodificati. Si decodifica subito il frame di
    /// atterraggio (tenuto in `pending`) e, se è oltre, si riprova più
    /// indietro raddoppiando il passo.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<(), crate::MediaError> {
        let target_idx = (secs.max(0.0) * self.fps.as_f64()).round() as FrameIdx;
        if let Some(frame) = &self.still_image {
            // Un'immagine atterra esattamente sul target; `synthetic_idx` riparte da
            // lì.
            self.pending = Some((target_idx, frame.clone()));
            self.synthetic_idx = target_idx + 1;
            return Ok(());
        }
        let mut ts = (secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
        // Passo iniziale del backoff: un secondo, raddoppiato a ogni tentativo.
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
        if let Some(frame) = &self.still_image {
            // Chiamato senza un seek prima (`pending` vuoto): "avanza" di
            // un frame sintetico, sempre la stessa immagine — vedi doc di
            // `still_image`/`synthetic_idx`.
            let idx = self.synthetic_idx;
            self.synthetic_idx += 1;
            return Ok(Some((idx, frame.clone())));
        }
        self.decode_next_frame()
    }

    /// `next_frame` senza `pending`.
    fn decode_next_frame(&mut self) -> Result<Option<(FrameIdx, FrameYuv420)>, crate::MediaError> {
        let mut decoded = ffmpeg::frame::Video::empty();

        // EOF già inviato: si drenano solo i frame rimasti.
        if self.eof_sent {
            return Ok(if self.decoder.receive_frame(&mut decoded).is_ok() {
                Some(self.finish_frame(&mut decoded)?)
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
                        return Ok(Some(self.finish_frame(&mut decoded)?));
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof()?;
                    self.eof_sent = true;
                    if self.decoder.receive_frame(&mut decoded).is_ok() {
                        return Ok(Some(self.finish_frame(&mut decoded)?));
                    }
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn finish_frame(
        &mut self,
        decoded: &mut ffmpeg::frame::Video,
    ) -> Result<(FrameIdx, FrameYuv420), crate::MediaError> {
        let pts = decoded.pts().unwrap_or(0);
        let secs =
            pts as f64 * self.time_base.numerator() as f64 / self.time_base.denominator() as f64;
        let idx = (secs * self.fps.as_f64()).round() as FrameIdx;
        let matrix = guess_matrix(decoded.color_space(), decoded.height());
        let format = decoded.format();
        let full_range = decoded.color_range() == color::Range::JPEG
            || without_deprecated_range(format) != format;
        decoded.set_format(without_deprecated_range(format));
        let frame = yuv420_from_decoded(&mut self.scaler, decoded, matrix, full_range)?;
        Ok((idx, frame))
    }
}

/// I formati `yuvj*` fanno comprimere a sws il range full in limited, ma
/// il range è già portato da `FrameYuv420::full_range`: si converte come se
/// fossero `yuv*`, lasciando i valori intatti.
fn without_deprecated_range(format: Pixel) -> Pixel {
    match format {
        Pixel::YUVJ420P => Pixel::YUV420P,
        Pixel::YUVJ422P => Pixel::YUV422P,
        Pixel::YUVJ444P => Pixel::YUV444P,
        Pixel::YUVJ440P => Pixel::YUV440P,
        Pixel::YUVJ411P => Pixel::YUV411P,
        other => other,
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

    // `sws_scale` può lasciare padding a fine riga: si ricompatta ogni piano.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_first_frame(path: &Path) -> Result<FrameYuv420, crate::MediaError> {
        let mut decoder = Decoder::open(path)?;
        decoder
            .next_frame()?
            .map(|(_, frame)| frame)
            .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))
    }

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

        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size=320x240:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-g",
                "10", // GOP corto: più keyframe per testare il seek
                "-pix_fmt",
                "yuv420p",
            ],
            &path,
        );
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

        crate::test_support::ffmpeg(
            &[
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
            ],
            &path,
        );
        path
    }

    #[test]
    #[ignore = "misurazione manuale, non una asserzione di correttezza"]
    fn bench_decode_forward_through_a_large_gop() {
        let dir = std::env::temp_dir().join("vv-media-decode-bench");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large_gop.mp4");
        crate::test_support::ffmpeg(
            &[
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
            ],
            &path,
        );

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

    fn make_test_image(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("vv-media-decode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);

        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=1:duration=1",
                "-frames:v",
                "1",
                "-update",
                "1",
            ],
            &path,
        );
        path
    }

    #[test]
    fn open_image_decodes_correct_dimensions_and_pixels() {
        let path = make_test_image("still.png");
        let decoder = Decoder::open_image(&path).expect("apertura immagine fallita");
        assert_eq!(decoder.width(), 320);
        assert_eq!(decoder.height(), 240);
    }

    /// Un'immagine non ha "il prossimo frame": qualunque `source_frame`
    /// richiesto (via `seek_to_time`) o una chiamata sequenziale a
    /// `next_frame` senza seek deve restituire sempre lo stesso
    /// contenuto, mai `None` come farebbe un vero video dopo l'unico
    /// frame disponibile.
    #[test]
    fn open_image_returns_the_same_frame_for_any_requested_position() {
        let path = make_test_image("still_repeat.png");
        let mut decoder = Decoder::open_image(&path).expect("apertura immagine fallita");

        decoder.seek_to_time(0.0).unwrap();
        let (idx0, frame0) = decoder.next_frame().unwrap().expect("frame atteso");
        assert_eq!(idx0, 0);

        decoder.seek_to_time(120.0).unwrap();
        let (idx_far, frame_far) = decoder.next_frame().unwrap().expect("frame atteso anche lontano nel tempo");
        assert_eq!(idx_far, (120.0 * crate::probe::IMAGE_FPS.as_f64()).round() as FrameIdx);
        assert_eq!(frame_far.y, frame0.y, "stesso identico frame, qualunque posizione");

        // Senza un seek in mezzo, next_frame continua a restituire
        // qualcosa (mai None) invece di comportarsi come un vero EOF.
        let (idx_next, frame_next) = decoder.next_frame().unwrap().expect("mai EOF per un'immagine");
        assert!(idx_next > idx_far);
        assert_eq!(frame_next.y, frame0.y);
    }

    /// I JPEG decodificano in `yuvj420p`: il range full va preservato, non
    /// compresso a limited dallo scaler.
    #[test]
    fn full_range_jpeg_keeps_its_luma_range() {
        let dir = std::env::temp_dir().join("vv-media-decode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("white.jpg");
        crate::test_support::ffmpeg(
            &["-f", "lavfi", "-i", "color=white:size=64x64", "-frames:v", "1", "-update", "1"],
            &path,
        );
        let mut decoder = Decoder::open_image(&path).expect("apertura immagine fallita");
        let (_, frame) = decoder.next_frame().unwrap().expect("frame atteso");
        assert!(frame.full_range);
        assert!(frame.y.iter().all(|&y| y >= 250), "luma compressa: {}", frame.y[0]);
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
