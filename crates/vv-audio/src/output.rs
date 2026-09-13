//! Output realtime via `cpal`.
//!
//! Milestone 2: un buffer audio pre-decodificato in RAM (vedi
//! `vv_media::decode_audio_track`) viene riprodotto con un cursore
//! condiviso (atomic) che il resto dell'app legge come clock di
//! sincronizzazione A/V — l'audio è il master durante il playback, vedi
//! ARCHITECTURE.md § Pipeline audio.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct AudioPlayer {
    _stream: cpal::Stream,
    playing: Arc<AtomicBool>,
    /// Posizione in *frame audio* (un campione per canale), non in byte né
    /// in campioni totali.
    position_frames: Arc<AtomicUsize>,
    sample_rate: u32,
    total_frames: usize,
}

impl AudioPlayer {
    pub fn new(samples: Vec<f32>, sample_rate: u32, channels: u16) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("nessun device audio di output")?;

        let config = cpal::StreamConfig {
            channels,
            sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let playing = Arc::new(AtomicBool::new(false));
        let position_frames = Arc::new(AtomicUsize::new(0));
        let total_frames = samples.len() / channels.max(1) as usize;
        let channels_usize = channels as usize;

        let samples = Arc::new(samples);
        let cb_samples = samples.clone();
        let cb_playing = playing.clone();
        let cb_position = position_frames.clone();

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !cb_playing.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let mut pos = cb_position.load(Ordering::Relaxed);
                    for frame in data.chunks_mut(channels_usize) {
                        if pos >= total_frames {
                            frame.fill(0.0);
                            continue;
                        }
                        let start = pos * channels_usize;
                        frame.copy_from_slice(&cb_samples[start..start + channels_usize]);
                        pos += 1;
                    }
                    cb_position.store(pos, Ordering::Relaxed);
                },
                move |err| eprintln!("vv-audio: errore stream: {err}"),
                None,
            )
            .map_err(|e| e.to_string())?;

        stream.play().map_err(|e| e.to_string())?;

        Ok(Self {
            _stream: stream,
            playing,
            position_frames,
            sample_rate,
            total_frames,
        })
    }

    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn seek_seconds(&self, secs: f64) {
        let frame = (secs.max(0.0) * self.sample_rate as f64) as usize;
        self.position_frames
            .store(frame.min(self.total_frames), Ordering::Relaxed);
    }

    pub fn position_seconds(&self) -> f64 {
        self.position_frames.load(Ordering::Relaxed) as f64 / self.sample_rate as f64
    }

    pub fn duration_seconds(&self) -> f64 {
        self.total_frames as f64 / self.sample_rate as f64
    }

    /// `true` quando il playback ha raggiunto la fine del buffer.
    pub fn finished(&self) -> bool {
        self.position_frames.load(Ordering::Relaxed) >= self.total_frames
    }
}
