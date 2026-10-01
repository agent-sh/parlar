//! Smart Turn v3.2: an audio model that says whether the user finished their turn. The features
//! mirror Hugging Face's WhisperFeatureExtractor(chunk_length=8) as Pipecat runs it:
//! normalized waveform, right-padded to 8 s, 80 Slaney mel bins, log10, clamp to max - 8,
//! (x + 4) / 4.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use realfft::{RealFftPlanner, RealToComplex};

pub const RATE: usize = 16000;
const SECS: usize = 8;
const N_FFT: usize = 400;
const HOP: usize = 160;
const N_MELS: usize = 80;
const FRAMES: usize = SECS * RATE / HOP;

pub struct Features {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// [N_MELS][N_FFT / 2 + 1]
    mel: Vec<Vec<f32>>,
}

impl Features {
    pub fn new() -> Features {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT);
        // periodic Hann
        let window = (0..N_FFT).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N_FFT as f32).cos()).collect();
        Features { fft, window, mel: mel_filters() }
    }

    /// Log-mel features for the last 8 s of `audio` (16 kHz), row-major [80][800].
    pub fn compute(&self, audio: &[f32]) -> Vec<f32> {
        let audio = &audio[audio.len().saturating_sub(SECS * RATE)..];
        let n = audio.len();
        let mean = audio.iter().map(|&v| v as f64).sum::<f64>() / n.max(1) as f64;
        let var = audio.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n.max(1) as f64;
        let sd = (var + 1e-7).sqrt();
        let mut x = vec![0f32; SECS * RATE];
        for (o, &v) in x.iter_mut().zip(audio) {
            *o = ((v as f64 - mean) / sd) as f32;
        }
        // center the frames: reflect-pad n_fft / 2 on both sides
        let pad = N_FFT / 2;
        let len = x.len();
        let at = |i: isize| -> f32 {
            let i = if i < 0 { -i } else if i as usize >= len { 2 * (len as isize - 1) - i } else { i };
            x[i as usize]
        };
        let bins = N_FFT / 2 + 1;
        let mut frame = self.fft.make_input_vec();
        let mut spec = self.fft.make_output_vec();
        let mut out = vec![0f32; N_MELS * FRAMES];
        let mut power = vec![0f32; bins];
        for t in 0..FRAMES {
            let start = (t * HOP) as isize - pad as isize;
            for (k, f) in frame.iter_mut().enumerate() {
                *f = at(start + k as isize) * self.window[k];
            }
            self.fft.process(&mut frame, &mut spec).expect("fft sizes match");
            for (p, c) in power.iter_mut().zip(&spec) {
                *p = c.norm_sqr();
            }
            for (m, filt) in self.mel.iter().enumerate() {
                let e: f32 = filt.iter().zip(&power).map(|(a, b)| a * b).sum();
                out[m * FRAMES + t] = e.max(1e-10).log10();
            }
        }
        let max = out.iter().copied().fold(f32::MIN, f32::max);
        for v in &mut out {
            *v = (v.max(max - 8.0) + 4.0) / 4.0;
        }
        out
    }
}

fn hz_to_mel(f: f64) -> f64 {
    let (f_sp, min_log_hz, min_log_mel, logstep) = (200.0 / 3.0, 1000.0, 15.0, (6.4f64).ln() / 27.0);
    if f >= min_log_hz { min_log_mel + (f / min_log_hz).ln() / logstep } else { f / f_sp }
}

fn mel_to_hz(m: f64) -> f64 {
    let (f_sp, min_log_hz, min_log_mel, logstep) = (200.0 / 3.0, 1000.0, 15.0, (6.4f64).ln() / 27.0);
    if m >= min_log_mel { min_log_hz * ((m - min_log_mel) * logstep).exp() } else { m * f_sp }
}

/// Slaney-scale, Slaney-normalized triangular filters, as librosa and Hugging Face build them.
fn mel_filters() -> Vec<Vec<f32>> {
    let bins = N_FFT / 2 + 1;
    let fft_freqs: Vec<f64> = (0..bins).map(|i| i as f64 * (RATE as f64 / 2.0) / (bins - 1) as f64).collect();
    let (lo, hi) = (hz_to_mel(0.0), hz_to_mel(8000.0));
    let pts: Vec<f64> = (0..N_MELS + 2).map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (N_MELS + 1) as f64)).collect();
    (0..N_MELS)
        .map(|m| {
            let (l, c, r) = (pts[m], pts[m + 1], pts[m + 2]);
            let norm = 2.0 / (r - l);
            fft_freqs
                .iter()
                .map(|&f| {
                    let down = (f - l) / (c - l);
                    let up = (r - f) / (r - c);
                    (down.min(up).max(0.0) * norm) as f32
                })
                .collect()
        })
        .collect()
}

/// Point `ort` at the ONNX Runtime that libmoonshine already loaded, once per process.
pub fn init_ort(lib: &Path) -> Result<()> {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if DONE.get().is_none() {
        ort::init_from(lib.display().to_string()).commit().context("init onnx runtime")?;
        let _ = DONE.set(());
    }
    Ok(())
}

pub struct SmartTurn {
    session: ort::session::Session,
    features: Features,
}

impl SmartTurn {
    /// `ort_lib` is the ONNX Runtime shared library already loaded by libmoonshine, so the process
    /// carries one runtime.
    pub fn load(model: &Path, ort_lib: &Path) -> Result<SmartTurn> {
        init_ort(ort_lib)?;
        let session = ort::session::Session::builder()?
            .with_intra_threads(1)?
            .commit_from_file(model)
            .with_context(|| format!("load {}", model.display()))?;
        Ok(SmartTurn { session, features: Features::new() })
    }

    /// Probability that the turn is complete, for the turn's 16 kHz audio.
    pub fn complete(&mut self, audio: &[f32]) -> Result<f32> {
        let f = self.features.compute(audio);
        let input = ort::value::Tensor::from_array(([1usize, N_MELS, FRAMES], f))?;
        let out = self.session.run(ort::inputs!["input_features" => input])?;
        let (_, p) = out[0].try_extract_tensor::<f32>()?;
        Ok(p[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_have_slaney_shape() {
        let m = mel_filters();
        assert_eq!(m.len(), 80);
        assert_eq!(m[0].len(), 201);
        // every filter has some weight, and the first one sits at the bottom of the spectrum
        assert!(m.iter().all(|f| f.iter().any(|&v| v > 0.0)));
        assert!(m[0][1] > 0.0 && m[0][20] == 0.0);
    }

    #[test]
    fn features_have_model_shape_and_range() {
        let f = Features::new();
        let tone: Vec<f32> = (0..RATE * 2).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin()).collect();
        let out = f.compute(&tone);
        assert_eq!(out.len(), 80 * 800);
        let max = out.iter().copied().fold(f32::MIN, f32::max);
        let min = out.iter().copied().fold(f32::MAX, f32::min);
        assert!((min - (max - 2.0)).abs() < 1e-4, "clamped to max - 8 before scaling by 1/4");
    }
}
