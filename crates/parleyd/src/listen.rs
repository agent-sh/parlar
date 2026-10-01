//! Mic to finished user turns: streaming recognition, turn endpointing, filler cleanup, and
//! barge-in detection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use parley_moonshine::{ARCH_SMALL_STREAMING, Transcriber};
use tokio::sync::mpsc::UnboundedSender;

use crate::audio::{self, Frames};

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
    pub partials: bool,
    pub keyterms: Option<String>,
}

/// What the agent is saying right now, so its own voice picked up by the mic is not mistaken
/// for the user. Stand-in until echo cancellation is in the capture path.
pub type Echo = Arc<Mutex<String>>;

pub fn spawn(
    frames: Frames,
    cfg: Config,
    agent_speaking: Arc<AtomicBool>,
    echo: Echo,
    tx: UnboundedSender<Heard>,
) -> Result<()> {
    let opts: Vec<(&str, &str)> = if cfg.partials { vec![] } else { vec![("decode_incomplete_lines", "false")] };
    let t = Transcriber::load(&crate::models::stt_dir(), ARCH_SMALL_STREAMING, &opts)?;
    if let Some(k) = &cfg.keyterms {
        t.set_keyterms(k)?;
    }
    std::thread::Builder::new().name("parley-listen".into()).spawn(move || {
        if let Err(e) = run(t, frames, cfg, agent_speaking, echo, tx) {
            eprintln!("listener stopped: {e:#}");
        }
    })?;
    Ok(())
}

fn run(
    t: Transcriber,
    frames: Frames,
    cfg: Config,
    agent_speaking: Arc<AtomicBool>,
    echo: Echo,
    tx: UnboundedSender<Heard>,
) -> Result<()> {
    let mut s = t.stream()?;
    s.start()?;
    let chunk = frames.rate as usize * cfg.every_ms / 1000;
    let mut buf: Vec<f32> = Vec::with_capacity(chunk * 2);
    let mut ep = Endpointer::default();
    let mut last_level = Instant::now();
    let mut peak = 0f32;
    loop {
        match frames.rx.recv_timeout(Duration::from_millis(30)) {
            Ok(f) => {
                peak = peak.max(audio::rms(&f));
                buf.extend_from_slice(&f);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if last_level.elapsed() >= Duration::from_millis(33) {
            let _ = tx.send(Heard::Level((peak * 6.0).min(1.0)));
            peak = 0.0;
            last_level = Instant::now();
        }
        if buf.len() >= chunk {
            s.add_audio(&buf, frames.rate as i32)?;
            buf.clear();
            let lines = s.transcribe()?;
            let speaking = agent_speaking.load(Ordering::SeqCst);
            let said = echo.lock().unwrap().clone();
            for ev in ep.update(&lines, Instant::now(), speaking, &said) {
                let _ = tx.send(ev);
            }
        } else {
            for ev in ep.tick(Instant::now()) {
                let _ = tx.send(ev);
            }
        }
    }
}

#[derive(Default)]
struct Endpointer {
    /// Lines before this index belong to turns already delivered or dropped as echo.
    done: usize,
    /// Line count at the last update.
    seen: usize,
    text: String,
    complete: bool,
    changed: Option<Instant>,
    talking: bool,
    barged: bool,
}

impl Endpointer {
    fn update(&mut self, lines: &[parley_moonshine::Line], now: Instant, agent_speaking: bool, echo: &str) -> Vec<Heard> {
        let mut out = Vec::new();
        self.seen = lines.len();
        if agent_speaking {
            // the mic heard the agent: drop those lines once they settle
            while self.done < lines.len() && lines[self.done].complete && is_echo(&lines[self.done].text, echo) {
                self.done += 1;
            }
        }
        let open = &lines[self.done.min(lines.len())..];
        let text = open.iter().map(|l| l.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
        let complete = open.iter().all(|l| l.complete);
        let talking = open.last().is_some_and(|l| !l.complete);
        if agent_speaking && !text.is_empty() && is_echo(&text, echo) {
            return out;
        }
        if talking != self.talking {
            self.talking = talking;
            out.push(Heard::Talking(talking));
        }
        if text != self.text {
            self.text = text.clone();
            self.changed = Some(now);
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

    fn release(&mut self, now: Instant) -> Vec<Heard> {
        let Some(changed) = self.changed else { return vec![] };
        if self.text.is_empty() || !self.complete {
            return vec![];
        }
        if now.duration_since(changed) < hold(&self.text) {
            return vec![];
        }
        let heard = std::mem::take(&mut self.text);
        // every open line was complete at the last update, so the next turn starts after them
        self.done = self.seen;
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
    }
}

const HOLD_TAIL: &[&str] = &[
    "and", "so", "but", "or", "because", "um", "uh", "like", "the", "a", "an", "to", "of", "with",
    "then", "wait", "if", "that", "is", "maybe", "also",
];

/// How long a complete phrase must sit before the turn is handed over.
fn hold(text: &str) -> Duration {
    let t = text.trim_end();
    if t.ends_with('?') {
        return Duration::from_millis(250);
    }
    let last = t
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if t.ends_with(',') || t.ends_with("...") || HOLD_TAIL.contains(&last.as_str()) || last == "think" {
        return Duration::from_millis(1600);
    }
    Duration::from_millis(450)
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
        assert!(hold("open the router and") > hold("open the router"));
        assert!(hold("is it done?") < hold("open the router"));
        assert!(hold("let me think") > hold("open the router"));
    }

    #[test]
    fn turn_waits_for_completion_and_hold() {
        let mut ep = Endpointer::default();
        let t0 = Instant::now();
        let ev = ep.update(&[line("open the router", false)], t0, false, "");
        assert!(ev.iter().all(|e| !matches!(e, Heard::Turn { .. })));
        let ev = ep.update(&[line("open the router file", true)], t0 + Duration::from_millis(300), false, "");
        assert!(ev.iter().all(|e| !matches!(e, Heard::Turn { .. })));
        let ev = ep.tick(t0 + Duration::from_millis(900));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Open the router file"));
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
        let ev = ep.tick(t0 + Duration::from_millis(2000));
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
        assert_eq!(ep.tick(t0 + Duration::from_secs(1)).len(), 1);
        ep.update(&[l("first thing", true), l("second thing", true)], t0 + Duration::from_secs(2), false, "");
        let ev = ep.tick(t0 + Duration::from_secs(3));
        assert!(matches!(&ev[..], [Heard::Turn { text, .. }] if text == "Second thing"));
    }
}
