//! Multiband compressor: Linkwitz-Riley crossovers (the bands sum back to
//! a flat response) and one stereo-linked compressor per band.

use vv_core::{CompressorBand, MultibandCompressor};

use crate::filter::{Biquad, unit_delays};
use crate::mixer::db_to_linear;

/// Of the soft knee, in dB.
const KNEE_DB: f32 = 6.0;

#[derive(Debug, Clone, Copy)]
struct BandDynamics {
    threshold_db: f32,
    slope: f32,
    makeup: f32,
    attack: f32,
    release: f32,
}

impl BandDynamics {
    fn new(band: &CompressorBand, sample_rate: f32) -> Self {
        let coefficient = |ms: f32| (-1.0 / (ms.max(0.01) * 0.001 * sample_rate)).exp();
        Self {
            threshold_db: band.threshold_db,
            slope: 1.0 - 1.0 / band.ratio.max(1.0),
            makeup: db_to_linear(band.makeup_db),
            attack: coefficient(band.attack_ms),
            release: coefficient(band.release_ms),
        }
    }

    /// Gain reduction in dB at `level_db`, with a soft knee.
    fn reduction_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        if 2.0 * over < -KNEE_DB {
            0.0
        } else if 2.0 * over.abs() <= KNEE_DB {
            self.slope * (over + KNEE_DB / 2.0).powi(2) / (2.0 * KNEE_DB)
        } else {
            self.slope * over
        }
    }
}

/// Memories of the 12 biquads of one channel: low/high split at the first
/// crossover, the upper part split at the second, the low band through the
/// allpass of the second so it stays in phase with the other two.
type ChannelMemory = [[f32; 2]; 12];

pub struct MultibandProcessor {
    low_pass: [Biquad; 2],
    high_pass: [Biquad; 2],
    bands: [BandDynamics; 3],
    memory: Vec<ChannelMemory>,
    /// Peak envelope of each band, linear, linked across the channels.
    envelope: [f32; 3],
    /// Band samples of the current frame, per channel.
    split: Vec<[f32; 3]>,
}

impl MultibandProcessor {
    pub fn new(params: &MultibandCompressor, sample_rate: u32, channels: u16) -> Self {
        let rate = sample_rate as f32;
        let nyquist_margin = rate * 0.45;
        let low = params.crossovers_hz[0].clamp(20.0, nyquist_margin);
        let high = params.crossovers_hz[1].clamp(low, nyquist_margin);
        let ch = channels.max(1) as usize;
        Self {
            low_pass: [low, high].map(|f| Biquad::butterworth(f, rate, false)),
            high_pass: [low, high].map(|f| Biquad::butterworth(f, rate, true)),
            bands: params.bands.map(|b| BandDynamics::new(&b, rate)),
            memory: vec![[[0.0; 2]; 12]; ch],
            envelope: [0.0; 3],
            split: vec![[0.0; 3]; ch],
        }
    }

    pub fn reset(&mut self) {
        self.memory.fill([[0.0; 2]; 12]);
        self.envelope = [0.0; 3];
    }

    /// Continues from where `other` was, if it has the same channels: new
    /// parameters must not restart the filters and the envelopes.
    pub fn take_memory_from(&mut self, other: &Self) {
        if self.memory.len() == other.memory.len() {
            self.memory.copy_from_slice(&other.memory);
            self.envelope = other.envelope;
        }
    }

    /// In place, interleaved. No allocations: it runs in the realtime
    /// callback.
    pub fn process(&mut self, samples: &mut [f32]) -> BandActivity {
        let mut activity = BandActivity::default();
        let ch = self.memory.len();
        let [lp1, lp2] = &self.low_pass;
        let [hp1, hp2] = &self.high_pass;
        for frame in samples.chunks_exact_mut(ch) {
            let mut peak = [0.0f32; 3];
            for ((x, m), split) in frame.iter().zip(&mut self.memory).zip(&mut self.split) {
                let low = lp1.tick_lr4(&mut m[0..2], *x);
                let rest = hp1.tick_lr4(&mut m[2..4], *x);
                let mid = lp2.tick_lr4(&mut m[4..6], rest);
                let high = hp2.tick_lr4(&mut m[6..8], rest);
                let low = lp2.tick_lr4(&mut m[8..10], low) + hp2.tick_lr4(&mut m[10..12], low);
                *split = [low, mid, high];
                for (p, s) in peak.iter_mut().zip(split.iter()) {
                    *p = p.max(s.abs());
                }
            }
            let mut gains = [1.0f32; 3];
            for (b, band) in self.bands.iter().enumerate() {
                let env = &mut self.envelope[b];
                let coefficient = if peak[b] > *env {
                    band.attack
                } else {
                    band.release
                };
                *env = coefficient * *env + (1.0 - coefficient) * peak[b];
                let level_db = 20.0 * env.max(1e-9).log10();
                let reduction_db = band.reduction_db(level_db);
                activity.reduction_db[b] = activity.reduction_db[b].max(reduction_db);
                activity.level[b] = activity.level[b].max(*env);
                gains[b] = db_to_linear(-reduction_db) * band.makeup;
            }
            for (x, split) in frame.iter_mut().zip(&self.split) {
                *x = split[0] * gains[0] + split[1] * gains[1] + split[2] * gains[2];
            }
        }
        activity
    }
}

/// What the bands went through during a `process`: the highest level the
/// detectors saw (linear) and the most they were reduced (dB).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BandActivity {
    pub level: [f32; 3],
    pub reduction_db: [f32; 3],
}

/// The frequency response of a compressor's crossovers, for drawing it.
pub struct CrossoverResponse {
    low_pass: [Biquad; 2],
    high_pass: [Biquad; 2],
    sample_rate: f32,
}

impl CrossoverResponse {
    pub fn new(params: &MultibandCompressor, sample_rate: u32) -> Self {
        let processor = MultibandProcessor::new(params, sample_rate, 1);
        Self {
            low_pass: processor.low_pass,
            high_pass: processor.high_pass,
            sample_rate: sample_rate as f32,
        }
    }

    /// Gain in dB at `freq_hz` with each band at `gains_db`: the bands
    /// weighted and summed through the same filters as the audio.
    pub fn gain_db(&self, gains_db: [f32; 3], freq_hz: f32) -> f32 {
        let [lp1, lp2] = &self.low_pass;
        let [hp1, hp2] = &self.high_pass;
        let (z1, z2) = unit_delays(freq_hz, self.sample_rate);
        let allpass = lp2.response_lr4(z1, z2).add(hp2.response_lr4(z1, z2));
        let low = lp1.response_lr4(z1, z2).mul(allpass);
        let rest = hp1.response_lr4(z1, z2);
        let mid = rest.mul(lp2.response_lr4(z1, z2));
        let high = rest.mul(hp2.response_lr4(z1, z2));
        let [g_low, g_mid, g_high] = gains_db.map(db_to_linear);
        let sum = low
            .scale(g_low)
            .add(mid.scale(g_mid))
            .add(high.scale(g_high));
        sum.power_db()
    }
}

#[cfg(test)]
#[path = "tests/dynamics.rs"]
mod tests;
