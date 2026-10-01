//! Device audio: mic capture to mono frames, and a playback buffer the speaker feeds.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};

/// Mono f32 frames from the mic, at `rate`.
pub struct Frames {
    pub rx: mpsc::Receiver<Vec<f32>>,
    pub rate: u32,
}

/// PipeWire when it is running (it lists real nodes, e.g. Bluetooth headsets), else the
/// platform default host.
fn host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if let Ok(h) = cpal::host_from_id(cpal::HostId::PipeWire) {
        return h;
    }
    cpal::default_host()
}

fn find(host: &cpal::Host, want: Option<&str>, input: bool) -> Result<cpal::Device> {
    if let Some(w) = want {
        let devs: Vec<_> = if input { host.input_devices()?.collect() } else { host.output_devices()?.collect() };
        return devs
            .into_iter()
            .find(|d| d.to_string().contains(w) || d.id().map(|i| i.to_string() == w).unwrap_or(false))
            .ok_or_else(|| anyhow!("no audio device matching {w}"));
    }
    let d = if input { host.default_input_device() } else { host.default_output_device() };
    d.ok_or_else(|| anyhow!("no default {} device", if input { "input" } else { "output" }))
}

pub fn list() -> Result<(Vec<String>, Vec<String>)> {
    let host = host();
    let ins = host.input_devices()?.map(|d| d.to_string()).collect();
    let outs = host.output_devices()?.map(|d| d.to_string()).collect();
    Ok((ins, outs))
}

/// Open the mic on its own thread (cpal streams are not Send everywhere). Frames are dropped,
/// not queued, while `gate` is false.
pub fn capture(device: Option<String>, gate: Arc<AtomicBool>) -> Result<Frames> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();
    std::thread::Builder::new().name("parley-mic".into()).spawn(move || {
        let run = || -> Result<(cpal::Stream, u32)> {
            let host = host();
            let dev = find(&host, device.as_deref(), true)?;
            let cfg = dev.default_input_config().context("mic config")?;
            let rate = cfg.sample_rate();
            let ch = cfg.channels() as usize;
            eprintln!("mic: {dev} at {rate} Hz, {ch} ch, {:?}", cfg.sample_format());
            let stream = match cfg.sample_format() {
                SampleFormat::F32 => input::<f32>(&dev, cfg.into(), ch, tx, gate)?,
                SampleFormat::I16 => input::<i16>(&dev, cfg.into(), ch, tx, gate)?,
                f => return Err(anyhow!("unsupported mic sample format {f:?}")),
            };
            stream.play()?;
            Ok((stream, rate))
        };
        match run() {
            Ok((stream, rate)) => {
                let _ = ready_tx.send(Ok(rate));
                let _keep = stream;
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    })?;
    let rate = ready_rx.recv().context("mic thread died")??;
    Ok(Frames { rx, rate })
}

trait ToF32: SizedSample + Send + 'static {
    fn f(self) -> f32;
}
impl ToF32 for f32 {
    fn f(self) -> f32 {
        self
    }
}
impl ToF32 for i16 {
    fn f(self) -> f32 {
        self as f32 / 32768.0
    }
}

fn input<T: ToF32>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    ch: usize,
    tx: mpsc::Sender<Vec<f32>>,
    gate: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    Ok(dev.build_input_stream(
        cfg,
        move |data: &[T], _: &_| {
            if !gate.load(Ordering::Relaxed) {
                return;
            }
            let mono: Vec<f32> = data.chunks(ch).map(|f| f.iter().map(|s| s.f()).sum::<f32>() / ch as f32).collect();
            let _ = tx.send(mono);
        },
        |e| eprintln!("mic stream: {e}"),
        None,
    )?)
}

/// Samples waiting to be played, at the output device rate, plus a level meter.
pub struct Player {
    buf: Mutex<VecDeque<f32>>,
    pub rate: u32,
    level: AtomicU32,
}

impl Player {
    pub fn push(&self, pcm: &[f32], from_rate: u32) {
        let out = resample(pcm, from_rate, self.rate);
        self.buf.lock().unwrap().extend(out);
    }
    pub fn queued_secs(&self) -> f32 {
        self.buf.lock().unwrap().len() as f32 / self.rate as f32
    }
    pub fn clear(&self) {
        self.buf.lock().unwrap().clear();
        self.level.store(0f32.to_bits(), Ordering::Relaxed);
    }
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

pub fn playback(device: Option<String>) -> Result<Arc<Player>> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<Arc<Player>>>();
    std::thread::Builder::new().name("parley-speaker".into()).spawn(move || {
        let run = || -> Result<(cpal::Stream, Arc<Player>)> {
            let host = host();
            let dev = find(&host, device.as_deref(), false)?;
            let cfg = dev.default_output_config().context("speaker config")?;
            let rate = cfg.sample_rate();
            let ch = cfg.channels() as usize;
            eprintln!("speaker: {dev} at {rate} Hz, {ch} ch, {:?}", cfg.sample_format());
            let p = Arc::new(Player { buf: Mutex::new(VecDeque::new()), rate, level: AtomicU32::new(0) });
            let stream = match cfg.sample_format() {
                SampleFormat::F32 => output::<f32>(&dev, cfg.into(), ch, p.clone(), |v| v)?,
                SampleFormat::I16 => output::<i16>(&dev, cfg.into(), ch, p.clone(), |v| (v * 32767.0) as i16)?,
                f => return Err(anyhow!("unsupported speaker sample format {f:?}")),
            };
            stream.play()?;
            Ok((stream, p))
        };
        match run() {
            Ok((stream, p)) => {
                let _ = ready_tx.send(Ok(p));
                let _keep = stream;
                loop {
                    std::thread::park();
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    })?;
    ready_rx.recv().context("speaker thread died")?
}

fn output<T: SizedSample + Send + 'static>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    ch: usize,
    p: Arc<Player>,
    conv: fn(f32) -> T,
) -> Result<cpal::Stream> {
    Ok(dev.build_output_stream(
        cfg,
        move |out: &mut [T], _: &_| {
            let mut buf = p.buf.lock().unwrap();
            let mut sq = 0f32;
            let frames = out.len() / ch;
            for f in out.chunks_mut(ch) {
                let v = buf.pop_front().unwrap_or(0.0);
                sq += v * v;
                for s in f {
                    *s = conv(v);
                }
            }
            let rms = (sq / frames.max(1) as f32).sqrt();
            p.level.store((rms * 4.0).min(1.0).to_bits(), Ordering::Relaxed);
        },
        |e| eprintln!("speaker stream: {e}"),
        None,
    )?)
}

/// Linear interpolation. Good enough for speech going to a speaker; the recognizer and the
/// turn detector take their input at the mic rate or do their own resampling.
pub fn resample(pcm: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || pcm.is_empty() {
        return pcm.to_vec();
    }
    let n = (pcm.len() as u64 * to as u64 / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..n)
        .map(|i| {
            let x = i as f64 * step;
            let j = x as usize;
            let t = (x - j as f64) as f32;
            let a = pcm[j.min(pcm.len() - 1)];
            let b = pcm[(j + 1).min(pcm.len() - 1)];
            a + (b - a) * t
        })
        .collect()
}

pub fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt()
}

/// Test source: WAV files played at real-time pace in 20 ms frames, after `delay` seconds of
/// silence, with `gap` seconds of silence after each, then silence forever.
pub fn from_wavs(paths: Vec<std::path::PathBuf>, delay: f32, gap: f32, gate: Arc<AtomicBool>) -> Result<Frames> {
    let rate = 16000u32;
    let clips: Vec<Vec<f32>> = paths
        .iter()
        .map(|p| {
            let (pcm, r) = crate::wav::read(p).with_context(|| format!("read {}", p.display()))?;
            Ok(resample(&pcm, r, rate))
        })
        .collect::<Result<_>>()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("parley-wav".into()).spawn(move || {
        let frame = rate as usize / 50;
        let silence = vec![0f32; frame];
        let send = |f: &[f32]| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if gate.load(Ordering::Relaxed) {
                tx.send(f.to_vec()).is_ok()
            } else {
                true
            }
        };
        for _ in 0..(delay * 50.0) as usize {
            if !send(&silence) {
                return;
            }
        }
        for c in &clips {
            for f in c.chunks(frame) {
                if !send(f) {
                    return;
                }
            }
            for _ in 0..(gap * 50.0) as usize {
                if !send(&silence) {
                    return;
                }
            }
        }
        while send(&silence) {}
    })?;
    Ok(Frames { rx, rate })
}
