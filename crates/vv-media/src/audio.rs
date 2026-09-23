//! Decoding whole audio tracks into interleaved f32. The track sits
//! entirely in RAM; the streaming version delivers it in chunks so the mixer
//! can play its start before the decode is over.

use ffmpeg::ChannelLayout;
use ffmpeg::format::sample::{Sample, Type as SampleType};
use ffmpeg::media::Type;
use ffmpeg::software::resampling::context::Context as Resampler;
use ffmpeg_next as ffmpeg;
use std::ops::ControlFlow;
use std::path::Path;

pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved f32, length = total_frames * channels.
    pub samples: Vec<f32>,
}

/// Decodes the N-th audio stream (order of `probe::audio_streams`).
/// `Ok(None)` if it does not exist.
pub fn decode_audio_track(
    path: &Path,
    stream_index: usize,
) -> Result<Option<AudioBuffer>, crate::MediaError> {
    let mut samples = Vec::new();
    let formats = decode_audio_streams_streaming(path, &[stream_index], None, |_, _, chunk| {
        samples.extend_from_slice(chunk);
        ControlFlow::Continue(())
    })?;
    Ok(formats[0].map(|(sample_rate, channels)| AudioBuffer {
        sample_rate,
        channels,
        samples,
    }))
}

struct StreamDecoder {
    decoder: ffmpeg::decoder::Audio,
    resampler: Resampler,
    layout: ChannelLayout,
    in_rate: u32,
    out_rate: u32,
    channels: u16,
}

impl StreamDecoder {
    fn open(
        stream: &ffmpeg::format::stream::Stream,
        out_rate: Option<u32>,
    ) -> Result<Self, crate::MediaError> {
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        let layout = decoder_channel_layout(&decoder);
        let in_rate = decoder.rate();
        let out_rate = out_rate.unwrap_or(in_rate);
        let resampler = Resampler::get(
            decoder.format(),
            layout,
            in_rate,
            Sample::F32(SampleType::Packed),
            layout,
            out_rate,
        )?;
        Ok(Self {
            channels: decoder.channels(),
            decoder,
            resampler,
            layout,
            in_rate,
            out_rate,
        })
    }

    /// Resamples and delivers all the frames ready in the decoder.
    fn drain(
        &mut self,
        slot: usize,
        samples: &mut Vec<f32>,
        on_chunk: &mut impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, crate::MediaError> {
        let mut decoded = ffmpeg::frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            // `Resampler::run` allocates the output as large as the input, which
            // is not enough when resampling upwards.
            let capacity =
                decoded.samples() * self.out_rate as usize / self.in_rate.max(1) as usize + 64;
            let mut resampled =
                ffmpeg::frame::Audio::new(Sample::F32(SampleType::Packed), capacity, self.layout);
            run_resampler(&mut self.resampler, &mut decoded, &mut resampled)?;
            if emit(slot, self.channels, &resampled, samples, on_chunk).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn flush(
        &mut self,
        slot: usize,
        samples: &mut Vec<f32>,
        on_chunk: &mut impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, crate::MediaError> {
        while self.resampler.delay().is_some() {
            let mut resampled =
                ffmpeg::frame::Audio::new(Sample::F32(SampleType::Packed), 4096, self.layout);
            self.resampler.flush(&mut resampled)?;
            if resampled.samples() == 0 {
                break;
            }
            if emit(slot, self.channels, &resampled, samples, on_chunk).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

fn emit(
    slot: usize,
    channels: u16,
    frame: &ffmpeg::frame::Audio,
    samples: &mut Vec<f32>,
    on_chunk: &mut impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
) -> ControlFlow<()> {
    samples.clear();
    append_f32(frame, samples);
    if samples.is_empty() {
        ControlFlow::Continue(())
    } else {
        on_chunk(slot, channels, samples)
    }
}

/// Several streams in a single read, in chunks: `on_chunk(i, channels,
/// samples)` with `i` an index into `stream_indices`; `Break` interrupts.
/// `out_rate` resamples with one swresample context per stream, without
/// discontinuities between chunks. Returns `(rate, channels)` per stream,
/// `None` if it does not exist.
pub fn decode_audio_streams_streaming(
    path: &Path,
    stream_indices: &[usize],
    out_rate: Option<u32>,
    mut on_chunk: impl FnMut(usize, u16, &[f32]) -> ControlFlow<()>,
) -> Result<Vec<Option<(u32, u16)>>, crate::MediaError> {
    crate::probe::ensure_init();

    let mut ictx = ffmpeg::format::input(&path)?;
    let audio_streams: Vec<_> = ictx
        .streams()
        .filter(|s| s.parameters().medium() == Type::Audio)
        .collect();
    // (index in the container, decoder) per slot.
    let mut decoders: Vec<Option<(usize, StreamDecoder)>> = stream_indices
        .iter()
        .map(|&i| {
            audio_streams
                .get(i)
                .map(|stream| Ok((stream.index(), StreamDecoder::open(stream, out_rate)?)))
                .transpose()
        })
        .collect::<Result<_, crate::MediaError>>()?;
    drop(audio_streams);
    let formats = decoders
        .iter()
        .map(|d| d.as_ref().map(|(_, d)| (d.out_rate, d.channels)))
        .collect();
    if decoders.iter().all(Option::is_none) {
        return Ok(formats);
    }

    let mut packet = ffmpeg::Packet::empty();
    let mut samples = Vec::new();
    loop {
        match packet.read(&mut ictx) {
            Ok(()) => {
                let Some((slot, (_, stream))) = decoders
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(slot, d)| Some((slot, d.as_mut()?)))
                    .find(|(_, (i, _))| *i == packet.stream())
                else {
                    continue;
                };
                stream.decoder.send_packet(&packet)?;
                if stream.drain(slot, &mut samples, &mut on_chunk)?.is_break() {
                    return Ok(formats);
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    for (slot, entry) in decoders.iter_mut().enumerate() {
        let Some((_, stream)) = entry else {
            continue;
        };
        stream.decoder.send_eof()?;
        if stream.drain(slot, &mut samples, &mut on_chunk)?.is_break()
            || stream.flush(slot, &mut samples, &mut on_chunk)?.is_break()
        {
            break;
        }
    }
    Ok(formats)
}

/// Decoder channel layout, with a default one in place of an unspecified
/// one (e.g. PCM in MKV): otherwise swresample rejects every frame with
/// "Input changed".
pub(crate) fn decoder_channel_layout(decoder: &ffmpeg::decoder::Audio) -> ChannelLayout {
    let layout = decoder.channel_layout();
    if layout.is_empty() {
        ChannelLayout::default(i32::from(decoder.channels()))
    } else {
        layout
    }
}

/// `Resampler::run` after aligning the frame layout to the resampler's
/// (see `decoder_channel_layout`).
pub(crate) fn run_resampler(
    resampler: &mut Resampler,
    decoded: &mut ffmpeg::frame::Audio,
    resampled: &mut ffmpeg::frame::Audio,
) -> Result<(), crate::MediaError> {
    if decoded.channel_layout().is_empty() {
        decoded.set_channel_layout(resampler.input().channel_layout);
    }
    resampler.run(decoded, resampled)?;
    Ok(())
}

fn append_f32(frame: &ffmpeg::frame::Audio, out: &mut Vec<f32>) {
    let byte_len = frame.samples() * frame.channels() as usize * 4;
    out.extend(
        frame.data(0)[..byte_len]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_ne_bytes(*b)),
    );
}

#[cfg(test)]
#[path = "tests/audio.rs"]
mod tests;
