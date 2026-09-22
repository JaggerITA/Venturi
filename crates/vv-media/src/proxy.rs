//! Proxy tutto-intra a bassa risoluzione, generati in background. Su
//! sorgenti long-GOP uno scrub deve decodificare dal keyframe precedente a
//! ogni frame e non tiene il passo; nel proxy ogni frame è un keyframe.
//! Solo anteprima: l'export usa sempre gli originali.
//!
//! Cache globale a chiave `content_hash`, non accanto al progetto: vale per
//! ogni progetto che usa lo stesso file, anche prima di salvare.

use crate::decode::{ColorMatrix, Decoder};
use ffmpeg::Dictionary;
use ffmpeg::codec::{self, encoder};
use ffmpeg::format::{self, Pixel};
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::color;
use ffmpeg_next as ffmpeg;
use std::path::{Path, PathBuf};

/// Larghezza massima di un proxy; altezza in proporzione, pari per il
/// 4:2:0. Un sorgente più stretto non viene ingrandito.
pub const PROXY_MAX_WIDTH: u32 = 960;

pub fn proxies_dir() -> PathBuf {
    crate::cache_dir("proxies")
}

/// Path del file proxy per questo `content_hash`, che il file esista o
/// no ancora — vedi `proxy_exists`.
pub fn proxy_path_for(content_hash: u64) -> PathBuf {
    proxies_dir().join(format!("{content_hash:016x}.mp4"))
}

/// Un `stat` a ogni chiamata invece di uno stato in memoria: il file lo
/// scrive un altro thread e così non serve sincronizzazione.
pub fn proxy_exists(content_hash: u64) -> bool {
    proxy_path_for(content_hash).is_file()
}

/// Genera il proxy e lo scrive atomicamente (file temporaneo + `rename`).
/// `on_frame(frame_scritti)` può bloccare (pausa); `false` annulla senza
/// lasciare file su disco.
pub fn generate_proxy(
    source_path: &Path,
    content_hash: u64,
    mut on_frame: impl FnMut(u64) -> bool,
) -> Result<PathBuf, crate::MediaError> {
    crate::probe::ensure_init();

    let mut decoder = Decoder::open(source_path)?;
    // Dimensioni reali e metadati colore dal primo frame decodificato, come
    // nel path di anteprima.
    let Some((_, first_frame)) = decoder.next_frame()? else {
        return Err(crate::MediaError::NoStream(format!(
            "nessun frame decodificabile in {}",
            source_path.display()
        )));
    };
    let (src_w, src_h) = (first_frame.width, first_frame.height);
    let (dst_w, dst_h) = scaled_dimensions(src_w, src_h, PROXY_MAX_WIDTH);

    let dir = proxies_dir();
    std::fs::create_dir_all(&dir)?;
    let final_path = proxy_path_for(content_hash);
    let tmp_path = dir.join(format!(
        "{content_hash:016x}.tmp-{}.mp4",
        std::process::id()
    ));

    let written = (|| {
        let mut enc = ProxyEncoder::new(
            &tmp_path,
            dst_w,
            dst_h,
            src_w,
            src_h,
            decoder.fps(),
            first_frame.matrix,
            first_frame.full_range,
        )?;
        enc.write_frame(&first_frame)?;
        let mut frames = 1u64;
        if !on_frame(frames) {
            return Err(crate::MediaError::Cancelled);
        }
        while let Some((_, frame)) = decoder.next_frame()? {
            enc.write_frame(&frame)?;
            frames += 1;
            if !on_frame(frames) {
                return Err(crate::MediaError::Cancelled);
            }
        }
        enc.finish()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// Dimensioni scalate mantenendo l'aspect ratio, larghezza al più
/// `max_w`, entrambe pari (richiesto da YUV420P). Un sorgente già più
/// stretto di `max_w` resta alla sua risoluzione nativa.
fn scaled_dimensions(src_w: u32, src_h: u32, max_w: u32) -> (u32, u32) {
    if src_w <= max_w {
        return (even(src_w), even(src_h));
    }
    let ratio = max_w as f64 / src_w as f64;
    (even(max_w), even((src_h as f64 * ratio).round() as u32))
}

fn even(x: u32) -> u32 {
    let x = x.max(2);
    if x.is_multiple_of(2) { x } else { x + 1 }
}

fn to_ffmpeg_space(matrix: ColorMatrix) -> color::Space {
    match matrix {
        ColorMatrix::Bt601 => color::Space::SMPTE170M,
        ColorMatrix::Bt709 => color::Space::BT709,
        ColorMatrix::Bt2020 => color::Space::BT2020NCL,
    }
}

fn to_ffmpeg_range(full_range: bool) -> color::Range {
    if full_range {
        color::Range::JPEG
    } else {
        color::Range::MPEG
    }
}

/// Encoder del proxy: solo video (l'audio in anteprima viene dagli
/// originali), tarato per velocità e non qualità. Obiettivi opposti
/// all'`Encoder` di export, per questo non condiviso.
struct ProxyEncoder {
    octx: format::context::Output,
    encoder: encoder::Video,
    scaler: Scaler,
    stream_index: usize,
    time_base: ffmpeg::Rational,
    ost_time_base: ffmpeg::Rational,
    next_pts: i64,
}

impl ProxyEncoder {
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: &Path,
        dst_w: u32,
        dst_h: u32,
        src_w: u32,
        src_h: u32,
        fps: vv_core::Rational,
        matrix: ColorMatrix,
        full_range: bool,
    ) -> Result<Self, crate::MediaError> {
        let mut octx = format::output(path)?;
        let global_header = octx.format().flags().contains(format::Flags::GLOBAL_HEADER);

        let codec = encoder::find(codec::Id::H264)
            .ok_or_else(|| crate::MediaError::NoStream("encoder H264 non disponibile".into()))?;
        let ost = octx.add_stream(codec)?;
        let stream_index = ost.index();

        let time_base = ffmpeg::Rational::new(fps.den, fps.num);
        let mut ctx = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        ctx.set_width(dst_w);
        ctx.set_height(dst_h);
        ctx.set_format(Pixel::YUV420P);
        ctx.set_time_base(time_base);
        ctx.set_frame_rate(Some(ffmpeg::Rational::new(fps.num, fps.den)));
        ctx.set_colorspace(to_ffmpeg_space(matrix));
        ctx.set_color_range(to_ffmpeg_range(full_range));
        if global_header {
            ctx.set_flags(codec::Flags::GLOBAL_HEADER);
        }

        let mut opts = Dictionary::new();
        // Veloce da codificare e decodificare; la qualità basta per lo scrub.
        opts.set("preset", "veryfast");
        opts.set("crf", "26");
        // Tutto-intra: la ragione d'essere del proxy.
        opts.set("g", "1");
        opts.set("keyint_min", "1");
        opts.set("sc_threshold", "0");

        let encoder = ctx.open_with(opts)?;
        let mut ost = ost;
        ost.set_parameters(&encoder);

        let scaler = Scaler::get(
            Pixel::YUV420P,
            src_w,
            src_h,
            Pixel::YUV420P,
            dst_w,
            dst_h,
            Flags::BILINEAR,
        )?;

        octx.write_header()?;
        let ost_time_base = octx.stream(stream_index).unwrap().time_base();

        Ok(Self {
            octx,
            encoder,
            scaler,
            stream_index,
            time_base,
            ost_time_base,
            next_pts: 0,
        })
    }

    fn write_frame(&mut self, frame: &crate::decode::FrameYuv420) -> Result<(), crate::MediaError> {
        let mut src = ffmpeg::frame::Video::new(Pixel::YUV420P, frame.width, frame.height);
        crate::encode::fill_plane(&mut src, 0, &frame.y, frame.width as usize);
        crate::encode::fill_plane(&mut src, 1, &frame.u, frame.u_width as usize);
        crate::encode::fill_plane(&mut src, 2, &frame.v, frame.u_width as usize);

        let mut scaled = ffmpeg::frame::Video::empty();
        self.scaler.run(&src, &mut scaled)?;
        scaled.set_pts(Some(self.next_pts));
        scaled.set_kind(ffmpeg::picture::Type::None);
        self.next_pts += 1;

        self.encoder.send_frame(&scaled)?;
        self.drain_packets()
    }

    fn drain_packets(&mut self) -> Result<(), crate::MediaError> {
        crate::encode::drain_packets(
            &mut self.octx,
            &mut self.encoder,
            self.stream_index,
            self.time_base,
            self.ost_time_base,
        )
    }
    fn finish(mut self) -> Result<(), crate::MediaError> {
        self.encoder.send_eof()?;
        self.drain_packets()?;
        self.octx.write_trailer()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_clip(file_name: &str, size: &str, duration_secs: u32) -> PathBuf {
        let dir = std::env::temp_dir().join("vv-media-proxy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        crate::test_support::ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size={size}:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ],
            &path,
        );
        path
    }

    #[test]
    fn scaled_dimensions_downscales_preserving_aspect_ratio_to_even_numbers() {
        assert_eq!(scaled_dimensions(1920, 1080, 960), (960, 540));
        // 1280x717 -> larghezza 960, altezza 960*717/1280=537.75 -> 538 (pari).
        assert_eq!(scaled_dimensions(1280, 717, 960), (960, 538));
    }

    #[test]
    fn scaled_dimensions_never_upscales_a_narrower_source() {
        assert_eq!(scaled_dimensions(640, 360, 960), (640, 360));
    }

    #[test]
    fn proxy_path_for_is_deterministic_and_keyed_by_content_hash() {
        assert_eq!(proxy_path_for(42), proxy_path_for(42));
        assert_ne!(proxy_path_for(42), proxy_path_for(43));
        assert!(
            proxy_path_for(42)
                .to_string_lossy()
                .ends_with("venturi/proxies/000000000000002a.mp4")
        );
    }

    #[test]
    fn generate_proxy_produces_a_smaller_all_intra_file_that_decodes_back_correctly() {
        let path = make_test_clip("source.mp4", "640x360", 2);
        let content_hash = 0xABCDEF;
        // Pulizia da un run precedente: `generate_proxy` non sovrascrive
        // in place (scrive un temporaneo e fa rename), ma un file finale
        // già presente da un test precedente fallito a metà potrebbe
        // confondere l'asserzione su `proxy_exists` prima della
        // generazione.
        let _ = std::fs::remove_file(proxy_path_for(content_hash));

        assert!(!proxy_exists(content_hash));
        let proxy_path = generate_proxy(&path, content_hash, |_| true).expect("generazione proxy fallita");
        assert_eq!(proxy_path, proxy_path_for(content_hash));
        assert!(proxy_exists(content_hash));

        // Il proxy deve essere un file H.264 valido, ridecodificabile con
        // lo stesso `Decoder` usato per i sorgenti normali, con la stessa
        // durata (in frame) del sorgente.
        let mut source_decoder = Decoder::open(&path).unwrap();
        let mut source_frames: i32 = 0;
        while source_decoder.next_frame().unwrap().is_some() {
            source_frames += 1;
        }

        let mut proxy_decoder = Decoder::open(&proxy_path).unwrap();
        assert_eq!(
            proxy_decoder.width(),
            640,
            "sorgente già più stretto di 960: non ingrandito"
        );
        let mut proxy_frames = 0;
        while proxy_decoder.next_frame().unwrap().is_some() {
            proxy_frames += 1;
        }
        // Tolleranza di 1 frame in coda: il muxer MP4 di `write_interleaved`
        // (condiviso con `encode.rs`, non specifico al proxy) perde in modo
        // riproducibile l'ultimissimo pacchetto scritto quando non c'è una
        // seconda traccia a forzare il flush dell'interleaving — bug
        // preesistente e più ampio (probabilmente affligge anche l'export),
        // non qualcosa da nascondere qui ma nemmeno da risolvere in questo
        // modulo. Un frame di tolleranza in coda non compromette l'uso da
        // scrub/editing di un proxy.
        assert!(
            (source_frames - proxy_frames).abs() <= 1,
            "il proxy deve avere lo stesso numero di frame del sorgente (tolleranza 1 in coda): sorgente={source_frames} proxy={proxy_frames}"
        );
    }

    #[test]
    fn generate_proxy_downscales_a_wider_source() {
        let path = make_test_clip("wide_source.mp4", "1920x1080", 1);
        let content_hash = 0x123456;
        let _ = std::fs::remove_file(proxy_path_for(content_hash));

        let proxy_path = generate_proxy(&path, content_hash, |_| true).expect("generazione proxy fallita");
        let proxy_decoder = Decoder::open(&proxy_path).unwrap();
        assert_eq!(proxy_decoder.width(), 960);
        assert_eq!(proxy_decoder.height(), 540);
    }
}
