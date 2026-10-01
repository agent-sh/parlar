//! Kokoro through libmoonshine, one synthesis call per sentence, played through the speaker
//! buffer. Barge-in clears the buffer at once, so audio can be synthesized well ahead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parley::voice::Speaker;
use parley_moonshine::Tts;

use crate::audio::Player;
use crate::listen::Echo;

/// A speaker that has not asked for audio this long is stuck (suspended, gone); the line is
/// given up so the queue moves on.
const STALL: Duration = Duration::from_secs(2);

pub struct Kokoro {
    tts: Mutex<Tts>,
    player: Arc<Player>,
    /// Raised while a line is playing, for the listener's barge-in check.
    pub playing: Arc<AtomicBool>,
    /// What is playing, plus a moment after, for the listener's echo check.
    pub echo: Echo,
}

impl Kokoro {
    pub fn new(tts: Tts, player: Arc<Player>) -> Self {
        Kokoro { tts: Mutex::new(tts), player, playing: Arc::default(), echo: Echo::default() }
    }
}

struct Playing<'a>(&'a Kokoro);

impl Drop for Playing<'_> {
    fn drop(&mut self) {
        self.0.playing.store(false, Ordering::SeqCst);
        // the tail is still in the device and the recognizer, so the echo text stays a moment
        self.0.echo.end();
    }
}

impl Speaker for Kokoro {
    fn speak(&self, text: &str, cancel: &AtomicBool) -> Option<String> {
        let tts = self.tts.lock().unwrap();
        self.echo.start(text);
        self.playing.store(true, Ordering::SeqCst);
        let _playing = Playing(self);
        let sentences = parley_moonshine::split_utterances("en_us", text).unwrap_or_else(|_| vec![text.to_string()]);
        let stop = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<(String, Vec<f32>, u32)>();
        let (tts_ref, sentences_ref, stop_ref) = (&*tts, &sentences, &stop);
        std::thread::scope(|scope| {
            // whole sentences, synthesized ahead on their own thread: one call per sentence has no
            // seams inside it, and the speaker never waits on the synthesizer mid-sentence. The
            // sender moves into the thread, so the player sees the end when the last one is made.
            scope.spawn(move || {
                let (tts, sentences, stop) = (tts_ref, sentences_ref, stop_ref);
                for s in sentences {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    match tts.synthesize(s) {
                        Ok((pcm, rate)) if !pcm.is_empty() => {
                            let pcm = tighten(&pcm, rate as u32);
                            if tx.send((s.clone(), pcm, rate as u32)).is_err() {
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
    fn play(&self, rx: std::sync::mpsc::Receiver<(String, Vec<f32>, u32)>, cancel: &AtomicBool, stop: &AtomicBool) -> Option<String> {
        // (sentence, seconds of audio up to its end) for everything handed to the speaker
        let mut pushed: Vec<(String, f32)> = Vec::new();
        let mut total = 0f32;
        let mut done = false;
        loop {
            if cancel.load(Ordering::SeqCst) {
                stop.store(true, Ordering::SeqCst);
                let played = total - self.player.queued_secs();
                self.player.clear();
                let heard: Vec<&str> = pushed.iter().filter(|(_, end)| *end <= played + 0.05).map(|(t, _)| t.as_str()).collect();
                return Some(heard.join(" "));
            }
            if !done {
                match rx.recv_timeout(Duration::from_millis(15)) {
                    Ok((text, pcm, rate)) => {
                        total += pcm.len() as f32 / rate as f32;
                        self.player.push(&pcm, rate);
                        pushed.push((text, total));
                    }
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

/// Kokoro leaves long silences at sentence ends and commas (0.4 to 0.7 s), which sound like the
/// voice breaking up. Leading silence is trimmed and any pause is capped, keeping a short one.
pub fn tighten(pcm: &[f32], rate: u32) -> Vec<f32> {
    let block = (rate / 100) as usize; // 10 ms
    let max_pause = 25; // blocks: 250 ms
    let tail = 18; // blocks kept after the sentence: 180 ms
    let quiet: Vec<bool> = pcm
        .chunks(block)
        .map(|b| (b.iter().map(|v| v * v).sum::<f32>() / b.len() as f32).sqrt() < 0.006)
        .collect();
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
    fn long_pauses_are_capped_and_short_ones_kept() {
        let rate = 24000;
        let tone = |ms: usize| (0..rate as usize * ms / 1000).map(|i| (i as f32 * 0.05).sin() * 0.3).collect::<Vec<f32>>();
        let gap = |ms: usize| vec![0.0f32; rate as usize * ms / 1000];
        let pcm: Vec<f32> = [gap(300), tone(500), gap(700), tone(500), gap(100), tone(500), gap(400)].concat();
        let out = tighten(&pcm, rate);
        let secs = out.len() as f32 / rate as f32;
        // 1.5 s of speech, a 250 ms cap on the long pause, the short pause kept, 180 ms tail,
        // leading silence trimmed to 30 ms
        assert!((secs - (1.5 + 0.25 + 0.1 + 0.18 + 0.03)).abs() < 0.03, "{secs}");
    }
}
