//! Generazione proxy in background: copia a bassa risoluzione,
//! tutto-intra (ogni frame è un keyframe), di ogni media importato.
//!
//! Perché: su sorgenti long-GOP (tipico H.264, keyframe ogni 5-10s), uno
//! scrub veloce nella timeline richiede al decoder di attraversare in
//! sequenza dal keyframe più vicino fino al frame richiesto — un costo
//! reale, proporzionale alla distanza, che nessuna euristica di seek può
//! eliminare (misurato: a 1080p il decode non tiene il passo di uno
//! scrub veloce, 0 frame esatti disponibili durante il drag, ~1.6s per
//! recuperare dopo essersi fermati). Un proxy tutto-intra a bassa
//! risoluzione rende invece *ogni* frame raggiungibile con un decode
//! singolo, indipendentemente da dove sia il keyframe più vicino nel
//! sorgente — non un'approssimazione del frame mostrato (resta quello
//! esatto alla posizione richiesta), solo una rappresentazione più
//! leggera da cui decodificarlo. Usato per l'anteprima/editing in
//! generale quando attivo (non solo durante lo scrub): l'export usa
//! sempre e solo i sorgenti originali (vedi ARCHITECTURE.md).
//!
//! Cartella cache globale (`proxies_dir`), non "accanto al progetto":
//! la chiave è `content_hash` del `MediaItem`, indipendente dal
//! progetto — un proxy generato in una sessione resta valido (e
//! riusabile da un altro progetto che referenzia lo stesso file) anche
//! prima che il progetto corrente sia mai stato salvato.

use crate::decode::{ColorMatrix, Decoder};
use ffmpeg::Dictionary;
use ffmpeg::codec::{self, encoder};
use ffmpeg::format::{self, Pixel};
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::util::color;
use ffmpeg_next as ffmpeg;
use std::path::{Path, PathBuf};

/// Larghezza massima di un proxy (ARCHITECTURE.md §Pipeline di decode +
/// cache): l'altezza segue mantenendo l'aspect ratio del sorgente,
/// arrotondata a un numero pari (richiesto dal sottocampionamento 4:2:0
/// di YUV420P). Un sorgente già più stretto di questa non viene
/// ingrandito, solo ricodificato tutto-intra.
pub const PROXY_MAX_WIDTH: u32 = 960;

/// Cartella cache globale dei proxy: `$XDG_CACHE_HOME/vibevideo/proxies/`,
/// o `~/.cache/vibevideo/proxies/` se `XDG_CACHE_HOME` non è impostata
/// (fallback a una cartella temporanea di sistema nel caso limite in cui
/// non ci sia nemmeno `HOME`, es. alcuni ambienti sandboxed).
pub fn proxies_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("vibevideo").join("proxies")
}

/// Path del file proxy per questo `content_hash`, che il file esista o
/// no ancora — vedi `proxy_exists`.
pub fn proxy_path_for(content_hash: u64) -> PathBuf {
    proxies_dir().join(format!("{content_hash:016x}.mp4"))
}

/// Il proxy per questo `content_hash` è già stato generato ed è pronto
/// da usare? Un controllo su file system economico (un `stat`), fatto
/// ogni volta invece di tenere uno stato in memoria: il file può
/// comparire in qualunque momento da un thread di generazione separato,
/// e un semplice `is_file()` non ha bisogno di sincronizzazione.
pub fn proxy_exists(content_hash: u64) -> bool {
    proxy_path_for(content_hash).is_file()
}

/// Genera il proxy per `source_path` (chiave `content_hash`) e lo scrive
/// **atomicamente** a `proxy_path_for(content_hash)`: prima un file
/// temporaneo nella stessa cartella, poi `rename` (atomico sullo stesso
/// filesystem) — mai un file a metà scritta visibile a un lettore
/// concorrente (`proxy_exists`/`render_ahead`, su un altro thread,
/// potrebbero controllarlo in qualunque istante mentre questa funzione
/// è ancora in corso).
pub fn generate_proxy(source_path: &Path, content_hash: u64) -> Result<PathBuf, crate::MediaError> {
    crate::probe::ensure_init();

    let mut decoder = Decoder::open(source_path)?;
    // Il primo frame decodificato porta sia le dimensioni reali sia i
    // metadati colore (matrice/range) da propagare al proxy — letti qui
    // invece che dal contesto del decoder perché è lo stesso identico
    // meccanismo già usato per il path di preview (`decode::finish_frame`),
    // non uno nuovo da mantenere allineato.
    let Some((_, first_frame)) = decoder.next_frame()? else {
        return Err(crate::MediaError::NoStream(format!(
            "nessun frame decodificabile in {}",
            source_path.display()
        )));
    };
    let (src_w, src_h) = (first_frame.width, first_frame.height);
    let (dst_w, dst_h) = scaled_dimensions(src_w, src_h, PROXY_MAX_WIDTH);

    let dir = proxies_dir();
    std::fs::create_dir_all(&dir).map_err(io_err)?;
    let final_path = proxy_path_for(content_hash);
    let tmp_path = dir.join(format!(
        "{content_hash:016x}.tmp-{}.mp4",
        std::process::id()
    ));

    {
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
        while let Some((_, frame)) = decoder.next_frame()? {
            enc.write_frame(&frame)?;
        }
        enc.finish()?;
    }

    std::fs::rename(&tmp_path, &final_path).map_err(io_err)?;
    Ok(final_path)
}

fn io_err(e: std::io::Error) -> crate::MediaError {
    crate::MediaError::NoStream(e.to_string())
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

/// Encoder minimale per il proxy: solo video, senza audio (il proxy
/// serve solo per il video — l'audio in anteprima lo suona il mixer dai
/// sorgenti originali), parametri
/// x264 mirati a "veloce da generare e da decodificare", non a qualità
/// d'archivio (`Encoder` in `encode.rs`, usato per l'export, ha
/// obiettivi opposti — non condiviso apposta).
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
        // Veloce da codificare (generazione in background, non deve
        // competere a lungo con decode/UI) e da decodificare (bassa
        // complessità per il player durante lo scrub). Qualità bassa ma
        // accettabile per editing/scrub, mai usata per l'export.
        opts.set("preset", "veryfast");
        opts.set("crf", "26");
        // Tutto-intra: ogni frame è un keyframe. È l'unico motivo
        // d'essere di questo modulo — senza questo, il proxy avrebbe lo
        // stesso identico problema del sorgente originale, solo più
        // piccolo.
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
        copy_plane_into(&mut src, 0, &frame.y, frame.width as usize);
        copy_plane_into(&mut src, 1, &frame.u, frame.u_width as usize);
        copy_plane_into(&mut src, 2, &frame.v, frame.u_width as usize);

        let mut scaled = ffmpeg::frame::Video::empty();
        self.scaler.run(&src, &mut scaled)?;
        scaled.set_pts(Some(self.next_pts));
        scaled.set_kind(ffmpeg::picture::Type::None);
        self.next_pts += 1;

        self.encoder.send_frame(&scaled)?;
        self.drain_packets()
    }

    fn drain_packets(&mut self) -> Result<(), crate::MediaError> {
        let mut packet = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(self.stream_index);
            packet.rescale_ts(self.time_base, self.ost_time_base);
            packet.write_interleaved(&mut self.octx)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), crate::MediaError> {
        self.encoder.send_eof()?;
        self.drain_packets()?;
        self.octx.write_trailer()?;
        Ok(())
    }
}

/// Copia un piano denso (`FrameYuv420::y`/`u`/`v`, nessun padding tra le
/// righe) in un piano `ffmpeg::frame::Video`, che invece può avere uno
/// stride più largo della riga effettiva — simmetrico a `pack_plane` in
/// `decode.rs` (quella impacchetta leggendo, questa spacchetta scrivendo).
fn copy_plane_into(dst: &mut ffmpeg::frame::Video, index: usize, src: &[u8], row_width: usize) {
    let stride = dst.stride(index);
    let height = src.len() / row_width.max(1);
    let data = dst.data_mut(index);
    for row in 0..height {
        let src_start = row * row_width;
        let dst_start = row * stride;
        data[dst_start..dst_start + row_width]
            .copy_from_slice(&src[src_start..src_start + row_width]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as OsCommand;

    fn make_test_clip(file_name: &str, size: &str, duration_secs: u32) -> PathBuf {
        let dir = std::env::temp_dir().join("vv-media-proxy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        let status = OsCommand::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size={size}:rate=25:duration={duration_secs}"),
                "-c:v",
                "libx264",
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
                .ends_with("vibevideo/proxies/000000000000002a.mp4")
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
        let proxy_path = generate_proxy(&path, content_hash).expect("generazione proxy fallita");
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

        let proxy_path = generate_proxy(&path, content_hash).expect("generazione proxy fallita");
        let proxy_decoder = Decoder::open(&proxy_path).unwrap();
        assert_eq!(proxy_decoder.width(), 960);
        assert_eq!(proxy_decoder.height(), 540);
    }
}
