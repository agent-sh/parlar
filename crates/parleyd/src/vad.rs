//! Silero VAD v6: speech probability per 32 ms, so the recognizer only runs while someone talks.

use std::path::Path;

use anyhow::{Context, Result};

pub const RATE: usize = 16000;
const WINDOW: usize = 512;
const CONTEXT: usize = 64;

pub struct Vad {
    session: ort::session::Session,
    state: Vec<f32>,
    ctx: [f32; CONTEXT],
    pending: Vec<f32>,
}

impl Vad {
    pub fn load(model: &Path, ort_lib: &Path) -> Result<Vad> {
        crate::turn::init_ort(ort_lib)?;
        let session = ort::session::Session::builder()?
            .with_intra_threads(1)?
            .commit_from_file(model)
            .with_context(|| format!("load {}", model.display()))?;
        Ok(Vad { session, state: vec![0.0; 2 * 128], ctx: [0.0; CONTEXT], pending: Vec::new() })
    }

    /// Feed 16 kHz audio; returns one speech probability per completed 32 ms window.
    pub fn push(&mut self, pcm: &[f32]) -> Result<Vec<f32>> {
        self.pending.extend_from_slice(pcm);
        let mut out = Vec::new();
        let mut used = 0;
        while self.pending.len() - used >= WINDOW {
            let mut input = Vec::with_capacity(CONTEXT + WINDOW);
            input.extend_from_slice(&self.ctx);
            input.extend_from_slice(&self.pending[used..used + WINDOW]);
            self.ctx.copy_from_slice(&self.pending[used + WINDOW - CONTEXT..used + WINDOW]);
            let x = ort::value::Tensor::from_array(([1usize, CONTEXT + WINDOW], input))?;
            let st = ort::value::Tensor::from_array(([2usize, 1, 128], self.state.clone()))?;
            // the model takes the sample rate as a scalar
            let sr = ort::value::Tensor::from_array(([0usize; 0], vec![RATE as i64]))?;
            let res = self.session.run(ort::inputs!["input" => x, "state" => st, "sr" => sr])?;
            let (_, p) = res["output"].try_extract_tensor::<f32>()?;
            out.push(p[0]);
            let (_, s) = res["stateN"].try_extract_tensor::<f32>()?;
            self.state.copy_from_slice(&s[..2 * 128]);
            used += WINDOW;
        }
        self.pending.drain(..used);
        Ok(out)
    }
}

/// Opens on speech and stays open through short gaps, so the recognizer sees whole phrases and
/// the trailing silence it needs to finish a line.
pub struct Gate {
    open: bool,
    quiet_windows: usize,
}

/// Probability that opens the gate, and the lower one that keeps it open (hysteresis).
const OPEN_AT: f32 = 0.5;
const STAY_AT: f32 = 0.35;
/// About 1.5 s of quiet windows before the gate closes.
const HANGOVER: usize = 47;

impl Gate {
    pub fn new() -> Gate {
        Gate { open: false, quiet_windows: 0 }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Returns true when the gate just opened.
    pub fn update(&mut self, probs: &[f32]) -> bool {
        let mut opened = false;
        for &p in probs {
            if !self.open {
                if p >= OPEN_AT {
                    self.open = true;
                    self.quiet_windows = 0;
                    opened = true;
                }
            } else if p < STAY_AT {
                self.quiet_windows += 1;
                if self.quiet_windows >= HANGOVER {
                    self.open = false;
                }
            } else {
                self.quiet_windows = 0;
            }
        }
        opened
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_opens_on_speech_and_closes_after_hangover() {
        let mut g = Gate::new();
        assert!(!g.update(&[0.1, 0.2]));
        assert!(g.update(&[0.9]));
        assert!(g.is_open());
        g.update(&[0.1; HANGOVER - 1]);
        assert!(g.is_open(), "short pauses keep it open");
        g.update(&[0.4]);
        g.update(&[0.1; HANGOVER - 1]);
        assert!(g.is_open(), "a mid-level window resets the hangover");
        g.update(&[0.1]);
        assert!(!g.is_open());
    }
}
