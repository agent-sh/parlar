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

pub struct Kokoro {
    tts: Mutex<Tts>,
    player: Arc<Player>,
    /// Raised while a line is playing, for the listener's barge-in and echo checks.
    pub playing: Arc<AtomicBool>,
    pub echo: Echo,
}

impl Kokoro {
    pub fn new(tts: Tts, player: Arc<Player>) -> Self {
        Kokoro { tts: Mutex::new(tts), player, playing: Arc::default(), echo: Arc::default() }
    }
}

struct Playing<'a>(&'a Kokoro);

impl Drop for Playing<'_> {
    fn drop(&mut self) {
        self.0.playing.store(false, Ordering::SeqCst);
        self.0.echo.lock().unwrap().clear();
    }
}

impl Speaker for Kokoro {
    fn speak(&self, text: &str, cancel: &AtomicBool) -> Option<String> {
        let tts = self.tts.lock().unwrap();
        *self.echo.lock().unwrap() = text.to_string();
        self.playing.store(true, Ordering::SeqCst);
        let _playing = Playing(self);
        let mut said: Vec<String> = Vec::new();
        let cut = |said: &[String], player: &Player| {
            // the last chunk handed to the speaker was most likely not heard in full
            let n = if player.queued_secs() > 0.05 { said.len().saturating_sub(1) } else { said.len() };
            player.clear();
            Some(said[..n].join(" "))
        };
        if tts.push(text).and_then(|_| tts.end_input()).is_err() {
            return None;
        }
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = tts.cancel();
                while let Ok(Next::Audio { .. }) = tts.next() {}
                return cut(&said, &self.player);
            }
            if self.player.queued_secs() > AHEAD_SECS {
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
                    break;
                }
            }
        }
        while self.player.queued_secs() > 0.0 {
            if cancel.load(Ordering::SeqCst) {
                return cut(&said, &self.player);
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        None
    }

    fn level(&self) -> f32 {
        self.player.level()
    }
}
