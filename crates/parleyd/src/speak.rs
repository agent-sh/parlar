//! Kokoro through libmoonshine's streaming synthesizer, played through the speaker buffer.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use parley::voice::Speaker;
use parley_moonshine::{Next, Tts};

use crate::audio::Player;
use crate::listen::Echo;

/// How much synthesized audio may wait ahead of the speaker. Small keeps barge-in snappy.
const AHEAD_SECS: f32 = 0.6;

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

/// Stop synthesis and discard what it already made, so none of it reaches the next line.
fn abandon(tts: &Tts) {
    let _ = tts.cancel();
    while let Ok(Next::Audio { .. }) = tts.next() {}
}

impl Speaker for Kokoro {
    fn speak(&self, text: &str, cancel: &AtomicBool) -> Option<String> {
        let tts = self.tts.lock().unwrap();
        self.echo.start(text);
        self.playing.store(true, Ordering::SeqCst);
        let _playing = Playing(self);
        let mut said: Vec<String> = Vec::new();
        let cut = |said: &[String], player: &Player| {
            // the last chunk handed to the speaker was most likely not heard in full
            let n = if player.queued_secs() > 0.05 { said.len().saturating_sub(1) } else { said.len() };
            player.clear();
            Some(said[..n].join(" "))
        };
        let stalled = |player: &Player| {
            if !player.stalled(STALL) {
                return false;
            }
            eprintln!("speaker stopped taking audio; dropping the line");
            player.clear();
            true
        };
        if let Err(e) = tts.push(text).and_then(|_| tts.end_input()) {
            eprintln!("tts: {e:#}");
            abandon(&tts);
            return None;
        }
        loop {
            if cancel.load(Ordering::SeqCst) {
                abandon(&tts);
                return cut(&said, &self.player);
            }
            if self.player.queued_secs() > AHEAD_SECS {
                if stalled(&self.player) {
                    abandon(&tts);
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
                continue;
            }
            match tts.next() {
                Ok(Next::Audio { pcm, sample_rate, text, .. }) => {
                    self.player.push(&pcm, sample_rate as u32);
                    if !text.trim().is_empty() {
                        said.push(text.trim().to_string());
                    }
                }
                Ok(Next::NeedText) => {
                    let _ = tts.flush();
                }
                Ok(Next::End) | Ok(Next::Cancelled) => break,
                Err(e) => {
                    eprintln!("tts: {e:#}");
                    abandon(&tts);
                    break;
                }
            }
        }
        while self.player.queued_secs() > 0.0 {
            if cancel.load(Ordering::SeqCst) {
                return cut(&said, &self.player);
            }
            if stalled(&self.player) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        None
    }

    fn level(&self) -> f32 {
        self.player.level()
    }
}
