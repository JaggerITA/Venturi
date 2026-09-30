//! Spectrum of what an effect plays, for drawing it: the output stream
//! records into a `SpectrumTap` and the interface reads it back and
//! transforms it.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Samples per analysis: at 48 kHz, 11.7 Hz per bin and 85 ms of audio.
pub const SPECTRUM_SIZE: usize = 4096;

/// The last `SPECTRUM_SIZE` samples, mixed down to mono, before and after
/// an effect. Written by the realtime callback without locks; a reader may
/// catch a frame half written, which a drawing does not notice.
#[derive(Debug)]
pub struct SpectrumTap {
    pre: Box<[AtomicU32]>,
    post: Box<[AtomicU32]>,
    written: AtomicUsize,
}

impl Default for SpectrumTap {
    fn default() -> Self {
        let ring = || (0..SPECTRUM_SIZE).map(|_| AtomicU32::new(0)).collect();
        Self {
            pre: ring(),
            post: ring(),
            written: AtomicUsize::new(0),
        }
    }
}

impl SpectrumTap {
    /// Records `samples` (interleaved) as the input of the effect.
    pub fn record_pre(&self, samples: &[f32], channels: usize) {
        self.record(&self.pre, samples, channels);
    }

    /// Records the output for the same frames as the last `record_pre`, and
    /// moves on past them.
    pub fn record_post(&self, samples: &[f32], channels: usize) {
        self.record(&self.post, samples, channels);
        let frames = samples.len() / channels.max(1);
        self.written.fetch_add(frames, Ordering::Relaxed);
    }

    fn record(&self, ring: &[AtomicU32], samples: &[f32], channels: usize) {
        let ch = channels.max(1);
        let start = self.written.load(Ordering::Relaxed);
        for (i, frame) in samples.chunks_exact(ch).enumerate() {
            let mono = frame.iter().sum::<f32>() / ch as f32;
            ring[(start + i) % SPECTRUM_SIZE].store(mono.to_bits(), Ordering::Relaxed);
        }
    }

    /// How many frames were ever recorded: unchanged between two reads, the
    /// audio stopped.
    pub fn written(&self) -> usize {
        self.written.load(Ordering::Relaxed)
    }

    /// The last `SPECTRUM_SIZE` samples before and after the effect, oldest
    /// first.
    pub fn read(&self) -> (Vec<f32>, Vec<f32>) {
        let start = self.written() % SPECTRUM_SIZE;
        let ordered = |ring: &[AtomicU32]| -> Vec<f32> {
            (0..SPECTRUM_SIZE)
                .map(|i| f32::from_bits(ring[(start + i) % SPECTRUM_SIZE].load(Ordering::Relaxed)))
                .collect()
        };
        (ordered(&self.pre), ordered(&self.post))
    }
}

/// Level of each frequency bin of `samples` (a power of two long), in dBFS:
/// a full scale sine reads 0 dB in its bin. Hann window; bins 0 to n/2.
pub fn magnitudes_db(samples: &[f32]) -> Vec<f32> {
    let n = samples.len();
    assert!(n.is_power_of_two());
    let mut re: Vec<f32> = samples
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let hann = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
            s * hann
        })
        .collect();
    let mut im = vec![0.0f32; n];
    fft(&mut re, &mut im);
    // The Hann window sums to n/2; a real sine splits into two bins.
    let scale = 2.0 / (n as f32 / 2.0);
    (0..=n / 2)
        .map(|k| {
            let magnitude = (re[k] * re[k] + im[k] * im[k]).sqrt() * scale;
            20.0 * magnitude.max(1e-10).log10()
        })
        .collect()
}

/// In place, iterative radix-2.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = -2.0 * std::f32::consts::PI / len as f32;
        let (w_re, w_im) = (angle.cos(), angle.sin());
        for start in (0..n).step_by(len) {
            let (mut t_re, mut t_im) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let x_re = re[b] * t_re - im[b] * t_im;
                let x_im = re[b] * t_im + im[b] * t_re;
                re[b] = re[a] - x_re;
                im[b] = im[a] - x_im;
                re[a] += x_re;
                im[a] += x_im;
                (t_re, t_im) = (t_re * w_re - t_im * w_im, t_re * w_im + t_im * w_re);
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
#[path = "tests/spectrum.rs"]
mod tests;
