//! Recording from an input device: the microphone of a voiceover.

use std::sync::mpsc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// The names of the input devices, as `Recorder::start` takes them.
pub fn input_device_names() -> Vec<String> {
    let Ok(devices) = cpal::default_host().input_devices() else {
        return Vec::new();
    };
    // ALSA lists a card once per way to open it, all under the same name:
    // one entry is enough, `start` takes the first.
    let mut names: Vec<String> = Vec::new();
    for name in devices.filter_map(|d| device_name(&d)) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

/// A take being recorded: it runs until `finish`.
pub struct Recorder {
    stream: cpal::Stream,
    chunks: mpsc::Receiver<Vec<f32>>,
    take: Take,
}

/// Interleaved samples at their device's rate and channels.
#[derive(Debug, Clone, PartialEq)]
pub struct Take {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl Take {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / self.sample_rate.max(1) as f64
    }
}

impl Recorder {
    /// From the input device called `device`, or the system's default one
    /// when `None` or no longer there.
    pub fn start(device: Option<&str>) -> Result<Self, String> {
        let host = cpal::default_host();
        let named = device.and_then(|name| {
            host.input_devices()
                .ok()?
                .find(|d| device_name(d).as_deref() == Some(name))
        });
        let device = named
            .or_else(|| host.default_input_device())
            .ok_or("no audio input device")?;
        let config = device.default_input_config().map_err(|e| e.to_string())?;
        let format = config.sample_format();
        let stream_config: cpal::StreamConfig = config.into();
        let (tx, chunks) = mpsc::channel();
        let stream = device
            .build_input_stream_raw(
                stream_config.clone(),
                format,
                move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
                    if let Some(samples) = to_f32(data) {
                        let _ = tx.send(samples);
                    }
                },
                |err| eprintln!("vv-audio: recorder stream error: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(Self {
            stream,
            chunks,
            take: Take {
                samples: Vec::new(),
                sample_rate: stream_config.sample_rate,
                channels: stream_config.channels,
            },
        })
    }

    /// Collects what arrived: call it often, the device does not wait.
    pub fn poll(&mut self) -> &Take {
        while let Ok(chunk) = self.chunks.try_recv() {
            self.take.samples.extend_from_slice(&chunk);
        }
        &self.take
    }

    pub fn finish(mut self) -> Take {
        let _ = self.stream.pause();
        self.poll();
        self.take
    }
}

fn to_f32(data: &cpal::Data) -> Option<Vec<f32>> {
    use cpal::SampleFormat as F;
    let scaled = |v: f64, scale: f64| (v / scale) as f32;
    Some(match data.sample_format() {
        F::F32 => data.as_slice::<f32>()?.to_vec(),
        F::F64 => data.as_slice::<f64>()?.iter().map(|&s| s as f32).collect(),
        F::I8 => data
            .as_slice::<i8>()?
            .iter()
            .map(|&s| scaled(s as f64, 128.0))
            .collect(),
        F::I16 => data
            .as_slice::<i16>()?
            .iter()
            .map(|&s| scaled(s as f64, 32768.0))
            .collect(),
        F::I32 => data
            .as_slice::<i32>()?
            .iter()
            .map(|&s| scaled(s as f64, 2147483648.0))
            .collect(),
        F::U8 => data
            .as_slice::<u8>()?
            .iter()
            .map(|&s| scaled(s as f64 - 128.0, 128.0))
            .collect(),
        F::U16 => data
            .as_slice::<u16>()?
            .iter()
            .map(|&s| scaled(s as f64 - 32768.0, 32768.0))
            .collect(),
        _ => return None,
    })
}

#[cfg(test)]
#[path = "tests/recorder.rs"]
mod tests;
