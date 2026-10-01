//! Mic to finished user turns: streaming recognition, turn endpointing, filler cleanup, and
//! barge-in detection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::mpsc::UnboundedSender;

use crate::audio::{self, Frames};
use crate::vocab::Vocab;

pub enum Heard {
    Level(f32),
    /// The current turn so far, for live captions.
    Partial(String),
    /// The user is mid-phrase.
    Talking(bool),
    Turn { text: String, heard: Option<String> },
    /// The user started talking over the agent.
    BargeIn,
}

pub struct Config {
    /// Echo canceller fed by the speaker; without it only the text echo guard applies.
    pub aec: Option<crate::aec::Aec>,
    /// Open while a conversation is on and the mic is not muted. The models load when it opens
    /// and unload after it has been closed for `UNLOAD_AFTER`, so a stopped parlar stays small.
    pub gate: Arc<AtomicBool>,
    pub vocab: Arc<Mutex<Vocab>>,
}

/// What the agent is saying, so its own voice picked up by the mic is not mistaken for the user.
/// The text outlives playback by `ECHO_TAIL`: the end of a line is still in the device and the
/// recognizer when the speaker buffer drains.
#[derive(Clone, Default)]
pub struct Echo(Arc<Mutex<EchoText>>);

#[derive(Default)]
struct EchoText {
    text: String,
    /// Set when playback ended; the text counts as echo until then.
    until: Option<Instant>,
}

const ECHO_TAIL: Duration = Duration::from_millis(1500);

impl Echo {
    /// A line starts playing. The previous line's tail may still be coming in, so its text stays.
    pub fn start(&self, text: &str) {
        let mut e = self.0.lock().unwrap();
        if e.until.is_some_and(|u| Instant::now() < u) && !e.text.is_empty() {
            e.text.push(' ');
            e.text.push_str(text);
        } else {
            e.text = text.to_string();
        }
        e.until = None;
    }

    /// Playback ended.
    pub fn end(&self) {
        self.0.lock().unwrap().until = Some(Instant::now() + ECHO_TAIL);
    }

    /// What may still come back through the mic; empty when nothing does.
    pub fn current(&self) -> String {
        let mut e = self.0.lock().unwrap();
        if e.until.is_some_and(|u| Instant::now() >= u) {
            e.text.clear();
            e.until = None;
        }
        e.text.clone()
    }
}

/// How long the conversation may be stopped before the speech models are unloaded.
pub const UNLOAD_AFTER: Duration = Duration::from_secs(120);

/// The models the listener loads while a conversation is on.
struct Models {
    /// Voice detector that gates the recognizer; without it the recognizer sees everything.
    vad: Option<crate::vad::Vad>,
    /// End-of-turn audio model; without it the word rule alone decides.
    turn: Option<crate::turn::SmartTurn>,
    /// The speech recognizer, run on the turn so far at each pause.
    final_stt: Option<crate::stt::Tdt>,
}

impl Models {
    fn load() -> Result<Models> {
        use crate::models;
        let t0 = Instant::now();
        let ort = models::ort_lib();
        let soft = |what: &str, e: anyhow::Error| eprintln!("{what} unavailable: {e:#}");
        let vad = crate::vad::Vad::load(&models::vad_model(), &ort).map_err(|e| soft("voice detector", e)).ok();
        let turn = crate::turn::SmartTurn::load(&models::turn_model(), &ort).map_err(|e| soft("end-of-turn model", e)).ok();
        let final_stt = crate::stt::Tdt::load(&models::final_dir(), &ort, 4).map_err(|e| soft("final-transcript model", e)).ok();
        eprintln!("speech models loaded in {:.1} s", t0.elapsed().as_secs_f32());
        Ok(Models { vad, turn, final_stt })
    }
}

enum Exit {
    /// Stopped long enough to unload the models.
    Idle,
    /// The mic source is gone.
    Ended,
}

/// Mic audio missing for longer than this (the gate was closed, or the queue overflowed) means
/// the echo canceller's far end no longer lines up with the mic.
const GAP: Duration = Duration::from_millis(200);

pub fn spawn(frames: Frames, mut cfg: Config, agent_speaking: Arc<AtomicBool>, echo: Echo, tx: UnboundedSender<Heard>) -> Result<()> {
    std::thread::Builder::new().name("parlar-listen".into()).spawn(move || {
        let e = loop {
            // nothing loads until someone starts a conversation
            while !cfg.gate.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
            }
            let mut m = match Models::load() {
                Ok(m) if m.final_stt.is_some() => m,
                result => {
                    // missing models are a setup problem, not a crash: stay up, say why, and try
                    // again the next time a conversation starts
                    let why = result.err().map(|e| format!("{e:#}")).unwrap_or_else(|| "no speech recognition model".into());
                    eprintln!("cannot listen: {why}; run parlard fetch or /parlar:setup");
                    let _ = tx.send(Heard::Level(0.0));
                    while cfg.gate.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    continue;
                }
            };
            match run(&mut m, &frames, &mut cfg, &agent_speaking, &echo, &tx) {
                Ok(Exit::Idle) => {
                    drop(m);
                    crate::models::release_memory();
                    eprintln!("conversation stopped for {} s; speech models unloaded", UNLOAD_AFTER.as_secs());
                }
                Ok(Exit::Ended) => break anyhow!("mic audio ended"),
                Err(e) => break e,
            }
        };
        // a daemon that went deaf looks alive; exit so the service manager restarts it
        eprintln!("listener stopped: {e:#}; exiting");
        let _ = tx.send(Heard::Level(0.0));
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_file(parlar::client::socket_path());
        std::process::exit(1);
    })?;
    Ok(())
}

/// Continuous speech this long while the agent talks is the user cutting in.
const BARGE_AFTER: Duration = Duration::from_millis(500);
/// Quiet this long inside a turn is a pause: the turn so far is transcribed.
const PAUSE: Duration = Duration::from_millis(300);
/// Speech probability that counts as voice.
const VOICED: f32 = 0.5;

/// One stretch of the user's speech, as the endpointer sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub id: u64,
    pub text: String,
    pub start: f32,
    pub duration: f32,
    pub complete: bool,
    pub text_changed: bool,
    pub latency_ms: u32,
}

fn line(text: String, complete: bool) -> Line {
    Line { id: 0, text, start: 0.0, duration: 0.0, complete, text_changed: true, latency_ms: 0 }
}

/// The voice detector marks speech and pauses; the recognizer runs only at a pause, on the turn
/// so far. Its text drives the pause rules, the captions and the echo check, and becomes the
/// turn once the endpointer releases it. Nothing transcribes while nobody talks.
fn run(
    m: &mut Models,
    frames: &Frames,
    cfg: &mut Config,
    agent_speaking: &AtomicBool,
    echo: &Echo,
    tx: &UnboundedSender<Heard>,
) -> Result<Exit> {
    let (vad, turn, stt) = (&mut m.vad, &mut m.turn, &mut m.final_stt);
    if stt.is_none() {
        anyhow::bail!("no speech recognition model in {}", crate::models::final_dir().display());
    }
    let mut closed_since: Option<Instant> = None;
    let mut ep = Endpointer::default();
    // one line per turn, in the shape the endpointer reads
    let mut lines: Vec<Line> = Vec::new();
    let mut vocab = Vocab::default();
    let mut vocab_seen: Option<u64> = None;
    let mut last_level = Instant::now();
    let mut last_rx = Instant::now();
    let mut peak = 0f32;
    // the turn's audio, for the recognizer, the end-of-turn model and dumps
    let mut recent = Recent::new(frames.rate as usize * 60);
    // audio from just before speech started, so the first word is not clipped
    let mut preroll = Recent::new(frames.rate as usize / 2);
    let dump = std::env::var_os("PARLAR_DUMP_TURNS").map(std::path::PathBuf::from);
    let mut in_turn = false;
    let mut paused = false;
    let mut last_voice = Instant::now();
    let mut voiced_since: Option<Instant> = None;
    let mut barged = false;
    loop {
        if cfg.gate.load(Ordering::SeqCst) {
            closed_since = None;
        } else if closed_since.get_or_insert_with(Instant::now).elapsed() > UNLOAD_AFTER && !in_turn {
            return Ok(Exit::Idle);
        }
        let mut changed = false;
        match frames.rx.recv_timeout(Duration::from_millis(30)) {
            Ok(mut f) => {
                while let Ok(more) = frames.rx.try_recv() {
                    f.extend(more);
                }
                let got = Duration::from_secs_f64(f.len() as f64 / frames.rate as f64);
                if last_rx.elapsed().saturating_sub(got) > GAP
                    && let Some(a) = cfg.aec.as_mut()
                {
                    a.resume();
                }
                last_rx = Instant::now();
                let f = match cfg.aec.as_mut() {
                    Some(a) => a.process(&f),
                    None => f,
                };
                peak = peak.max(audio::rms(&f));
                let voiced = match vad.as_mut().map(|v| v.push(&f)) {
                    Some(Ok(p)) => p.iter().any(|&x| x >= VOICED),
                    Some(Err(e)) => {
                        eprintln!("voice detector failed, falling back to a level threshold: {e:#}");
                        *vad = None;
                        audio::rms(&f) > 0.02
                    }
                    None => audio::rms(&f) > 0.02,
                };
                let now = Instant::now();
                if voiced {
                    last_voice = now;
                    voiced_since.get_or_insert(now);
                    if !in_turn {
                        in_turn = true;
                        recent.buf.clear();
                        recent.buf.extend(preroll.buf.drain(..));
                        lines.push(line(String::new(), false));
                        changed = true;
                    } else if paused {
                        paused = false;
                        if let Some(l) = lines.last_mut() {
                            l.complete = false;
                        }
                        changed = true;
                    }
                } else {
                    voiced_since = None;
                }
                if in_turn {
                    recent.push(&f);
                } else {
                    preroll.push(&f);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(Exit::Ended),
        }
        let now = Instant::now();
        let speaking = agent_speaking.load(Ordering::SeqCst);
        if !speaking {
            barged = false;
        } else if !barged && voiced_since.is_some_and(|t| now.duration_since(t) >= BARGE_AFTER) {
            // echo cancellation keeps the agent's own voice out; the transcript at the next pause is
            // still checked against what it said
            barged = true;
            let _ = tx.send(Heard::BargeIn);
        }
        if last_level.elapsed() >= Duration::from_millis(33) {
            let _ = tx.send(Heard::Level((peak * 6.0).min(1.0)));
            peak = 0.0;
            last_level = Instant::now();
        }
        let fresh = {
            let v = cfg.vocab.lock().unwrap();
            (Some(v.version) != vocab_seen).then(|| v.clone())
        };
        if let Some(v) = fresh {
            vocab_seen = Some(v.version);
            vocab = v;
        }
        if in_turn && !paused && now.duration_since(last_voice) >= PAUSE {
            paused = true;
            let audio: Vec<f32> = recent.buf.iter().copied().collect();
            let t0 = Instant::now();
            let text = stt.as_mut().expect("checked above").transcribe(&audio).unwrap_or_else(|e| {
                eprintln!("recognizer: {e:#}");
                String::new()
            });
            eprintln!("hearing ({:.0} ms): {text}", t0.elapsed().as_secs_f32() * 1e3);
            if let Some(l) = lines.last_mut() {
                l.text = text;
                l.complete = true;
            }
            changed = true;
        }
        if in_turn && !paused {
            // still talking: the open line must not look settled
            ep.touch(now);
        }
        let fix = |ev: Heard| match ev {
            Heard::Turn { text, heard } => {
                let joined = vocab.join_dots(&text);
                let heard = heard.or_else(|| (joined != text).then(|| text.clone()));
                Heard::Turn { text: joined, heard }
            }
            e => e,
        };
        if ep.wants_score(now) {
            let p = match turn.as_mut() {
                Some(m) => m.complete(&recent.speech()).unwrap_or_else(|e| {
                    eprintln!("turn model: {e:#}");
                    1.0
                }),
                None => 1.0,
            };
            eprintln!("turn score {p:.2}: {}", ep.text);
            ep.set_score(p);
        }
        let events = if changed { ep.update(&lines, now, speaking, &echo.current()) } else { ep.tick(now) };
        for ev in events {
            let ev = fix(ev);
            if matches!(ev, Heard::Turn { .. }) {
                dump_turn(dump.as_deref(), &ev, &recent, frames.rate);
                in_turn = false;
                paused = false;
                recent.buf.clear();
            }
            let _ = tx.send(ev);
        }
        // a turn whose words were all echo or nothing: let the next one start fresh
        if in_turn && paused && ep.text.is_empty() {
            in_turn = false;
            paused = false;
            recent.buf.clear();
        }
    }
}

/// With PARLAR_DUMP_TURNS set, keep each finished turn's recent audio and its text, for comparing
/// recognizers on real speech. Off unless asked for.
fn dump_turn(dir: Option<&std::path::Path>, ev: &Heard, recent: &Recent, rate: u32) {
    let (Some(dir), Heard::Turn { text, heard }) = (dir, ev) else { return };
    let _ = std::fs::create_dir_all(dir);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let audio: Vec<f32> = recent.buf.iter().copied().collect();
    let _ = crate::wav::write(&dir.join(format!("{stamp}.wav")), &audio, rate);
    let _ = std::fs::write(dir.join(format!("{stamp}.txt")), format!("{}\n{}\n", text, heard.clone().unwrap_or_default()));
}

/// The last few seconds of mic audio, for the end-of-turn model.
struct Recent {
    buf: std::collections::VecDeque<f32>,
    cap: usize,
}

impl Recent {
    fn new(cap: usize) -> Self {
        Recent { buf: std::collections::VecDeque::with_capacity(cap), cap }
    }

    fn push(&mut self, f: &[f32]) {
        self.buf.extend(f);
        let over = self.buf.len().saturating_sub(self.cap);
        self.buf.drain(..over);
    }

    /// Recent audio with the trailing silence cut to 200 ms, as the model saw in training.
    fn speech(&self) -> Vec<f32> {
        let v: Vec<f32> = self.buf.iter().copied().collect();
        let block = 320;
        let blocks: Vec<f32> = v.chunks(block).map(audio::rms).collect();
        let floor = blocks.iter().copied().fold(f32::MAX, f32::min).max(1e-4);
        let loud = (floor * 4.0).max(0.01);
        let last = blocks.iter().rposition(|&e| e > loud).unwrap_or(blocks.len().saturating_sub(1));
        let end = ((last + 1) * block + 3200).min(v.len());
        v[..end].to_vec()
    }
}

#[derive(Default)]
struct Endpointer {
    /// Lines before this index belong to turns already delivered or dropped as echo.
    done: usize,
    /// Line count at the last update.
    seen: usize,
    /// A line that was still open when its words were delivered (they had stopped changing):
    /// its index and how many of its words went out. Later words in it make the next turn.
    sent: Option<(usize, usize)>,
    /// Word count of the last line at the last update, when that line was still open.
    open_words: Option<usize>,
    text: String,
    complete: bool,
    changed: Option<Instant>,
    talking: bool,
    barged: bool,
    /// End-of-turn probability from the audio model for the current text, once scored.
    score: Option<f32>,
}

impl Endpointer {
    fn update(&mut self, lines: &[Line], now: Instant, agent_speaking: bool, echo: &str) -> Vec<Heard> {
        let mut out = Vec::new();
        self.seen = lines.len();
        // `echo` is what the agent is saying, or just finished saying: the mic heard the agent,
        // so drop those lines once they settle
        while self.done < lines.len() && lines[self.done].complete && is_echo(&lines[self.done].text, echo) {
            self.done += 1;
        }
        if self.sent.is_some_and(|(at, _)| at < self.done) {
            self.sent = None;
        }
        let start = self.done.min(lines.len());
        let open = &lines[start..];
        let fresh: Vec<String> = open
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let skip = match self.sent {
                    Some((at, n)) if at == start + i => n,
                    _ => 0,
                };
                l.text.split_whitespace().skip(skip).collect::<Vec<_>>().join(" ")
            })
            .collect();
        let text = fresh.iter().filter(|t| !t.is_empty()).cloned().collect::<Vec<_>>().join(" ");
        let complete = open.iter().all(|l| l.complete);
        // an open line whose words all went out already is noise holding the line, not talking
        let held = self.sent.is_some_and(|(at, _)| at + 1 == lines.len()) && fresh.last().is_some_and(|t| t.is_empty());
        let talking = open.last().is_some_and(|l| !l.complete) && !held;
        self.open_words = lines.last().filter(|l| !l.complete).map(|l| words(&l.text));
        if !text.is_empty() && is_echo(&text, echo) {
            return out;
        }
        if talking != self.talking {
            self.talking = talking;
            out.push(Heard::Talking(talking));
        }
        if text != self.text {
            self.text = text.clone();
            self.changed = Some(now);
            self.score = None;
            if !text.is_empty() {
                out.push(Heard::Partial(text.clone()));
                if agent_speaking && !self.barged && words(&text) >= 2 {
                    self.barged = true;
                    out.push(Heard::BargeIn);
                }
            }
        }
        self.complete = complete;
        if !agent_speaking {
            self.barged = false;
        }
        out.extend(self.release(now));
        out
    }

    /// The person is still talking: an open line with settled words is not stale yet.
    fn touch(&mut self, now: Instant) {
        if self.changed.is_some() {
            self.changed = Some(now);
        }
    }

    fn tick(&mut self, now: Instant) -> Vec<Heard> {
        self.release(now)
    }

    /// True when the phrase is complete, has been quiet a moment, and has not been scored yet.
    fn wants_score(&self, now: Instant) -> bool {
        self.score.is_none()
            && !self.text.is_empty()
            && self.changed.is_some_and(|c| now.duration_since(c) >= SCORE_AFTER)
    }

    fn set_score(&mut self, p: f32) {
        self.score = Some(p);
    }

    fn release(&mut self, now: Instant) -> Vec<Heard> {
        let Some(changed) = self.changed else { return vec![] };
        if self.text.is_empty() {
            return vec![];
        }
        let quiet = now.duration_since(changed);
        let hold = hold(&self.text, self.score);
        // background noise can keep the recognizer's voice detector open, so a line whose words
        // stopped changing counts as finished after a longer wait
        if !self.complete && quiet < hold + STALE_LINE {
            return vec![];
        }
        if quiet < hold {
            return vec![];
        }
        let heard = std::mem::take(&mut self.text);
        match self.open_words.filter(|_| !self.complete) {
            // the last line is still open: the next turn is whatever it adds after these words
            Some(n) if self.seen > 0 => {
                self.done = self.seen - 1;
                self.sent = Some((self.done, n));
            }
            // every open line was complete at the last update, so the next turn starts after them
            _ => {
                self.done = self.seen;
                self.sent = None;
            }
        }
        self.reset();
        let text = clean(&heard);
        if text.is_empty() {
            return vec![];
        }
        let heard = (text != heard).then_some(heard);
        vec![Heard::Turn { text, heard }]
    }

    fn reset(&mut self) {
        self.text.clear();
        self.complete = false;
        self.changed = None;
        self.score = None;
    }
}

const HOLD_TAIL: &[&str] = &[
    "and", "so", "but", "or", "because", "um", "uh", "like", "the", "a", "an", "to", "of", "with",
    "then", "wait", "if", "that", "is", "maybe", "also", "seems", "think", "want", "need", "it",
    "this", "for", "in", "on", "my", "your", "we", "i", "you", "well", "okay", "ok", "hmm",
];

/// Extra wait before an open line whose words stopped changing is treated as finished.
const STALE_LINE: Duration = Duration::from_millis(1500);

/// Quiet time before the audio model is asked about the phrase.
const SCORE_AFTER: Duration = Duration::from_millis(200);

/// How long a complete phrase must sit before the turn is handed over. The words decide first:
/// a trailing "and", "so" or "um" holds whatever the audio model says, because a speaker can
/// trail off on a falling pitch. Otherwise a low end-of-turn score reads as a thinking pause.
fn hold(text: &str, score: Option<f32>) -> Duration {
    let t = text.trim_end();
    let words = t.split_whitespace().count();
    if t.ends_with('?') && words > 2 {
        return Duration::from_millis(500);
    }
    let last = t
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if t.ends_with(',') || t.ends_with("...") || HOLD_TAIL.contains(&last.as_str()) || last == "think" {
        return Duration::from_millis(3000);
    }
    // one or two words is usually the start of a thought, not the whole of it
    if words <= 2 {
        return Duration::from_millis(2000);
    }
    match score {
        Some(p) if p < 0.5 => Duration::from_millis(2500),
        _ => Duration::from_millis(900),
    }
}

const FILLERS: &[&str] = &["um", "uh", "uhm", "umm", "erm", "er", "hmm", "hm", "mm", "mhm", "ah"];

/// Drop filler words. Self-repairs stay; the model resolves them with the heard text beside.
pub fn clean(raw: &str) -> String {
    let kept: Vec<&str> = raw
        .split_whitespace()
        .filter(|w| {
            let bare = w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            !FILLERS.contains(&bare.as_str())
        })
        .collect();
    let mut s = kept.join(" ");
    s = s.trim_start_matches([',', ' ']).to_string();
    if let Some(first) = s.chars().next() {
        let rest = &s[first.len_utf8()..];
        s = first.to_uppercase().collect::<String>() + rest;
    }
    s
}

fn words(t: &str) -> usize {
    t.split_whitespace().count()
}

fn norm_words(t: &str) -> Vec<String> {
    t.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// True when most of what was heard is in what the agent is saying.
fn is_echo(heard: &str, said: &str) -> bool {
    let h = norm_words(heard);
    if h.is_empty() || said.is_empty() {
        return false;
    }
    let s = norm_words(said);
    let hit = h.iter().filter(|w| s.contains(w)).count();
    hit * 10 >= h.len() * 6
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, complete: bool) -> Line {
        Line {
            id: 0,
            text: text.into(),
            start: 0.0,
            duration: 0.0,
            complete,
            text_changed: true,
            latency_ms: 0,
        }
    }

    #[test]
    fn fillers_are_dropped_and_heard_kept() {
        assert_eq!(clean("um, open the uh router file"), "Open the router file");
        assert_eq!(clean("Hmm."), "");
    }

    #[test]
    fn trailing_conjunction_holds_longer() {
        assert!(hold("open the router and", None) > hold("open the router", None));
        assert!(hold("is it done yet?", None) < hold("open the router", None));
        assert!(hold("so", None) >= Duration::from_secs(2));
        assert!(hold("seems that", None) >= Duration::from_secs(2));
        assert!(hold("let me think", None) > hold("open the router", None));
        // the audio model can extend a pause but never cut a trailing conjunction short
        assert!(hold("open the router", Some(0.2)) > hold("open the router", Some(0.9)));
        assert_eq!(hold("open the router and", Some(0.99)), hold("open the router and", None));
    }

    #[test]
    fn turn_waits_for_completion_and_hold() {
        let mut ep = Endpointer::default();
        let t0 = Instant::now();
        let ev = ep.update(&[line("open the router", false)], t0, false, "");
        assert!(ev.iter().all(|e| !matches!(e, Heard::Turn { .. })));
        let ev = ep.update(&[line("open the router file", true)], t0 + Duration::from_millis(300), false, "");
        assert!(ev.iter().all(|e| !matches!(e, Heard::Turn { .. })));
        // not yet: a complete phrase still waits its hold
        assert!(ep.tick(t0 + Duration::from_millis(900)).is_empty());
        let ev = ep.tick(t0 + Duration::from_millis(1300));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Open the router file"));
    }

    #[test]
    fn open_line_with_settled_words_is_released() {
        let mut ep = Endpointer::default();
        let t0 = Instant::now();
        ep.update(&[line("check the training run", false)], t0, false, "");
        assert!(ep.tick(t0 + Duration::from_millis(1500)).is_empty());
        let ev = ep.tick(t0 + Duration::from_millis(2600));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Check the training run"));
    }

    #[test]
    fn continuation_after_pause_merges() {
        let mut ep = Endpointer::default();
        let t0 = Instant::now();
        ep.update(&[line("open the router config and", true)], t0, false, "");
        // still holding (trailing "and") when the next phrase starts
        let ev = ep.update(
            &[line("open the router config and", true), line("the tests", false)],
            t0 + Duration::from_millis(1000),
            false,
            "",
        );
        assert!(ev.iter().all(|e| !matches!(e, Heard::Turn { .. })));
        ep.update(
            &[line("open the router config and", true), line("the tests", true)],
            t0 + Duration::from_millis(1400),
            false,
            "",
        );
        let ev = ep.tick(t0 + Duration::from_millis(2400));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Open the router config and the tests"));
    }

    #[test]
    fn echo_of_agent_is_ignored_but_real_speech_barges_in() {
        let mut ep = Endpointer::default();
        let t0 = Instant::now();
        let said = "I'll remove the copy in app.ts and run the router tests.";
        let ev = ep.update(&[line("remove the copy in app", false)], t0, true, said);
        assert!(ev.is_empty());
        let ev = ep.update(&[line("remove the copy in app", true), line("wait stop that", false)], t0, true, said);
        assert!(ev.iter().any(|e| matches!(e, Heard::BargeIn)));
        assert!(ev.iter().any(|e| matches!(e, Heard::Partial(t) if t == "wait stop that")));
    }

    #[test]
    fn words_after_a_stale_release_make_the_next_turn() {
        let mut ep = Endpointer::default();
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        ep.update(&[line("check the training run", false)], t0, false, "");
        let ev = ep.tick(ms(2600));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Check the training run"));
        // the same line is still open and keeps growing
        let ev = ep.update(&[line("check the training run and the eval logs", false)], ms(3000), false, "");
        assert!(ev.iter().any(|e| matches!(e, Heard::Partial(t) if t == "and the eval logs")));
        ep.update(&[line("check the training run and the eval logs", true)], ms(3200), false, "");
        let ev = ep.tick(ms(4300));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "And the eval logs"));
        // the line is done with; the next one starts clean
        ep.update(&[line("check the training run and the eval logs", true), line("open the plots", true)], ms(5000), false, "");
        let ev = ep.tick(ms(6000));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Open the plots"));
    }

    #[test]
    fn echo_tail_outlives_playback() {
        let e = Echo::default();
        e.start("first line");
        assert_eq!(e.current(), "first line");
        e.end();
        assert_eq!(e.current(), "first line", "kept while the tail plays out");
        e.start("second line");
        assert_eq!(e.current(), "first line second line");
        e.0.lock().unwrap().until = Some(Instant::now());
        assert_eq!(e.current(), "");
    }

}

#[cfg(test)]
mod release_tests {
    use super::*;

    #[test]
    fn turn_after_clock_release_is_not_lost() {
        let l = |t: &str, c| Line { id: 0, text: t.into(), start: 0.0, duration: 0.0, complete: c, text_changed: true, latency_ms: 0 };
        let mut ep = Endpointer::default();
        let t0 = Instant::now();
        ep.update(&[l("first thing", true)], t0, false, "");
        assert!(ep.tick(t0 + Duration::from_secs(1)).is_empty(), "two words wait longer");
        assert_eq!(ep.tick(t0 + Duration::from_secs(3)).len(), 1);
        ep.update(&[l("first thing", true), l("second thing", true)], t0 + Duration::from_secs(4), false, "");
        let ev = ep.tick(t0 + Duration::from_secs(7));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Second thing"));
    }
}
