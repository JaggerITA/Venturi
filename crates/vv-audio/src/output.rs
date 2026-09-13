//! Output realtime via `cpal`.
//!
//! Milestone 2: un buffer audio pre-decodificato in RAM (vedi
//! `vv_media::decode_audio_track`) viene riprodotto con un cursore
//! condiviso (atomic) che il resto dell'app legge come clock di
//! sincronizzazione A/V — l'audio è il master durante il playback, vedi
//! ARCHITECTURE.md § Pipeline audio.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

pub struct AudioPlayer {
    _stream: cpal::Stream,
    playing: Arc<AtomicBool>,
    /// Posizione in *frame audio* (un campione per canale), non in byte né
    /// in campioni totali.
    position_frames: Arc<AtomicUsize>,
    /// Guadagno lineare (non dB) codificato come bit pattern di un f32, per
    /// poterlo leggere/scrivere via atomic dal callback realtime senza lock.
    gain_linear_bits: Arc<AtomicU32>,
    /// Picco assoluto (post-gain) dell'ultimo buffer scritto dal callback,
    /// stesso incapsulamento bit-pattern di `gain_linear_bits`: per
    /// l'audiometer nella UI (vedi `Player::peak_linear` in vv-app), non
    /// una misura accurata/professionale (nessun RMS, nessuna finestra —
    /// solo il valore assoluto massimo tra i campioni dell'ultimo
    /// callback).
    peak_linear_bits: Arc<AtomicU32>,
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
        let gain_linear_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let peak_linear_bits = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let total_frames = samples.len() / channels.max(1) as usize;
        let channels_usize = channels as usize;

        let samples = Arc::new(samples);
        let cb_samples = samples.clone();
        let cb_playing = playing.clone();
        let cb_position = position_frames.clone();
        let cb_gain = gain_linear_bits.clone();
        let cb_peak = peak_linear_bits.clone();

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if !cb_playing.load(Ordering::Relaxed) {
                        data.fill(0.0);
                    } else {
                        let gain = f32::from_bits(cb_gain.load(Ordering::Relaxed));
                        let mut pos = cb_position.load(Ordering::Relaxed);
                        for frame in data.chunks_mut(channels_usize) {
                            if pos >= total_frames {
                                frame.fill(0.0);
                                continue;
                            }
                            let start = pos * channels_usize;
                            for (dst, src) in frame
                                .iter_mut()
                                .zip(&cb_samples[start..start + channels_usize])
                            {
                                *dst = src * gain;
                            }
                            pos += 1;
                        }
                        cb_position.store(pos, Ordering::Relaxed);
                    }
                    // Picco (post-gain) di questo buffer, per l'audiometer:
                    // calcolato qui in entrambi i rami (silenzio quando in
                    // pausa/a fine buffer inclusi), non solo quando si
                    // riproduce davvero, così il meter scende a zero da
                    // solo alla pausa invece di restare "incollato" all'ultimo
                    // valore.
                    let peak = data.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                    cb_peak.store(peak.to_bits(), Ordering::Relaxed);
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
            gain_linear_bits,
            peak_linear_bits,
            sample_rate,
            total_frames,
        })
    }

    /// Imposta il guadagno in decibel (0.0 = invariato, -inf teorico -> 0
    /// lineare, valori positivi amplificano). Applicato in tempo reale nel
    /// callback audio, nessuna riconversione del buffer.
    pub fn set_gain_db(&self, db: f32) {
        self.gain_linear_bits
            .store(db_to_linear(db).to_bits(), Ordering::Relaxed);
    }

    /// Picco lineare (0.0..=1.0 di norma, può superare 1.0 con un gain
    /// positivo) dell'ultimo buffer scritto dal callback audio: per un
    /// audiometer nella UI, non una misura professionale (vedi doc del
    /// campo `peak_linear_bits`).
    pub fn peak_linear(&self) -> f32 {
        f32::from_bits(self.peak_linear_bits.load(Ordering::Relaxed))
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

fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_to_linear_matches_known_reference_points() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        // -6dB ~= dimezza l'ampiezza; +6dB ~= raddoppia.
        assert!((db_to_linear(-6.0) - 0.5012).abs() < 1e-3);
        assert!((db_to_linear(6.0) - 1.9953).abs() < 1e-3);
        // -20dB = fattore 0.1 esatto.
        assert!((db_to_linear(-20.0) - 0.1).abs() < 1e-6);
    }
}
