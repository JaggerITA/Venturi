//! Decodifica sequenziale/seek per singola clip.
//!
//! `Decoder` apre lo stream video di un media e permette di decodificare
//! frame in sequenza (`next_frame`) o dopo un seek (`seek_to_time`). Per
//! ora la conversione YUV->RGB è su CPU via `sws_scale`: è la via più
//! rapida per avere un player funzionante. Il path ad alte prestazioni per
//! il playback continuo (milestone 5+) terrà i frame in YUV e farà la
//! conversione in shader nel compositor (vedi ARCHITECTURE.md § Compositing
//! GPU) — qui il costo CPU per frame resta finché il viewer passa dalla
//! texture egui standard.

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg_next as ffmpeg;
use std::path::Path;
use vv_core::FrameIdx;

pub struct FrameRgba {
    pub width: u32,
    pub height: u32,
    /// RGBA8 non-premoltiplicato, stride == width*4 (righe già compattate).
    pub data: Vec<u8>,
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

        let decoder = ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?
            .decoder()
            .video()?;

        let scaler = Scaler::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::RGBA,
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

    /// Seek al keyframe più vicino con timestamp <= `secs` (comportamento
    /// di default di `avformat_seek_file`). Il decoder va flushato: i
    /// reference frame da prima del seek non sono più validi.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<(), crate::MediaError> {
        let ts = (secs * f64::from(ffmpeg::ffi::AV_TIME_BASE)) as i64;
        self.ictx.seek(ts, ..)?;
        self.decoder.flush();
        self.eof_sent = false;
        Ok(())
    }

    /// Decodifica il prossimo frame video disponibile in ordine di
    /// presentazione. `Ok(None)` a fine stream.
    pub fn next_frame(&mut self) -> Result<Option<(FrameIdx, FrameRgba)>, crate::MediaError> {
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
    ) -> Result<(FrameIdx, FrameRgba), crate::MediaError> {
        let pts = decoded.pts().unwrap_or(0);
        let secs =
            pts as f64 * self.time_base.numerator() as f64 / self.time_base.denominator() as f64;
        let idx = (secs * self.fps.as_f64()).round() as FrameIdx;
        let rgba = rgba_from_decoded(&mut self.scaler, decoded)?;
        Ok((idx, rgba))
    }
}

fn rgba_from_decoded(
    scaler: &mut Scaler,
    decoded: &ffmpeg::frame::Video,
) -> Result<FrameRgba, crate::MediaError> {
    let mut rgba = ffmpeg::frame::Video::empty();
    scaler.run(decoded, &mut rgba)?;

    let width = rgba.width();
    let height = rgba.height();
    let stride = rgba.stride(0);
    let plane = rgba.data(0);
    let row_bytes = (width * 4) as usize;

    // sws_scale può restituire righe con padding (stride > row_bytes):
    // ricompattiamo per avere un buffer denso da caricare come texture.
    let mut data = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        data.extend_from_slice(&plane[start..start + row_bytes]);
    }

    Ok(FrameRgba {
        width,
        height,
        data,
    })
}

pub fn decode_first_frame(path: &Path) -> Result<FrameRgba, crate::MediaError> {
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

    #[test]
    fn decode_first_frame_of_x264_reads_correct_dimensions_and_pixels() {
        let path = make_test_clip("sample.mp4", 1);

        let frame = decode_first_frame(&path).expect("decode fallito");
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.data.len(), 320 * 240 * 4);

        // Il pattern testsrc non è mai uniforme: se troviamo più di un
        // valore distinto tra i byte RGB, lo stride/formato sono corretti.
        let distinct: std::collections::HashSet<u8> = frame
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .step_by(37) // campiona, non serve leggere ogni pixel
            .map(|px| px[0])
            .collect();
        assert!(
            distinct.len() > 5,
            "i pixel decodificati sembrano degeneri: {distinct:?}"
        );

        // Alpha sempre opaco per un frame video.
        assert!(frame.data.as_chunks::<4>().0.iter().all(|px| px[3] == 255));
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
}
