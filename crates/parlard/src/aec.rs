//! Echo cancellation (WebRTC AEC3 through sonora): removes the agent's own voice from the mic so
//! the user can talk over it on speakers, not only on headphones.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel,
};
use sonora::{AudioProcessing, Config, StreamConfig};

pub const RATE: u32 = 16000;
const FRAME: usize = (RATE / 100) as usize;
/// Far-end audio older than this is dropped; the canceller's delay estimator covers far less.
const FAR_CAP: usize = RATE as usize;
/// Far-end audio kept beyond what the pending mic audio needs, on top of the speaker's own
/// latency: the mic's capture latency and the callback size. AEC3 cancels well with the far end
/// up to a few hundred ms ahead of the echo, and not at all once it falls behind.
const FAR_SLACK: usize = (RATE / 20) as usize;
/// The speaker latency assumed until the stream reports one: a far end that leads by too much
/// costs a little cancellation, one that lags costs all of it.
const DEFAULT_LATENCY: usize = (RATE / 10) as usize;

/// What the speaker actually played, at 16 kHz, waiting to be matched against the mic.
#[derive(Clone)]
pub struct Far(Arc<FarQueue>);

struct FarQueue {
    q: Mutex<VecDeque<f32>>,
    /// Samples from the speaker callback handing audio over to it reaching the speaker.
    latency: AtomicUsize,
}

impl Default for Far {
    fn default() -> Far {
        Far(Arc::new(FarQueue { q: Mutex::default(), latency: AtomicUsize::new(DEFAULT_LATENCY) }))
    }
}

impl Far {
    /// The speaker's output latency, as its stream reports it: the far end pushed now is heard
    /// that much later, so that much more of it must wait for the echo.
    pub fn set_latency(&self, latency: std::time::Duration) {
        let n = (latency.as_secs_f64() * RATE as f64) as usize;
        self.0.latency.store(n.min(FAR_CAP / 2), Ordering::Relaxed);
    }

    pub fn push(&self, pcm: &[f32]) {
        let mut q = self.0.q.lock().unwrap();
        q.extend(pcm);
        let over = q.len().saturating_sub(FAR_CAP);
        q.drain(..over);
    }

    /// The oldest far-end audio for one mic frame, with `ahead` more mic samples still waiting.
    /// A backlog beyond that plus the slack (the speaker kept playing while no mic audio came
    /// in) is dropped from the old end so the far end stays level with the mic.
    fn take(&self, out: &mut [f32], ahead: usize) {
        let keep = out.len() + ahead + FAR_SLACK + self.0.latency.load(Ordering::Relaxed);
        let mut q = self.0.q.lock().unwrap();
        let over = q.len().saturating_sub(keep);
        q.drain(..over);
        for o in out.iter_mut() {
            *o = q.pop_front().unwrap_or(0.0);
        }
    }

    pub fn clear(&self) {
        self.0.q.lock().unwrap().clear();
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
            gain_controller2: (std::env::var_os("PARLAR_AEC_NO_GAIN").is_none())
                .then(|| GainController2 { adaptive_digital: Some(AdaptiveDigital::default()), ..Default::default() }),
            ..Default::default()
        };
        let mut apm = AudioProcessing::builder().config(config).capture_config(sc).render_config(sc).build();
        // a hint: output buffering plus capture latency on a typical desktop; AEC3 refines it
        let _ = apm.set_stream_delay_ms(60);
        Aec {
            apm,
            far,
            pending: Vec::new(),
            far_frame: vec![0.0; FRAME],
            scratch: vec![0.0; FRAME],
            clean: vec![0.0; FRAME],
        }
    }

    /// Capture resumed after a gap: what the speaker played meanwhile no longer lines up with
    /// the mic, and neither does a leftover partial frame.
    pub fn resume(&mut self) {
        self.far.clear();
        self.pending.clear();
    }

    /// Mic audio in, echo-cancelled audio out, in 10 ms steps (a remainder waits for the next call).
    pub fn process(&mut self, mic: &[f32]) -> Vec<f32> {
        self.pending.extend_from_slice(mic);
        let mut out = Vec::with_capacity(self.pending.len());
        let mut used = 0;
        while self.pending.len() - used >= FRAME {
            let ahead = self.pending.len() - used - FRAME;
            self.far.take(&mut self.far_frame, ahead);
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

    #[test]
    fn far_backlog_stays_level_with_the_mic() {
        let f = Far::default();
        // the speaker played a second while the mic gate was closed
        f.push(&vec![0.5; RATE as usize]);
        let mut out = vec![0.0; FRAME];
        f.take(&mut out, FRAME * 2);
        assert_eq!(f.0.q.lock().unwrap().len(), FRAME * 2 + FAR_SLACK + DEFAULT_LATENCY);
        // a speaker that reports its latency keeps that much more waiting for the echo
        f.set_latency(std::time::Duration::from_millis(200));
        f.push(&vec![0.5; RATE as usize]);
        f.take(&mut out, FRAME * 2);
        assert_eq!(f.0.q.lock().unwrap().len(), FRAME * 2 + FAR_SLACK + RATE as usize / 5);
        // a short backlog is left alone
        let g = Far::default();
        g.push(&vec![0.5; FRAME * 3]);
        g.take(&mut out, 0);
        assert_eq!(g.0.q.lock().unwrap().len(), FRAME * 2);
        g.clear();
        g.take(&mut out, 0);
        assert!(out.iter().all(|&v| v == 0.0));
    }
}
