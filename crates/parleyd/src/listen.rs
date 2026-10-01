//! Mic to finished user turns: streaming recognition, turn endpointing, filler cleanup, and
//! barge-in detection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use parley_moonshine::{ARCH_SMALL_STREAMING, Stream, Transcriber};
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
    pub every_ms: usize,
    /// Voice detector that gates the recognizer; without it the recognizer sees everything.
    pub vad: Option<crate::vad::Vad>,
    /// Final-transcript model run on each finished turn; the streaming recognizer's text is used
    /// only for live captions and turn detection when it is present.
    pub final_stt: Option<crate::stt::Tdt>,
    /// Echo canceller fed by the speaker; without it only the text echo guard applies.
    pub aec: Option<crate::aec::Aec>,
    /// End-of-turn audio model; without it the word rule alone decides.
    pub turn: Option<crate::turn::SmartTurn>,
    pub partials: bool,
    /// Extra biasing terms from the command line, kept alongside the repo vocabulary.
    pub keyterms: Option<String>,
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

/// Mic audio missing for longer than this (the gate was closed, or the queue overflowed) means
/// the echo canceller's far end no longer lines up with the mic.
const GAP: Duration = Duration::from_millis(200);

pub fn spawn(frames: Frames, cfg: Config, agent_speaking: Arc<AtomicBool>, echo: Echo, tx: UnboundedSender<Heard>) -> Result<()> {
    let opts: Vec<(&str, &str)> = if cfg.partials { vec![] } else { vec![("decode_incomplete_lines", "false")] };
    let t = Transcriber::load(&crate::models::stt_dir(), ARCH_SMALL_STREAMING, &opts)?;
    std::thread::Builder::new().name("parley-listen".into()).spawn(move || {
        let e = match run(t, frames, cfg, agent_speaking, echo, &tx) {
            Ok(()) => anyhow!("mic audio ended"),
            Err(e) => e,
        };
        // a daemon that went deaf looks alive; exit so the service manager restarts it
        eprintln!("listener stopped: {e:#}; exiting");
        let _ = tx.send(Heard::Level(0.0));
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_file(parley::client::socket_path());
        std::process::exit(1);
    })?;
    Ok(())
}

fn open_stream(t: &Transcriber) -> Result<Stream<'_>> {
    let mut s = t.stream()?;
    s.start()?;
    Ok(s)
}

fn recognize(s: &mut Stream<'_>, audio: &[f32], rate: u32) -> Result<Vec<parley_moonshine::Line>> {
    if !audio.is_empty() {
        s.add_audio(audio, rate as i32)?;
    }
    s.transcribe()
}

fn run(
    t: Transcriber,
    frames: Frames,
    mut cfg: Config,
    agent_speaking: Arc<AtomicBool>,
    echo: Echo,
    tx: &UnboundedSender<Heard>,
) -> Result<()> {
    let mut s = open_stream(&t)?;
    let chunk = frames.rate as usize * cfg.every_ms / 1000;
    let mut feed = Feed::new(chunk, frames.rate as usize / 2);
    let mut ep = Endpointer::default();
    let mut vocab = Vocab::default();
    // None so the command-line keyterms apply before any repo vocabulary arrives
    let mut vocab_seen: Option<u64> = None;
    let mut last_level = Instant::now();
    let mut last_rx = Instant::now();
    let mut peak = 0f32;
    // the end-of-turn model reads the last 8 s; turn dumps for evaluation keep up to 30 s
    let mut recent = Recent::new(frames.rate as usize * 60);
    let dump = std::env::var_os("PARLEY_DUMP_TURNS").map(std::path::PathBuf::from);
    loop {
        let mut ready = None;
        match frames.rx.recv_timeout(Duration::from_millis(30)) {
            Ok(mut f) => {
                // whatever queued up meanwhile goes through in one piece
                while let Ok(more) = frames.rx.try_recv() {
                    f.extend(more);
                }
                let got = Duration::from_secs_f64(f.len() as f64 / frames.rate as f64);
                if last_rx.elapsed().saturating_sub(got) > GAP
                    && let Some(a) = cfg.aec.as_mut() {
                        a.resume();
                    }
                last_rx = Instant::now();
                let f = match cfg.aec.as_mut() {
                    Some(a) => a.process(&f),
                    None => f,
                };
                peak = peak.max(audio::rms(&f));
                recent.push(&f);
                let probs = match cfg.vad.as_mut().map(|v| v.push(&f)) {
                    Some(Ok(p)) => Some(p),
                    Some(Err(e)) => {
                        eprintln!("voice detector failed, the recognizer now runs on all audio: {e:#}");
                        cfg.vad = None;
                        None
                    }
                    None => None,
                };
                ready = feed.push(&f, probs.as_deref());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
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
            let first = vocab_seen.replace(v.version).is_none();
            vocab = v;
            let mut terms = vocab.keyterms();
            if let Some(k) = &cfg.keyterms {
                terms = if terms.is_empty() { k.clone() } else { format!("{k},{terms}") };
            }
            if !(first && terms.is_empty())
                && let Err(e) = t.set_keyterms(&terms) {
                    eprintln!("keyterms: {e:#}");
                }
        }
        let fix = |ev: Heard| match ev {
            Heard::Turn { text, heard } => {
                let joined = vocab.join_dots(&text);
                let heard = heard.or_else(|| (joined != text).then(|| text.clone()));
                Heard::Turn { text: joined, heard }
            }
            e => e,
        };
        if ep.wants_score(Instant::now()) {
            let p = match cfg.turn.as_mut() {
                Some(m) => m.complete(&recent.speech()).unwrap_or_else(|e| {
                    eprintln!("turn model: {e:#}");
                    1.0
                }),
                None => 1.0,
            };
            eprintln!("turn score {p:.2}: {}", ep.text);
            ep.set_score(p);
        }
        let Some(audio) = ready else {
            for ev in ep.tick(Instant::now()) {
                let ev = fix(finalize(cfg.final_stt.as_mut(), &recent, ev));
                dump_turn(dump.as_deref(), &ev, &recent, frames.rate);
                if matches!(ev, Heard::Turn { .. }) {
                    // the next turn's audio starts here, for the end-of-turn model and dumps
                    recent.buf.clear();
                }
                let _ = tx.send(ev);
            }
            continue;
        };
        let lines = match recognize(&mut s, &audio, frames.rate) {
            Ok(l) => l,
            Err(e) => {
                // a fresh stream starts its lines from zero, so the turn state starts over too
                eprintln!("recognizer: {e:#}; starting a new stream");
                s = open_stream(&t).context("new recognizer stream")?;
                if ep.talking {
                    let _ = tx.send(Heard::Talking(false));
                }
                ep = Endpointer::default();
                recognize(&mut s, &audio, frames.rate).context("recognizer failed on a new stream")?
            }
        };
        let speaking = agent_speaking.load(Ordering::SeqCst);
        let said = echo.current();
        for ev in ep.update(&lines, Instant::now(), speaking, &said) {
            let ev = fix(finalize(cfg.final_stt.as_mut(), &recent, ev));
            dump_turn(dump.as_deref(), &ev, &recent, frames.rate);
            if matches!(ev, Heard::Turn { .. }) {
                recent.buf.clear();
            }
            let _ = tx.send(ev);
        }
    }
}

/// Re-transcribe a finished turn with the final-transcript model. The streaming text stays only
/// if the model fails or hears nothing.
fn finalize(model: Option<&mut crate::stt::Tdt>, recent: &Recent, ev: Heard) -> Heard {
    let (Some(model), Heard::Turn { text, heard }) = (model, &ev) else { return ev };
    let audio: Vec<f32> = recent.buf.iter().copied().collect();
    let t0 = Instant::now();
    match model.transcribe(&audio) {
        Ok(raw) if !raw.trim().is_empty() => {
            let cleaned = clean(&raw);
            eprintln!("final ({:.0} ms): {cleaned}", t0.elapsed().as_secs_f32() * 1e3);
            let heard = (cleaned != raw).then_some(raw);
            Heard::Turn { text: cleaned, heard }
        }
        Ok(_) => Heard::Turn { text: text.clone(), heard: heard.clone() },
        Err(e) => {
            eprintln!("final transcript failed, keeping the streaming text: {e:#}");
            ev
        }
    }
}

/// With PARLEY_DUMP_TURNS set, keep each finished turn's recent audio and its text, for comparing
/// recognizers on real speech. Off unless asked for.
fn dump_turn(dir: Option<&std::path::Path>, ev: &Heard, recent: &Recent, rate: u32) {
    let (Some(dir), Heard::Turn { text, heard }) = (dir, ev) else { return };
    let _ = std::fs::create_dir_all(dir);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let audio: Vec<f32> = recent.buf.iter().copied().collect();
    let _ = crate::wav::write(&dir.join(format!("{stamp}.wav")), &audio, rate);
    let _ = std::fs::write(dir.join(format!("{stamp}.txt")), format!("{}\n{}\n", text, heard.clone().unwrap_or_default()));
}

/// Decides which mic audio reaches the recognizer, and when.
struct Feed {
    chunk: usize,
    buf: Vec<f32>,
    gate: crate::vad::Gate,
    /// Audio from just before the voice detector fired, so the first word is not clipped.
    preroll: Recent,
}

impl Feed {
    fn new(chunk: usize, preroll: usize) -> Feed {
        Feed { chunk, buf: Vec::with_capacity(chunk * 2), gate: crate::vad::Gate::new(), preroll: Recent::new(preroll) }
    }

    /// One cleaned frame and its voice probabilities (None: no voice detector, all audio passes).
    /// Returns audio for the recognizer once a chunk is full, and also when the gate closes: the
    /// rest (possibly empty) goes in right away so the open line can finish now, not the next
    /// time someone talks.
    fn push(&mut self, f: &[f32], probs: Option<&[f32]>) -> Option<Vec<f32>> {
        if let Some(p) = probs {
            let was_open = self.gate.is_open();
            let opened = self.gate.update(p);
            if !self.gate.is_open() {
                self.preroll.push(f);
                return was_open.then(|| std::mem::take(&mut self.buf));
            }
            if opened {
                self.buf.extend(self.preroll.buf.drain(..));
            }
        }
        self.buf.extend_from_slice(f);
        (self.buf.len() >= self.chunk).then(|| std::mem::take(&mut self.buf))
    }
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
    fn update(&mut self, lines: &[parley_moonshine::Line], now: Instant, agent_speaking: bool, echo: &str) -> Vec<Heard> {
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

    fn line(text: &str, complete: bool) -> parley_moonshine::Line {
        parley_moonshine::Line {
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

    #[test]
    fn closing_gate_flushes_the_rest() {
        let mut feed = Feed::new(4000, 800);
        let frame = vec![0.1f32; 512];
        assert!(feed.push(&frame, Some(&[0.1])).is_none(), "closed gate keeps audio as preroll");
        assert!(feed.push(&frame, Some(&[0.9])).is_none(), "open, but under a chunk");
        assert!(feed.push(&frame, Some(&[0.8])).is_none());
        let rest = feed.push(&frame, Some(&[0.1; 64])).expect("closing hands over what is left");
        // the preroll frame plus the two frames while open
        assert_eq!(rest.len(), 512 * 3);
        assert!(feed.push(&frame, Some(&[0.1])).is_none(), "nothing more while closed");
        // without a voice detector everything passes in chunks
        let mut all = Feed::new(1000, 800);
        assert!(all.push(&frame, None).is_none());
        assert_eq!(all.push(&frame, None).map(|a| a.len()), Some(1024));
    }
}

#[cfg(test)]
mod release_tests {
    use super::*;

    #[test]
    fn turn_after_clock_release_is_not_lost() {
        let l = |t: &str, c| parley_moonshine::Line { id: 0, text: t.into(), start: 0.0, duration: 0.0, complete: c, text_changed: true, latency_ms: 0 };
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
