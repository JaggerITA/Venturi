//! Miniatura RGBA di un media per il media pool: un frame singolo,
//! convertito su CPU (troppo piccola per giustificare il compositor).

use crate::decode::{ColorMatrix, Decoder, FrameYuv420};
use std::path::Path;

pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Frame a ~1s (o a metà, per clip più corte): il primissimo frame è
/// spesso nero per fade-in.
pub fn generate_thumbnail(
    path: &Path,
    duration_secs: f64,
    max_width: u32,
) -> Result<Thumbnail, crate::MediaError> {
    crate::probe::ensure_init();
    let mut decoder = Decoder::open(path)?;
    let target = (duration_secs / 2.0).min(1.0);
    if target > 0.0 {
        decoder.seek_to_time(target)?;
    }
    let (_, frame) = decoder
        .next_frame()?
        .ok_or_else(|| crate::MediaError::NoStream(path.display().to_string()))?;
    Ok(downscale_to_rgba(&frame, max_width))
}

fn downscale_to_rgba(frame: &FrameYuv420, max_width: u32) -> Thumbnail {
    let width = frame.width.min(max_width).max(1);
    let height = ((frame.height as u64 * width as u64) / frame.width.max(1) as u64).max(1) as u32;
    let (kr, kb) = match frame.matrix {
        ColorMatrix::Bt601 => (0.299, 0.114),
        ColorMatrix::Bt709 => (0.2126, 0.0722),
        ColorMatrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for ty in 0..height {
        let sy = (ty as u64 * frame.height as u64 / height as u64) as usize;
        let cy = (sy * frame.u_height as usize / frame.height as usize).min(frame.u_height as usize - 1);
        for tx in 0..width {
            let sx = (tx as u64 * frame.width as u64 / width as u64) as usize;
            let cx = (sx * frame.u_width as usize / frame.width as usize).min(frame.u_width as usize - 1);
            let y = frame.y[sy * frame.width as usize + sx] as f32;
            let u = frame.u[cy * frame.u_width as usize + cx] as f32 - 128.0;
            let v = frame.v[cy * frame.u_width as usize + cx] as f32 - 128.0;
            let (y, u, v) = if frame.full_range {
                (y, u, v)
            } else {
                ((y - 16.0) * 255.0 / 219.0, u * 255.0 / 224.0, v * 255.0 / 224.0)
            };
            let r = y + 2.0 * (1.0 - kr) * v;
            let b = y + 2.0 * (1.0 - kb) * u;
            let g = (y - kr * r - kb * b) / kg;
            rgba.extend_from_slice(&[
                r.clamp(0.0, 255.0) as u8,
                g.clamp(0.0, 255.0) as u8,
                b.clamp(0.0, 255.0) as u8,
                255,
            ]);
        }
    }
    Thumbnail { width, height, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn generate_thumbnail_scales_down_preserving_aspect_ratio() {
        let dir = std::env::temp_dir().join("vv-media-thumbnail-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("red.mp4");
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "color=c=red:size=640x360:rate=25:duration=2"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", path.to_str().unwrap()])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let thumb = generate_thumbnail(&path, 2.0, 96).unwrap();
        assert_eq!((thumb.width, thumb.height), (96, 54));
        assert_eq!(thumb.rgba.len(), 96 * 54 * 4);
        let center = &thumb.rgba[(27 * 96 + 48) * 4..][..3];
        assert!(center[0] > 200 && center[1] < 60 && center[2] < 60, "{center:?}");
    }

    /// Un'immagine ferma passa `duration_frames`/`fps` sentinel (vedi
    /// `vv_core::IMAGE_DURATION_FRAMES`) come `duration_secs` — enorme,
    /// ma `generate_thumbnail` limita comunque il target del seek a
    /// `<= 1.0`, quindi funziona senza bisogno di `Decoder::open_image`.
    #[test]
    fn generate_thumbnail_works_on_a_still_image_despite_the_sentinel_duration() {
        let dir = std::env::temp_dir().join("vv-media-thumbnail-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("red.png");
        let status = Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "color=c=red:size=640x360:rate=1:duration=1"])
            .args(["-frames:v", "1", "-update", "1", path.to_str().unwrap()])
            .status()
            .expect("ffmpeg CLI non trovato");
        assert!(status.success());

        let sentinel_secs = vv_core::IMAGE_DURATION_FRAMES as f64 / crate::probe::IMAGE_FPS.as_f64();
        let thumb = generate_thumbnail(&path, sentinel_secs, 96).unwrap();
        assert_eq!((thumb.width, thumb.height), (96, 54));
        let center = &thumb.rgba[(27 * 96 + 48) * 4..][..3];
        assert!(center[0] > 200 && center[1] < 60 && center[2] < 60, "{center:?}");
    }
}
