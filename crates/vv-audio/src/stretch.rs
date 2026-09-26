//! Pitch-preserving time-stretch with libavfilter's `rubberband` filter.
//! It works on windows of a few seconds (`vv-app::timeline_audio`), not on
//! the whole track: stretching everything would delay the first sound by
//! seconds.

use ffmpeg::channel_layout::ChannelLayout;
use ffmpeg::format::sample::{Sample as SampleFormat, Type as SampleType};
use ffmpeg_next as ffmpeg;
use std::sync::Once;

static INIT: Once = Once::new();

fn ensure_init() {
    INIT.call_once(|| {
        ffmpeg::init().expect("cannot initialize ffmpeg");
    });
}

const CHUNK_FRAMES: usize = 4096;

/// Duration multiplied by `1/tempo`, same rate and channels, pitch
/// preserved.
pub fn stretch_samples(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    tempo: f64,
) -> Result<Vec<f32>, String> {
    if tempo.is_nan() || tempo <= 0.0 {
        return Err(format!("invalid stretch tempo: {tempo}"));
    }
    ensure_init();

    let format = SampleFormat::F32(SampleType::Packed);
    let layout = ChannelLayout::default(channels as i32);

    let mut graph = ffmpeg::filter::Graph::new();
    let args = format!(
        "time_base=1/{sample_rate}:sample_rate={sample_rate}:sample_fmt={}:channel_layout=0x{:x}",
        format.name(),
        layout.bits()
    );
    graph
        .add(
            &ffmpeg::filter::find("abuffer").ok_or("abuffer filter not found")?,
            "in",
            &args,
        )
        .map_err(|e| e.to_string())?;
    graph
        .add(
            &ffmpeg::filter::find("abuffersink").ok_or("abuffersink filter not found")?,
            "out",
            "",
        )
        .map_err(|e| e.to_string())?;

    // `rubberband` can output planar even with packed input: `aformat` forces
    // packed, otherwise `drain_filtered` reads past the first plane (panic
    // with stereo audio).
    graph
        .output("in", 0)
        .and_then(|p| p.input("out", 0))
        .and_then(|p| {
            p.parse(&format!(
                "rubberband=tempo={tempo},aformat=sample_fmts={}",
                format.name()
            ))
        })
        .map_err(|e| e.to_string())?;
    graph.validate().map_err(|e| e.to_string())?;

    let channels_usize = channels as usize;
    let total_frames = samples.len() / channels_usize.max(1);

    let mut out = Vec::new();
    let mut pos = 0;
    while pos < total_frames {
        let n = CHUNK_FRAMES.min(total_frames - pos);
        let mut frame = ffmpeg::frame::Audio::new(format, n, layout);
        frame.set_rate(sample_rate);
        let start = pos * channels_usize;
        frame.plane_mut::<f32>(0)[..n * channels_usize]
            .copy_from_slice(&samples[start..start + n * channels_usize]);

        graph
            .get("in")
            .ok_or("pad 'in' missing")?
            .source()
            .add(&frame)
            .map_err(|e| e.to_string())?;
        drain_filtered(&mut graph, channels_usize, &mut out)?;
        pos += n;
    }
    graph
        .get("in")
        .ok_or("pad 'in' missing")?
        .source()
        .flush()
        .map_err(|e| e.to_string())?;
    drain_filtered(&mut graph, channels_usize, &mut out)?;

    Ok(out)
}

fn drain_filtered(
    graph: &mut ffmpeg::filter::Graph,
    channels: usize,
    out: &mut Vec<f32>,
) -> Result<(), String> {
    let mut filtered = ffmpeg::frame::Audio::empty();
    while graph
        .get("out")
        .ok_or("pad 'out' missing")?
        .sink()
        .frame(&mut filtered)
        .is_ok()
    {
        let n = filtered.samples();
        out.extend_from_slice(&filtered.plane::<f32>(0)[..n * channels]);
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/stretch.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/stretch_bench_window.rs"]
mod bench_window;
