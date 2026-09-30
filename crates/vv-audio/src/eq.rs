//! Parametric equalizer: one biquad per band, in series.

use vv_core::{EqBand, Equalizer};

use crate::filter::{Biquad, Complex, unit_delays};

const BANDS: usize = 6;

type ChannelMemory = [[f32; 2]; BANDS];

/// The filter of `band`, `None` when it leaves the audio as it is.
fn band_filter(band: &EqBand, sample_rate: f32) -> Option<Biquad> {
    let flat = band.shape.has_gain() && band.gain_db == 0.0;
    (band.enabled && !flat).then(|| {
        let freq = band.freq_hz.clamp(10.0, sample_rate * 0.45);
        Biquad::new(band.shape, freq, band.gain_db, band.q, sample_rate)
    })
}

pub struct EqProcessor {
    filters: [Option<Biquad>; BANDS],
    memory: Vec<ChannelMemory>,
}

impl EqProcessor {
    pub fn new(params: &Equalizer, sample_rate: u32, channels: u16) -> Self {
        let rate = sample_rate as f32;
        Self {
            filters: params.bands.map(|b| band_filter(&b, rate)),
            memory: vec![[[0.0; 2]; BANDS]; channels.max(1) as usize],
        }
    }

    pub fn reset(&mut self) {
        self.memory.fill([[0.0; 2]; BANDS]);
    }

    /// Continues from where `other` was, if it has the same channels: a
    /// band being dragged must not click at every step.
    pub fn take_memory_from(&mut self, other: &Self) {
        if self.memory.len() == other.memory.len() {
            self.memory.copy_from_slice(&other.memory);
        }
    }

    /// In place, interleaved. No allocations: it runs in the realtime
    /// callback.
    pub fn process(&mut self, samples: &mut [f32]) {
        let ch = self.memory.len();
        for frame in samples.chunks_exact_mut(ch) {
            for (x, memory) in frame.iter_mut().zip(&mut self.memory) {
                for (filter, z) in self.filters.iter().zip(memory.iter_mut()) {
                    if let Some(filter) = filter {
                        *x = filter.tick(z, *x);
                    }
                }
            }
        }
    }
}

/// The frequency response of an equalizer, for drawing it.
pub struct EqResponse {
    filters: [Option<Biquad>; BANDS],
    sample_rate: f32,
}

impl EqResponse {
    pub fn new(params: &Equalizer, sample_rate: u32) -> Self {
        let rate = sample_rate as f32;
        Self {
            filters: params.bands.map(|b| band_filter(&b, rate)),
            sample_rate: rate,
        }
    }

    fn band_response(&self, band: usize, freq_hz: f32) -> Option<Complex> {
        let (z1, z2) = unit_delays(freq_hz, self.sample_rate);
        Some(self.filters[band]?.response(z1, z2))
    }

    /// Gain in dB of `band` alone at `freq_hz`; 0 when it is off.
    pub fn band_gain_db(&self, band: usize, freq_hz: f32) -> f32 {
        self.band_response(band, freq_hz)
            .map_or(0.0, Complex::power_db)
    }

    /// Gain in dB of all the bands together at `freq_hz`.
    pub fn gain_db(&self, freq_hz: f32) -> f32 {
        (0..BANDS).map(|b| self.band_gain_db(b, freq_hz)).sum()
    }
}

#[cfg(test)]
#[path = "tests/eq.rs"]
mod tests;
