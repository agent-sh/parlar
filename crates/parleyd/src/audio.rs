//! Device audio. The mic always delivers 16 kHz mono and the speaker always takes 24 kHz mono, so
//! switching devices (different rates, channel counts) never reaches the speech pipeline.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use parley::proto::Device;

pub const MIC_RATE: u32 = 16000;
pub const VOICE_RATE: u32 = 24000;

/// PipeWire when it is running (it lists real nodes, e.g. Bluetooth headsets), else the
/// platform default host.
fn host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if let Ok(h) = cpal::host_from_id(cpal::HostId::PipeWire) {
        return h;
    }
    cpal::default_host()
}

fn dev_id(d: &cpal::Device) -> String {
    d.id().map(|i| i.to_string()).unwrap_or_else(|_| d.to_string())
}

fn devices(host: &cpal::Host, input: bool) -> Result<Vec<cpal::Device>> {
    Ok(if input { host.input_devices()?.collect() } else { host.output_devices()?.collect() })
}

fn find(host: &cpal::Host, want: Option<&str>, input: bool) -> Result<cpal::Device> {
    if let Some(w) = want {
        let devs = devices(host, input)?;
        let by_id = devs.iter().position(|d| dev_id(d) == w);
        let by_name = || devs.iter().position(|d| d.to_string().contains(w));
        return by_id
            .or_else(by_name)
            .map(|i| devs[i].clone())
            .ok_or_else(|| anyhow!("no audio device matching {w}"));
    }
    let d = if input { host.default_input_device() } else { host.default_output_device() };
    d.ok_or_else(|| anyhow!("no default {} device", if input { "input" } else { "output" }))
}

/// Devices worth offering in a menu. On PipeWire the raw list also has sink monitors, internal
/// Bluetooth nodes and application streams; those are left out.
pub fn list() -> Result<(Vec<Device>, Vec<Device>)> {
    let host = host();
    let pick = |input: bool| -> Result<Vec<Device>> {
        let mut out = Vec::new();
        for d in devices(&host, input)? {
            let id = dev_id(&d);
            let Some(node) = id.strip_prefix("pipewire:") else {
                out.push(Device { id, name: d.to_string(), current: false });
                continue;
            };
            let default = if input { "input_default" } else { "output_default" };
            let real = if input {
                node.starts_with("alsa_input.") || node.starts_with("bluez_input.")
            } else {
                node.starts_with("alsa_output.") || node.starts_with("bluez_output.")
            };
            if node == default {
                out.insert(0, Device { id, name: "System default".into(), current: false });
            } else if real && !node.contains("_internal") {
                out.push(Device { id, name: d.to_string(), current: false });
            }
        }
        Ok(out)
    };
    Ok((pick(true)?, pick(false)?))
}

/// Streaming linear resampler that keeps its phase across buffers.
pub struct Resampler {
    step: f64,
    t: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Resampler { step: from as f64 / to as f64, t: 0.0, last: 0.0 }
    }
    pub fn process(&mut self, inp: &[f32], out: &mut Vec<f32>) {
        if inp.is_empty() {
            return;
        }
        // index 0 is the last sample of the previous buffer, index i is inp[i - 1]
        let last = self.last;
        let at = |i: usize| if i == 0 { last } else { inp[i - 1] };
        let len = inp.len() + 1;
        let mut t = self.t;
        while t + 1.0 < len as f64 {
            let j = t as usize;
            let f = (t - j as f64) as f32;
            let (a, b) = (at(j), at(j + 1));
            out.push(a + (b - a) * f);
            t += self.step;
        }
        self.t = t - (len - 1) as f64;
        self.last = inp[inp.len() - 1];
    }
}

/// One-shot resample of a whole buffer.
pub fn resample(pcm: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to {
        return pcm.to_vec();
    }
    let mut out = Vec::with_capacity(pcm.len() * to as usize / from as usize + 1);
    Resampler::new(from, to).process(pcm, &mut out);
    out
}

pub fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt()
}

/// A device stream on its own thread (cpal streams are not Send everywhere). Dropping the
/// handle stops the stream.
struct Running {
    id: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn run_stream<F>(name: &str, open: F) -> Result<Arc<AtomicBool>>
where
    F: FnOnce() -> Result<cpal::Stream> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let s = stop.clone();
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let opened = open().and_then(|st| {
            st.play()?;
            Ok(st)
        });
        match opened {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                while !s.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                drop(stream);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    })?;
    ready_rx.recv().context("audio thread died")??;
    Ok(stop)
}

/// Mono 16 kHz frames from whatever mic is current.
pub struct Frames {
    pub rx: mpsc::Receiver<Vec<f32>>,
    pub rate: u32,
}

pub struct Mic {
    tx: mpsc::Sender<Vec<f32>>,
    gate: Arc<AtomicBool>,
    running: Mutex<Option<Running>>,
}

impl Mic {
    pub fn new(gate: Arc<AtomicBool>) -> (Arc<Mic>, Frames) {
        let (tx, rx) = mpsc::channel();
        (Arc::new(Mic { tx, gate, running: Mutex::new(None) }), Frames { rx, rate: MIC_RATE })
    }

    pub fn current(&self) -> Option<String> {
        self.running.lock().unwrap().as_ref().map(|r| r.id.clone())
    }

    /// Open `device` (an id, a name substring, or None for the default), replacing the mic.
    pub fn open(&self, device: Option<&str>) -> Result<()> {
        let host = host();
        let dev = find(&host, device, true)?;
        let id = dev_id(&dev);
        let cfg = dev.default_input_config().context("mic config")?;
        let (rate, ch, fmt) = (cfg.sample_rate(), cfg.channels() as usize, cfg.sample_format());
        eprintln!("mic: {dev} at {rate} Hz, {ch} ch, {fmt:?}");
        let (tx, gate) = (self.tx.clone(), self.gate.clone());
        // stop the old stream first so two never feed the recognizer at once
        self.running.lock().unwrap().take();
        let stop = run_stream("parley-mic", move || {
            let cfg: cpal::StreamConfig = cfg.into();
            match fmt {
                SampleFormat::F32 => input::<f32>(&dev, cfg, ch, rate, tx, gate),
                SampleFormat::I16 => input::<i16>(&dev, cfg, ch, rate, tx, gate),
                f => Err(anyhow!("unsupported mic sample format {f:?}")),
            }
        })?;
        *self.running.lock().unwrap() = Some(Running { id, stop });
        Ok(())
    }
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
    rate: u32,
    tx: mpsc::Sender<Vec<f32>>,
    gate: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let mut rs = Resampler::new(rate, MIC_RATE);
    let mut mono = Vec::new();
    Ok(dev.build_input_stream(
        cfg,
        move |data: &[T], _: &_| {
            if !gate.load(Ordering::Relaxed) {
                return;
            }
            mono.clear();
            mono.extend(data.chunks(ch).map(|f| f.iter().map(|s| s.f()).sum::<f32>() / ch as f32));
            let mut out = Vec::with_capacity(mono.len() * MIC_RATE as usize / rate as usize + 1);
            rs.process(&mono, &mut out);
            let _ = tx.send(out);
        },
        |e| eprintln!("mic stream: {e}"),
        None,
    )?)
}

/// Speech waiting to be played, at 24 kHz, plus a level meter. Outlives device switches.
pub struct Player {
    buf: Mutex<VecDeque<f32>>,
    level: AtomicU32,
    running: Mutex<Option<Running>>,
}

impl Player {
    pub fn new() -> Arc<Player> {
        Arc::new(Player { buf: Mutex::new(VecDeque::new()), level: AtomicU32::new(0), running: Mutex::new(None) })
    }
    pub fn push(&self, pcm: &[f32], from_rate: u32) {
        let out = resample(pcm, from_rate, VOICE_RATE);
        self.buf.lock().unwrap().extend(out);
    }
    pub fn queued_secs(&self) -> f32 {
        self.buf.lock().unwrap().len() as f32 / VOICE_RATE as f32
    }
    pub fn clear(&self) {
        self.buf.lock().unwrap().clear();
        self.level.store(0f32.to_bits(), Ordering::Relaxed);
    }
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
    pub fn current(&self) -> Option<String> {
        self.running.lock().unwrap().as_ref().map(|r| r.id.clone())
    }

    /// Open `device`, replacing the speaker. Queued speech carries over.
    pub fn open(self: &Arc<Self>, device: Option<&str>) -> Result<()> {
        let host = host();
        let dev = find(&host, device, false)?;
        let id = dev_id(&dev);
        let cfg = dev.default_output_config().context("speaker config")?;
        let (rate, ch, fmt) = (cfg.sample_rate(), cfg.channels() as usize, cfg.sample_format());
        eprintln!("speaker: {dev} at {rate} Hz, {ch} ch, {fmt:?}");
        let p = self.clone();
        self.running.lock().unwrap().take();
        let stop = run_stream("parley-speaker", move || {
            let cfg: cpal::StreamConfig = cfg.into();
            match fmt {
                SampleFormat::F32 => output::<f32>(&dev, cfg, ch, rate, p, |v| v),
                SampleFormat::I16 => output::<i16>(&dev, cfg, ch, rate, p, |v| (v * 32767.0) as i16),
                f => Err(anyhow!("unsupported speaker sample format {f:?}")),
            }
        })?;
        *self.running.lock().unwrap() = Some(Running { id, stop });
        Ok(())
    }
}

fn output<T: SizedSample + Send + 'static>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    ch: usize,
    rate: u32,
    p: Arc<Player>,
    conv: fn(f32) -> T,
) -> Result<cpal::Stream> {
    let mut rs = Resampler::new(VOICE_RATE, rate);
    let mut ready: VecDeque<f32> = VecDeque::new();
    let mut src = Vec::new();
    let mut dst = Vec::new();
    Ok(dev.build_output_stream(
        cfg,
        move |out: &mut [T], _: &_| {
            let frames = out.len() / ch;
            {
                // pull just enough 24 kHz audio to cover this callback at the device rate
                let mut buf = p.buf.lock().unwrap();
                let need = frames.saturating_sub(ready.len());
                let take = (need as u64 * VOICE_RATE as u64 / rate as u64) as usize + 2;
                src.clear();
                let n = take.min(buf.len());
                src.extend(buf.drain(..n));
            }
            dst.clear();
            rs.process(&src, &mut dst);
            ready.extend(dst.iter().copied());
            let mut sq = 0f32;
            for f in out.chunks_mut(ch) {
                let v = ready.pop_front().unwrap_or(0.0);
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

/// Device control for the daemon protocol.
pub struct Control {
    pub mic: Option<Arc<Mic>>,
    pub player: Option<Arc<Player>>,
}

impl parley::daemon::Audio for Control {
    fn devices(&self) -> (Vec<Device>, Vec<Device>) {
        let (mut ins, mut outs) = list().unwrap_or_default();
        let cur_in = self.mic.as_ref().and_then(|m| m.current());
        let cur_out = self.player.as_ref().and_then(|p| p.current());
        for d in &mut ins {
            d.current = Some(&d.id) == cur_in.as_ref();
        }
        for d in &mut outs {
            d.current = Some(&d.id) == cur_out.as_ref();
        }
        (ins, outs)
    }
    fn set_input(&self, id: &str) -> Result<()> {
        self.mic.as_ref().ok_or_else(|| anyhow!("mic is disabled"))?.open(Some(id))
    }
    fn set_output(&self, id: &str) -> Result<()> {
        self.player.as_ref().ok_or_else(|| anyhow!("speaker is disabled"))?.open(Some(id))
    }
}

/// Test source: WAV files played at real-time pace in 20 ms frames, after `delay` seconds of
/// silence, with `gap` seconds of silence after each, then silence forever.
pub fn from_wavs(paths: Vec<std::path::PathBuf>, delay: f32, gap: f32, gate: Arc<AtomicBool>) -> Result<Frames> {
    let clips: Vec<Vec<f32>> = paths
        .iter()
        .map(|p| {
            let (pcm, r) = crate::wav::read(p).with_context(|| format!("read {}", p.display()))?;
            Ok(resample(&pcm, r, MIC_RATE))
        })
        .collect::<Result<_>>()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("parley-wav".into()).spawn(move || {
        let frame = MIC_RATE as usize / 50;
        let silence = vec![0f32; frame];
        let send = |f: &[f32]| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if gate.load(Ordering::Relaxed) { tx.send(f.to_vec()).is_ok() } else { true }
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
    Ok(Frames { rx, rate: MIC_RATE })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_resample_matches_one_shot() {
        let x: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
        let whole = resample(&x, 48000, 16000);
        let mut rs = Resampler::new(48000, 16000);
        let mut parts = Vec::new();
        for c in x.chunks(441) {
            rs.process(c, &mut parts);
        }
        assert_eq!(whole.len(), parts.len());
        assert!(whole.iter().zip(&parts).all(|(a, b)| (a - b).abs() < 1e-5));
        assert!((whole.len() as i64 - 1600).abs() <= 1);
    }
}
