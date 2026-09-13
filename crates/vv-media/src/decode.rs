//! Decodifica sequenziale/seek per singola clip (milestone 1-2).
//!
//! Per ora solo `decode_first_frame`: apre il primo stream video, decodifica
//! il primo frame e lo converte in RGBA8 via `sws_scale` per poterlo
//! mostrare subito nel viewer. La conversione YUV->RGB qui è su CPU
//! (software scaler di ffmpeg): è la via più rapida per chiudere la
//! milestone 1. Il path ad alte prestazioni per il playback (milestone 2+)
//! terrà i frame in YUV e farà la conversione in shader nel compositor
//! (vedi ARCHITECTURE.md § Compositing GPU) — qui serve solo un frame,
//! non un flusso continuo, quindi il costo della conversione CPU è
//! trascurabile.

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg_next as ffmpeg;
use std::path::Path;

pub struct FrameRgba {
    pub width: u32,
    pub height: u32,
    /// RGBA8 non-premoltiplicato, stride == width*4 (righe già compattate).
    pub data: Vec<u8>,
}

pub fn decode_first_frame(path: &Path) -> Result<FrameRgba, crate::MediaError> {
    crate::probe::ensure_init();

    let mut ictx = ffmpeg::format::input(&path)?;
    let video_stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
    let video_stream_index = video_stream.index();

    let mut decoder = ffmpeg::codec::context::Context::from_parameters(video_stream.parameters())?
        .decoder()
        .video()?;

    let mut scaler = Scaler::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        Pixel::RGBA,
        decoder.width(),
        decoder.height(),
        Flags::BILINEAR,
    )?;

    let mut decoded = ffmpeg::frame::Video::empty();

    for (stream, packet) in ictx.packets() {
        if stream.index() != video_stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        if decoder.receive_frame(&mut decoded).is_ok() {
            return rgba_from_decoded(&mut scaler, &decoded);
        }
    }

    // Alcuni codec bufferizzano: prova a flushare il decoder a fine stream.
    decoder.send_eof()?;
    if decoder.receive_frame(&mut decoded).is_ok() {
        return rgba_from_decoded(&mut scaler, &decoded);
    }

    Err(crate::MediaError::NoStream(path.display().to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn decode_first_frame_of_x264_reads_correct_dimensions_and_pixels() {
        let dir = std::env::temp_dir().join("vv-media-decode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.mp4");

        // "testsrc" disegna barre colorate + un pattern in movimento: usato
        // per assicurarci che i pixel decodificati non siano un buffer
        // degenere (es. tutto a zero per un bug di stride/formato).
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=25:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

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
}
