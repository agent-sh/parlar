//! Echo cancellation (WebRTC AEC3 through sonora): removes the agent's own voice from the mic so
//! the user can talk over it on speakers, not only on headphones.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use sonora::config::{AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel};
use sonora::{AudioProcessing, Config, StreamConfig};

pub const RATE: u32 = 16000;
const FRAME: usize = (RATE / 100) as usize;
/// Far-end audio older than this is dropped; the canceller's delay estimator covers far less.
const FAR_CAP: usize = RATE as usize;

/// What the speaker actually played, at 16 kHz, waiting to be matched against the mic.
#[derive(Clone, Default)]
pub struct Far(Arc<Mutex<VecDeque<f32>>>);

impl Far {
    pub fn push(&self, pcm: &[f32]) {
        let mut q = self.0.lock().unwrap();
        q.extend(pcm);
        let over = q.len().saturating_sub(FAR_CAP);
        q.drain(..over);
    }

    fn take(&self, out: &mut [f32]) {
        let mut q = self.0.lock().unwrap();
        for o in out.iter_mut() {
            *o = q.pop_front().unwrap_or(0.0);
        }
    }
}

pub struct Aec {
    apm: AudioProcessing,
    far: Far,
    pending: Vec<f32>,
    far_frame: Vec<f32>,
    scratch: Vec<f32>,
    clean: Vec<f32>,
}

impl Aec {
    /// Echo cancellation against `far`, plus a high-pass filter, strong noise suppression and
    /// adaptive gain so a quiet voice on a hissy laptop mic still reaches the recognizer.
    pub fn new(far: Far) -> Aec {
        Aec::with(far, true)
    }

    /// The same cleanup without echo cancellation (no speaker to cancel).
    pub fn cleanup_only() -> Aec {
        Aec::with(Far::default(), false)
    }

    fn with(far: Far, echo: bool) -> Aec {
        let sc = StreamConfig::new(RATE, 1);
        let config = Config {
            echo_canceller: echo.then(EchoCanceller::default),
            high_pass_filter: Some(HighPassFilter::default()),
            noise_suppression: Some(NoiseSuppression { level: NoiseSuppressionLevel::High, ..Default::default() }),
            gain_controller2: Some(GainController2 {
                adaptive_digital: Some(AdaptiveDigital::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut apm = AudioProcessing::builder()
            .config(config)
            .capture_config(sc)
            .render_config(sc)
            .build();
        // a hint: output buffering plus capture latency on a typical desktop; AEC3 refines it
        let _ = apm.set_stream_delay_ms(60);
        Aec { apm, far, pending: Vec::new(), far_frame: vec![0.0; FRAME], scratch: vec![0.0; FRAME], clean: vec![0.0; FRAME] }
    }

    /// Mic audio in, echo-cancelled audio out, in 10 ms steps (a remainder waits for the next call).
    pub fn process(&mut self, mic: &[f32]) -> Vec<f32> {
        self.pending.extend_from_slice(mic);
        let mut out = Vec::with_capacity(self.pending.len());
        let mut used = 0;
        while self.pending.len() - used >= FRAME {
            self.far.take(&mut self.far_frame);
            let _ = self.apm.process_render_f32(&[&self.far_frame], &mut [&mut self.scratch]);
            let frame = &self.pending[used..used + FRAME];
            let _ = self.apm.process_capture_f32(&[frame], &mut [&mut self.clean]);
            out.extend_from_slice(&self.clean);
            used += FRAME;
        }
        self.pending.drain(..used);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn cancels_a_delayed_echo() {
        // far end: a speech-like signal (noise shaped into syllables)
        let n = RATE as usize * 6;
        let mut seed = 12345u32;
        let far: Vec<f32> = (0..n)
            .map(|i| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let env = ((i as f32 / RATE as f32) * 4.0 * std::f32::consts::PI).sin().abs();
                noise * env * 0.6
            })
            .collect();
        // mic: the far end 40 ms later at 60%, nothing else
        let delay = RATE as usize * 40 / 1000;
        let mic: Vec<f32> = (0..n).map(|i| if i >= delay { far[i - delay] * 0.6 } else { 0.0 }).collect();

        let f = Far::default();
        let mut aec = Aec::new(f.clone());
        let mut out = Vec::new();
        for (fc, mc) in far.chunks(FRAME).zip(mic.chunks(FRAME)) {
            f.push(fc);
            out.extend(aec.process(mc));
        }
        // judge the last two seconds, after the canceller has converged
        let tail = RATE as usize * 2;
        let (m, o) = (rms(&mic[n - tail..]), rms(&out[out.len() - tail..]));
        eprintln!("echo reduction: {:.1} dB", 20.0 * (m / o.max(1e-9)).log10());
        assert!(o < m * 0.25, "echo should drop by 12 dB or more: mic {m:.4}, out {o:.4}");
    }
}
