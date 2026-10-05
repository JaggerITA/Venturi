//! Voice activity detection with Silero VAD (MIT, see `model/`), run by
//! tract. The model is frozen to 16 kHz by `scripts/freeze_silero_vad.py`:
//! the original's `If` branches on the rate are not supported by tract.

use std::sync::{Arc, OnceLock};

use tract_onnx::prelude::*;

pub const SAMPLE_RATE: u32 = 16_000;
/// Samples per probability.
pub const CHUNK: usize = 512;
pub const WINDOW_SECS: f64 = CHUNK as f64 / SAMPLE_RATE as f64;
/// Tail of the previous chunk the model expects in front of each one.
const CONTEXT: usize = 64;

const MODEL: &[u8] = include_bytes!("../model/silero_vad_16k.onnx");

type Plan = Arc<TypedSimplePlan>;

fn plan() -> Result<Plan, String> {
    static PLAN: OnceLock<Result<Plan, String>> = OnceLock::new();
    let load = || -> TractResult<Plan> {
        tract_onnx::onnx()
            .model_for_read(&mut &*MODEL)?
            .with_input_fact(0, f32::fact([1, CONTEXT + CHUNK]).into())?
            .with_input_fact(1, f32::fact([2, 1, 128]).into())?
            .into_optimized()?
            .into_runnable()
    };
    PLAN.get_or_init(|| load().map_err(|e| e.to_string()))
        .clone()
}

/// Speech probability, in [0, 1], of each `CHUNK` of a mono 16 kHz signal
/// pushed in pieces of any size.
pub struct Vad {
    plan: Plan,
    state: Tensor,
    /// The context followed by the samples not yet run.
    pending: Vec<f32>,
    probabilities: Vec<f32>,
}

impl Vad {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            plan: plan()?,
            state: Tensor::zero::<f32>(&[2, 1, 128]).map_err(|e| e.to_string())?,
            pending: vec![0.0; CONTEXT],
            probabilities: Vec::new(),
        })
    }

    pub fn push(&mut self, samples: &[f32]) -> Result<(), String> {
        self.pending.extend_from_slice(samples);
        let mut start = 0;
        while self.pending.len() - start >= CONTEXT + CHUNK {
            let input = self.pending[start..start + CONTEXT + CHUNK].to_vec();
            let probability = self.run(input).map_err(|e| e.to_string())?;
            self.probabilities.push(probability);
            start += CHUNK;
        }
        self.pending.drain(..start);
        Ok(())
    }

    /// The last partial chunk is padded with silence.
    pub fn finish(mut self) -> Result<Vec<f32>, String> {
        if self.pending.len() > CONTEXT {
            let padding = CONTEXT + CHUNK - self.pending.len();
            self.push(&vec![0.0; padding])?;
        }
        Ok(self.probabilities)
    }

    fn run(&mut self, input: Vec<f32>) -> TractResult<f32> {
        let input = tract_ndarray::Array2::from_shape_vec((1, input.len()), input)?;
        let state = std::mem::replace(&mut self.state, Tensor::zero::<f32>(&[0])?);
        let outputs = self
            .plan
            .run(tvec![input.into_tensor().into(), state.into()])?;
        let probability = *outputs[0]
            .to_plain_array_view::<f32>()?
            .iter()
            .next()
            .ok_or_else(|| TractError::msg("the model returned no probability"))?;
        self.state = outputs[1].clone().into_tensor();
        Ok(probability)
    }
}

#[cfg(test)]
#[path = "tests/lib.rs"]
mod tests;
