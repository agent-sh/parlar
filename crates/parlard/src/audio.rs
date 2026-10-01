//! Device audio. The mic always delivers 16 kHz mono and the speaker always takes 24 kHz mono, so
//! switching devices (different rates, channel counts) never reaches the speech pipeline.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use parlar::proto::Device;

pub const MIC_RATE: u32 = 16000;

/// Mic chunks that may wait for the listener (a few seconds of audio); beyond that new audio is
/// dropped so a stalled recognizer cannot grow memory without limit.
const MIC_QUEUE: usize = 256;

/// Raised by a stream's error callback when its device changed or went away (a Bluetooth
/// profile switch replaces the device); a supervisor thread then reopens the stream.
static MIC_STALE: AtomicBool = AtomicBool::new(false);
static SPEAKER_STALE: AtomicBool = AtomicBool::new(false);

/// Errors that mean the stream is gone or must be rebuilt. Glitches such as an xrun do not.
fn needs_reopen(e: &cpal::Error) -> bool {
    use cpal::ErrorKind as K;
    matches!(e.kind(), K::DeviceNotAvailable | K::StreamInvalidated | K::DeviceChanged)
}

/// Reopen a stream after its device changed, once the devices have settled, and keep retrying
/// (1 s, 2 s, 4 s, up to 10 s apart) while no device opens. `reopen` returns None once its owner
/// is gone.
fn supervise(name: &'static str, stale: &'static AtomicBool, reopen: impl Fn() -> Option<Result<()>> + Send + 'static) {
    let _ = std::thread::Builder::new().name(format!("parlar-{name}-watch")).spawn(move || {
        let mut retry: Option<Duration> = None;
        loop {
            match retry {
                Some(wait) => std::thread::sleep(wait),
                None => {
                    std::thread::sleep(Duration::from_millis(500));
                    if !stale.swap(false, Ordering::SeqCst) {
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(700));
                    eprintln!("{name}: device changed, reopening");
                }
            }
            stale.store(false, Ordering::SeqCst);
            match reopen() {
                None => return,
                Some(Ok(())) => {
                    if retry.take().is_some() {
                        eprintln!("{name}: reopened");
                    }
                }
                Some(Err(e)) => {
                    let wait = retry.map_or(Duration::from_secs(1), |w| (w * 2).min(Duration::from_secs(10)));
                    eprintln!("{name}: reopen failed: {e:#}; retrying in {} s", wait.as_secs());
                    retry = Some(wait);
                }
            }
        }
    });
}
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
        return by_id.or_else(by_name).map(|i| devs[i].clone()).ok_or_else(|| anyhow!("no audio device matching {w}"));
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
/// handle stops the stream and waits until it is closed, so an old and a new stream never run
/// side by side.
struct Running {
    id: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn run_stream<F>(name: &str, id: String, open: F) -> Result<Running>
where
    F: FnOnce() -> Result<cpal::Stream> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let s = stop.clone();
    let thread = std::thread::Builder::new().name(name.into()).spawn(move || {
        let opened = open().and_then(|st| {
            st.play()?;
            Ok(st)
        });
        match opened {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                // Running::drop unparks this thread; park may also wake spuriously
                while !s.load(Ordering::SeqCst) {
                    std::thread::park();
                }
                drop(stream);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    })?;
    match ready_rx.recv().context("audio thread died").and_then(|r| r) {
        Ok(()) => Ok(Running { id, stop, thread: Some(thread) }),
        Err(e) => {
            let _ = thread.join();
            Err(e)
        }
    }
}

/// Mono 16 kHz frames from whatever mic is current.
pub struct Frames {
    pub rx: mpsc::Receiver<Vec<f32>>,
    pub rate: u32,
}

pub struct Mic {
    tx: mpsc::SyncSender<Vec<f32>>,
    gate: Arc<AtomicBool>,
    running: Mutex<Option<Running>>,
    want: Mutex<Option<String>>,
    /// The device picked last, open or not.
    chosen: Mutex<Option<String>>,
    /// Held across every open and close, so a reopen racing a stop cannot leave a stream running.
    switching: Mutex<()>,
}

impl Mic {
    pub fn new(gate: Arc<AtomicBool>) -> (Arc<Mic>, Frames) {
        let (tx, rx) = mpsc::sync_channel(MIC_QUEUE);
        let m = Arc::new(Mic {
            tx,
            gate,
            running: Mutex::new(None),
            want: Mutex::new(None),
            chosen: Mutex::new(None),
            switching: Mutex::new(()),
        });
        let weak = Arc::downgrade(&m);
        supervise("mic", &MIC_STALE, move || {
            let m = weak.upgrade()?;
            let want = m.want.lock().unwrap().clone();
            Some(m.open(want.as_deref()).or_else(|_| m.open(None)))
        });
        // the device stays closed while the conversation is off: the system mic indicator goes
        // out, and a Bluetooth headset can return to its high-quality playback mode
        let weak = Arc::downgrade(&m);
        let mut was = m.gate.load(Ordering::SeqCst);
        let _ = std::thread::Builder::new().name("parlar-mic-gate".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let Some(m) = weak.upgrade() else { return };
                let on = m.gate.load(Ordering::SeqCst);
                if !on {
                    // checked every tick, not only on the edge: nothing may hold the mic while off
                    let _switching = m.switching.lock().unwrap();
                    if m.running.lock().unwrap().take().is_some() {
                        eprintln!("mic closed");
                    }
                } else if !was {
                    let want = m.want.lock().unwrap().clone();
                    if let Err(e) = m.open(want.as_deref()).or_else(|_| m.open(None)) {
                        eprintln!("mic: {e:#}");
                    }
                }
                was = on;
            }
        });
        (m, Frames { rx, rate: MIC_RATE })
    }

    pub fn current(&self) -> Option<String> {
        self.running.lock().unwrap().as_ref().map(|r| r.id.clone()).or_else(|| self.chosen.lock().unwrap().clone())
    }

    /// Pick `device` (an id, a name substring, or None for the default), replacing the mic. The
    /// stream only runs while the conversation is on; otherwise the device is remembered and
    /// opened when it starts.
    pub fn open(&self, device: Option<&str>) -> Result<()> {
        let _switching = self.switching.lock().unwrap();
        let host = host();
        let dev = find(&host, device, true)?;
        let id = dev_id(&dev);
        if device.is_some() || self.want.lock().unwrap().is_none() {
            *self.want.lock().unwrap() = device.map(str::to_string);
        }
        *self.chosen.lock().unwrap() = Some(id.clone());
        // stop the old stream first so two never feed the recognizer at once
        self.running.lock().unwrap().take();
        if !self.gate.load(Ordering::SeqCst) {
            return Ok(());
        }
        let cfg = dev.default_input_config().context("mic config")?;
        let (rate, ch, fmt) = (cfg.sample_rate(), cfg.channels() as usize, cfg.sample_format());
        eprintln!("mic: {dev} at {rate} Hz, {ch} ch, {fmt:?}");
        let (tx, gate) = (self.tx.clone(), self.gate.clone());
        let running = run_stream("parlar-mic", id, move || {
            let cfg: cpal::StreamConfig = cfg.into();
            match fmt {
                SampleFormat::F32 => input::<f32>(&dev, cfg, ch, rate, tx, gate),
                SampleFormat::I16 => input::<i16>(&dev, cfg, ch, rate, tx, gate),
                f => Err(anyhow!("unsupported mic sample format {f:?}")),
            }
        })?;
        // the conversation may have stopped while the device opened
        if self.gate.load(Ordering::SeqCst) {
            *self.running.lock().unwrap() = Some(running);
        }
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
    tx: mpsc::SyncSender<Vec<f32>>,
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
            // a full queue means the listener is stuck; drop audio rather than block the device
            let _ = tx.try_send(out);
        },
        |e| {
            eprintln!("mic stream: {e}");
            if needs_reopen(&e) {
                MIC_STALE.store(true, Ordering::SeqCst);
            }
        },
        None,
    )?)
}

/// Speech waiting to be played, at 24 kHz, plus a level meter. Outlives device switches.
pub struct Player {
    buf: Mutex<VecDeque<f32>>,
    level: AtomicU32,
    running: Mutex<Option<Running>>,
    want: Mutex<Option<String>>,
    /// When the output callback last ran, in ms since `born`, to notice a speaker that stopped
    /// pulling audio.
    pulled: AtomicU64,
    born: Instant,
    /// What actually reached the speaker, at 16 kHz, for echo cancellation.
    pub far: crate::aec::Far,
}

impl Player {
    pub fn new() -> Arc<Player> {
        let p = Arc::new(Player {
            buf: Mutex::new(VecDeque::new()),
            level: AtomicU32::new(0),
            running: Mutex::new(None),
            want: Mutex::new(None),
            pulled: AtomicU64::new(0),
            born: Instant::now(),
            far: crate::aec::Far::default(),
        });
        let weak = Arc::downgrade(&p);
        supervise("speaker", &SPEAKER_STALE, move || {
            let p = weak.upgrade()?;
            let want = p.want.lock().unwrap().clone();
            Some(p.open(want.as_deref()).or_else(|_| p.open(None)))
        });
        p
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
    fn mark_pulled(&self) {
        self.pulled.store(self.born.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
    /// True when the speaker has not asked for audio for `limit` (suspended or gone).
    pub fn stalled(&self, limit: Duration) -> bool {
        let last = Duration::from_millis(self.pulled.load(Ordering::Relaxed));
        self.born.elapsed().saturating_sub(last) > limit
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
        if device.is_some() || self.want.lock().unwrap().is_none() {
            *self.want.lock().unwrap() = device.map(str::to_string);
        }
        self.running.lock().unwrap().take();
        let running = run_stream("parlar-speaker", id, move || {
            let cfg: cpal::StreamConfig = cfg.into();
            match fmt {
                SampleFormat::F32 => output::<f32>(&dev, cfg, ch, rate, p, |v| v),
                SampleFormat::I16 => output::<i16>(&dev, cfg, ch, rate, p, |v| (v * 32767.0) as i16),
                f => Err(anyhow!("unsupported speaker sample format {f:?}")),
            }
        })?;
        *self.running.lock().unwrap() = Some(running);
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
    let mut to_far = Resampler::new(rate, crate::aec::RATE);
    let mut ready: VecDeque<f32> = VecDeque::new();
    let mut src = Vec::new();
    let mut dst = Vec::new();
    let mut played = Vec::new();
    let mut far = Vec::new();
    Ok(dev.build_output_stream(
        cfg,
        move |out: &mut [T], _: &_| {
            p.mark_pulled();
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
            played.clear();
            for f in out.chunks_mut(ch) {
                let v = ready.pop_front().unwrap_or(0.0);
                sq += v * v;
                played.push(v);
                for s in f {
                    *s = conv(v);
                }
            }
            let rms = (sq / frames.max(1) as f32).sqrt();
            p.level.store((rms * 4.0).min(1.0).to_bits(), Ordering::Relaxed);
            far.clear();
            to_far.process(&played, &mut far);
            p.far.push(&far);
        },
        |e| {
            eprintln!("speaker stream: {e}");
            if needs_reopen(&e) {
                SPEAKER_STALE.store(true, Ordering::SeqCst);
            }
        },
        None,
    )?)
}

/// Device control for the daemon protocol.
pub struct Control {
    pub mic: Option<Arc<Mic>>,
    pub player: Option<Arc<Player>>,
}

/// The devices the person picked, kept across restarts in `$XDG_CONFIG_HOME/parlar/devices.json`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct Saved {
    pub input: Option<String>,
    pub output: Option<String>,
}

fn saved_path() -> std::path::PathBuf {
    parlar::dirs::config().join("devices.json")
}

impl Saved {
    pub fn load() -> Saved {
        std::fs::read_to_string(saved_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    fn store(&self) {
        let p = saved_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}

impl parlar::daemon::Audio for Control {
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
        self.mic.as_ref().ok_or_else(|| anyhow!("mic is disabled"))?.open(Some(id))?;
        let mut saved = Saved::load();
        saved.input = Some(id.to_string());
        saved.store();
        Ok(())
    }
    fn set_output(&self, id: &str) -> Result<()> {
        self.player.as_ref().ok_or_else(|| anyhow!("speaker is disabled"))?.open(Some(id))?;
        let mut saved = Saved::load();
        saved.output = Some(id.to_string());
        saved.store();
        Ok(())
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
    // a blocking send here only paces the test source
    let (tx, rx) = mpsc::sync_channel(MIC_QUEUE);
    std::thread::Builder::new().name("parlar-wav".into()).spawn(move || {
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
