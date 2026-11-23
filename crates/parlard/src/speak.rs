//! The agent's voice: Kokoro through libmoonshine (one synthesis call per sentence) or an
//! OpenAI-compatible API (streamed per sentence), played through the speaker buffer. Barge-in
//! clears the buffer at once, so audio can be made well ahead. Both go through the same player,
//! so the echo canceller hears what was played and barge-in works the same.
//!
//! The voice model loads when a conversation starts (or on the first line) and unloads once the
//! conversation has been off and silent for `UNLOAD_AFTER`, like the listener's models. With the
//! API voice, the session opens when a conversation starts instead, and Kokoro loads only when
//! the API fails.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;

use parlar::voice::Speaker;
use parlar_moonshine::Tts;

use crate::audio::Player;
use crate::config::Fallback;
use crate::listen::{Echo, UNLOAD_AFTER};
use crate::models;
use crate::openai;

/// A speaker that has not asked for audio this long is stuck (suspended, gone); the line is
/// given up so the queue moves on.
const STALL: Duration = Duration::from_secs(2);

pub struct Kokoro {
    tts: Mutex<Option<Tts>>,
    voice: String,
    /// When a line last finished.
    last: Mutex<Instant>,
    player: Arc<Player>,
    /// Raised while a line is playing, for the listener's barge-in check.
    pub playing: Arc<AtomicBool>,
    /// What is playing, plus a moment after, for the listener's echo check.
    pub echo: Echo,
    /// The API voice, when it is the engine; Kokoro is then the fallback.
    remote: Option<openai::Tts>,
    fallback: Fallback,
}

impl Kokoro {
    /// `active` is raised while a conversation is on.
    pub fn new(
        voice: &str,
        player: Arc<Player>,
        active: Arc<AtomicBool>,
        remote: Option<openai::Tts>,
        fallback: Fallback,
    ) -> Arc<Self> {
        let k = Arc::new(Kokoro {
            tts: Mutex::new(None),
            voice: voice.into(),
            last: Mutex::new(Instant::now()),
            player,
            playing: Arc::default(),
            echo: Echo::default(),
            remote,
            fallback,
        });
        let weak = Arc::downgrade(&k);
        let life = move || {
            let (mut off_since, mut failed) = (None::<Instant>, false);
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let Some(k) = weak.upgrade() else { return };
                if active.load(Ordering::SeqCst) {
                    off_since = None;
                    if let Some(r) = &k.remote {
                        // open the session ahead of the first line; a failure waits retry_after_s
                        if r.client.available()
                            && let Err(e) = r.warm()
                        {
                            k.api_failed(r, &e);
                            if k.fallback == Fallback::Local {
                                // load Kokoro now rather than in front of the first line
                                if let Err(e) = k.ready() {
                                    eprintln!("voice: {e:#}");
                                }
                            }
                        }
                        continue;
                    }
                    match k.ready() {
                        Ok(_) => failed = false,
                        Err(e) if !failed => {
                            eprintln!("voice: {e:#}");
                            failed = true;
                        }
                        Err(_) => {}
                    }
                    continue;
                }
                let off = off_since.get_or_insert_with(Instant::now).elapsed();
                if off > UNLOAD_AFTER && k.last.lock().unwrap().elapsed() > UNLOAD_AFTER {
                    if let Some(r) = &k.remote {
                        r.close();
                    }
                    // a line being spoken holds the lock; try again on the next tick
                    if let Ok(mut t) = k.tts.try_lock()
                        && t.take().is_some()
                    {
                        models::release_memory();
                        eprintln!("conversation stopped for {} s; voice unloaded", UNLOAD_AFTER.as_secs());
                    }
                }
            }
        };
        std::thread::Builder::new().name("voice-life".into()).spawn(life).expect("spawn voice thread");
        k
    }

    /// The API failed: say so once, and keep off it for `retry_after_s`.
    fn api_failed(&self, r: &openai::Tts, e: &anyhow::Error) {
        r.client.failed(e);
        let next = match self.fallback {
            Fallback::Local => "Kokoro speaks",
            Fallback::None => "lines are not spoken",
        };
        eprintln!(
            "{}: {e:#}; {next} for {} s, then the API is tried again",
            r.client.what,
            r.client.conn.retry_after.as_secs()
        );
    }

    /// The synthesizer, loaded if it is not.
    fn ready(&self) -> Result<MutexGuard<'_, Option<Tts>>> {
        let mut t = self.tts.lock().unwrap();
        if t.is_none() {
            let t0 = Instant::now();
            models::moonshine()?;
            *t = Some(Tts::load(&models::tts_dir(), tts_language(), &self.voice, &[])?);
            eprintln!("voice loaded in {:.0} ms", t0.elapsed().as_secs_f32() * 1e3);
        }
        Ok(t)
    }
}

/// libmoonshine's name for the configured language.
pub fn tts_language() -> &'static str {
    crate::config::get().language().tts.unwrap_or("en_us")
}

struct Playing<'a>(&'a Kokoro);

/// Stamps the end of a line, for the unload timer.
struct Done<'a>(&'a Kokoro);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        *self.0.last.lock().unwrap() = Instant::now();
    }
}

impl Drop for Playing<'_> {
    fn drop(&mut self) {
        self.0.playing.store(false, Ordering::SeqCst);
        // the tail is still in the device and the recognizer, so the echo text stays a moment
        self.0.echo.end();
    }
}

/// What the maker thread hands the player.
enum Made {
    Audio(Vec<f32>, u32),
    /// The sentence whose audio was just handed over is complete.
    End(String),
}

impl Speaker for Kokoro {
    fn speak(&self, text: &str, cancel: &AtomicBool) -> Option<String> {
        let remote = self.remote.as_ref().filter(|r| r.client.available());
        // Kokoro is needed up front only when the API is not used for this line
        if remote.is_none() {
            if self.remote.is_some() && self.fallback == Fallback::None {
                eprintln!("voice: the API is unavailable and fallback is off; line not spoken");
                return None;
            }
            if let Err(e) = self.ready() {
                eprintln!("voice: {e:#}; line not spoken");
                return None;
            }
        }
        let _done = Done(self);
        self.echo.start(text);
        self.playing.store(true, Ordering::SeqCst);
        let _playing = Playing(self);
        let sentences = split(text);
        let stop = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<Made>();
        let (sentences_ref, stop_ref) = (&sentences, &stop);
        std::thread::scope(|scope| {
            // whole sentences, made ahead on their own thread: the speaker never waits on the
            // maker mid-sentence, and the next sentence is requested while this one plays. The
            // sender moves into the thread, so the player sees the end when the last one is made.
            scope.spawn(move || {
                let (sentences, stop) = (sentences_ref, stop_ref);
                let mut remote = remote;
                let mut guard = None;
                for s in sentences {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    if let Some(r) = remote {
                        let mut got = false;
                        // chunks at another rate are resampled with one state per sentence, so
                        // they join without a click
                        let mut up: Option<crate::audio::Resampler> = None;
                        let res = r.sentence(s, &mut |pcm, rate| {
                            if stop.load(Ordering::SeqCst) {
                                return false;
                            }
                            if !pcm.is_empty() {
                                got = true;
                                if rate == crate::audio::VOICE_RATE {
                                    return tx.send(Made::Audio(pcm.to_vec(), rate)).is_ok();
                                }
                                let rs = up.get_or_insert_with(|| {
                                    crate::audio::Resampler::new(rate, crate::audio::VOICE_RATE)
                                });
                                let mut out = Vec::new();
                                rs.process(pcm, &mut out);
                                return tx.send(Made::Audio(out, crate::audio::VOICE_RATE)).is_ok();
                            }
                            true
                        });
                        match res {
                            Ok(()) => {
                                if tx.send(Made::End(s.clone())).is_err() {
                                    return;
                                }
                                continue;
                            }
                            Err(_) if stop.load(Ordering::SeqCst) => return,
                            Err(e) => {
                                // another thread may have marked it failed already
                                if r.client.available() {
                                    self.api_failed(r, &e);
                                }
                                if self.fallback == Fallback::None {
                                    return;
                                }
                                remote = None;
                                if got {
                                    // part of it was heard, so it is not repeated, but it does not
                                    // count as heard in a cut report; Kokoro goes on with the next
                                    continue;
                                }
                            }
                        }
                    }
                    if guard.is_none() {
                        match self.ready() {
                            Ok(g) => guard = Some(g),
                            Err(e) => {
                                eprintln!("voice: {e:#}; rest of the line not spoken");
                                return;
                            }
                        }
                    }
                    let tts = guard.as_ref().and_then(|g| g.as_ref()).expect("loaded by ready");
                    match tts.synthesize(s) {
                        Ok((pcm, rate)) if !pcm.is_empty() => {
                            let pcm = tighten(&pcm, rate as u32);
                            if tx.send(Made::Audio(pcm, rate as u32)).is_err() || tx.send(Made::End(s.clone())).is_err()
                            {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("tts: {e:#}");
                            return;
                        }
                    }
                }
            });
            self.play(rx, cancel, &stop)
        })
    }

    fn level(&self) -> f32 {
        self.player.level()
    }
}

impl Kokoro {
    /// Feed synthesized sentences to the speaker until they are all heard, the user cuts in, or
    /// the speaker stops taking audio. Returns the words heard before a cut.
    fn play(&self, rx: std::sync::mpsc::Receiver<Made>, cancel: &AtomicBool, stop: &AtomicBool) -> Option<String> {
        // (sentence, seconds of audio up to its end) for everything handed to the speaker
        let mut pushed: Vec<(String, f32)> = Vec::new();
        let mut total = 0f32;
        let mut done = false;
        let t0 = Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                stop.store(true, Ordering::SeqCst);
                let played = total - self.player.queued_secs();
                self.player.clear();
                // the speaker is silent from here, even while the maker thread winds down
                self.playing.store(false, Ordering::SeqCst);
                self.echo.end();
                let heard: Vec<&str> =
                    pushed.iter().filter(|(_, end)| *end <= played + 0.05).map(|(t, _)| t.as_str()).collect();
                return Some(heard.join(" "));
            }
            if !done {
                match rx.recv_timeout(Duration::from_millis(15)) {
                    Ok(Made::Audio(pcm, rate)) => {
                        if total == 0.0 {
                            eprintln!("voice: first audio in {:.0} ms", t0.elapsed().as_secs_f32() * 1e3);
                        }
                        total += pcm.len() as f32 / rate as f32;
                        self.player.push(&pcm, rate);
                    }
                    Ok(Made::End(text)) => pushed.push((text, total)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => done = true,
                }
            } else {
                if self.player.queued_secs() <= 0.0 {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            if self.player.queued_secs() > 0.0 && self.player.stalled(STALL) {
                eprintln!("speaker stopped taking audio; dropping the line");
                stop.store(true, Ordering::SeqCst);
                self.player.clear();
                return None;
            }
        }
    }
}

/// The line as sentences, the way libmoonshine splits them; without libmoonshine (an API voice
/// with no local install), at sentence-ending punctuation.
fn split(text: &str) -> Vec<String> {
    parlar_moonshine::split_utterances(tts_language(), text).unwrap_or_else(|_| split_sentences(text))
}

pub fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|n| n.is_whitespace()) {
            // "1.5" and "e.g." stay inside; a short piece joins the next
            if cur.split_whitespace().count() >= 3 {
                out.push(cur.trim().to_string());
                cur.clear();
            }
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Kokoro leaves long silences at sentence ends and commas (0.4 to 0.7 s), which sound like the
/// voice breaking up. Leading silence is trimmed and any pause is capped, keeping a short one.
pub fn tighten(pcm: &[f32], rate: u32) -> Vec<f32> {
    let block = (rate / 100) as usize; // 10 ms
    let max_pause = 25; // blocks: 250 ms
    let tail = 18; // blocks kept after the sentence: 180 ms
    let quiet: Vec<bool> =
        pcm.chunks(block).map(|b| (b.iter().map(|v| v * v).sum::<f32>() / b.len() as f32).sqrt() < 0.006).collect();
    let first = quiet.iter().position(|q| !q).unwrap_or(quiet.len());
    let last = quiet.iter().rposition(|q| !q).map(|i| i + 1).unwrap_or(0);
    let mut out = Vec::with_capacity(pcm.len());
    let mut run = 0;
    for (i, q) in quiet.iter().enumerate().take(last).skip(first.saturating_sub(3)) {
        let b = &pcm[i * block..((i + 1) * block).min(pcm.len())];
        if *q {
            run += 1;
            if run > max_pause {
                continue;
            }
        } else {
            run = 0;
        }
        out.extend_from_slice(b);
    }
    out.extend(std::iter::repeat_n(0.0, tail * block));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_split_at_their_end_only() {
        assert_eq!(
            split_sentences("Done. The p95 went from 1.2 s to 480 ms! Push it now? Yes"),
            vec!["Done. The p95 went from 1.2 s to 480 ms!", "Push it now?", "Yes"]
        );
        assert_eq!(split_sentences("One sentence only"), vec!["One sentence only"]);
    }

    #[test]
    fn long_pauses_are_capped_and_short_ones_kept() {
        let rate = 24000;
        let tone =
            |ms: usize| (0..rate as usize * ms / 1000).map(|i| (i as f32 * 0.05).sin() * 0.3).collect::<Vec<f32>>();
        let gap = |ms: usize| vec![0.0f32; rate as usize * ms / 1000];
        let pcm: Vec<f32> = [gap(300), tone(500), gap(700), tone(500), gap(100), tone(500), gap(400)].concat();
        let out = tighten(&pcm, rate);
        let secs = out.len() as f32 / rate as f32;
        // 1.5 s of speech, a 250 ms cap on the long pause, the short pause kept, 180 ms tail,
        // leading silence trimmed to 30 ms
        assert!((secs - (1.5 + 0.25 + 0.1 + 0.18 + 0.03)).abs() < 0.03, "{secs}");
    }
}
