//! Biquad filters, from the RBJ audio EQ cookbook, and their frequency
//! response for drawing them.

use vv_core::EqShape;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    /// Butterworth (Q = 1/√2) low or high pass.
    pub(crate) fn butterworth(freq: f32, sample_rate: f32, high: bool) -> Self {
        let shape = if high {
            EqShape::HighPass
        } else {
            EqShape::LowPass
        };
        Self::new(
            shape,
            freq,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            sample_rate,
        )
    }

    /// `gain_db` is ignored by the passes.
    pub(crate) fn new(shape: EqShape, freq: f32, gain_db: f32, q: f32, sample_rate: f32) -> Self {
        let w = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let alpha = w.sin() / (2.0 * q.max(0.01));
        let cos = w.cos();
        let a = 10f32.powf(gain_db / 40.0);
        let shelf = 2.0 * a.sqrt() * alpha;
        let [b0, b1, b2, a0, a1, a2] = match shape {
            EqShape::LowPass => {
                let b = (1.0 - cos) / 2.0;
                [b, 1.0 - cos, b, 1.0 + alpha, -2.0 * cos, 1.0 - alpha]
            }
            EqShape::HighPass => {
                let b = (1.0 + cos) / 2.0;
                [b, -(1.0 + cos), b, 1.0 + alpha, -2.0 * cos, 1.0 - alpha]
            }
            EqShape::Peak => [
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ],
            EqShape::LowShelf => [
                a * ((a + 1.0) - (a - 1.0) * cos + shelf),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - shelf),
                (a + 1.0) + (a - 1.0) * cos + shelf,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - shelf,
            ],
            EqShape::HighShelf => [
                a * ((a + 1.0) + (a - 1.0) * cos + shelf),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - shelf),
                (a + 1.0) - (a - 1.0) * cos + shelf,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - shelf,
            ],
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// Transposed direct form II.
    pub(crate) fn tick(&self, z: &mut [f32; 2], x: f32) -> f32 {
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }

    /// Linkwitz-Riley 4th order: the same Butterworth twice.
    pub(crate) fn tick_lr4(&self, z: &mut [[f32; 2]], x: f32) -> f32 {
        let y = self.tick(&mut z[0], x);
        self.tick(&mut z[1], y)
    }

    pub(crate) fn response(&self, z1: Complex, z2: Complex) -> Complex {
        let one = Complex { re: 1.0, im: 0.0 };
        let num = one
            .scale(self.b0)
            .add(z1.scale(self.b1))
            .add(z2.scale(self.b2));
        let den = one.add(z1.scale(self.a1)).add(z2.scale(self.a2));
        num.div(den)
    }

    pub(crate) fn response_lr4(&self, z1: Complex, z2: Complex) -> Complex {
        let h = self.response(z1, z2);
        h.mul(h)
    }
}

/// z⁻¹ and z⁻² at `freq_hz`.
pub(crate) fn unit_delays(freq_hz: f32, sample_rate: f32) -> (Complex, Complex) {
    let w = 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
    let z1 = Complex {
        re: w.cos(),
        im: -w.sin(),
    };
    (z1, z1.mul(z1))
}

#[derive(Clone, Copy)]
pub(crate) struct Complex {
    pub(crate) re: f32,
    pub(crate) im: f32,
}

impl Complex {
    pub(crate) fn add(self, o: Self) -> Self {
        Self {
            re: self.re + o.re,
            im: self.im + o.im,
        }
    }

    pub(crate) fn mul(self, o: Self) -> Self {
        Self {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }

    pub(crate) fn scale(self, k: f32) -> Self {
        Self {
            re: self.re * k,
            im: self.im * k,
        }
    }

    fn div(self, o: Self) -> Self {
        let d = o.re * o.re + o.im * o.im;
        Self {
            re: (self.re * o.re + self.im * o.im) / d,
            im: (self.im * o.re - self.re * o.im) / d,
        }
    }

    pub(crate) fn power_db(self) -> f32 {
        10.0 * (self.re * self.re + self.im * self.im).max(1e-12).log10()
    }
}
