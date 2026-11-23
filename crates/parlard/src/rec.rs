//! The recognizer behind the listener, on its own thread so a slow transcription (a network
//! round trip) never holds up the mic, the echo canceller or barge-in. The listener streams the
//! turn's audio in, asks for a transcript at each pause, and picks the text up when it is ready.
//!
//! With the OpenAI engine, a failed call (network, 401, 429) is logged once and the speech is
//! transcribed by Phonon-2 instead, loaded on first need, until `retry_after_s` has passed.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::Fallback;
use crate::openai::Stt;
use crate::stt::Tdt;
use crate::vocab::Vocab;

enum Job {
    /// Speech of the current turn, 16 kHz.
    Append(Vec<f32>),
    /// The turn was dropped or delivered: forget its audio.
    Reset,
    /// A pause: transcribe what came since the last one. `before` is the turn's text so far.
    Commit { turn: u64, before: String },
}

/// A transcript for one pause of turn `turn`.
pub struct Piece {
    pub turn: u64,
    pub text: String,
    pub ms: f32,
    pub secs: f32,
}

pub struct Recognizer {
    tx: Sender<Job>,
    pub done: Receiver<Piece>,
}

impl Recognizer {
    /// `local` is Phonon-2 when it is the engine (or already loaded); `remote` the API engine.
    pub fn start(local: Option<Tdt>, remote: Option<Stt>, fallback: Fallback, vocab: Arc<Mutex<Vocab>>) -> Recognizer {
        let (tx, rx) = channel();
        let (done_tx, done) = channel();
        std::thread::Builder::new()
            .name("parlar-recognize".into())
            .spawn(move || work(rx, done_tx, local, remote, fallback, vocab))
            .expect("spawn recognizer thread");
        Recognizer { tx, done }
    }

    pub fn append(&self, pcm: &[f32]) {
        let _ = self.tx.send(Job::Append(pcm.to_vec()));
    }

    pub fn reset(&self) {
        let _ = self.tx.send(Job::Reset);
    }

    pub fn commit(&self, turn: u64, before: &str) {
        let _ = self.tx.send(Job::Commit { turn, before: before.to_string() });
    }
}

fn work(
    rx: Receiver<Job>,
    done: Sender<Piece>,
    mut local: Option<Tdt>,
    remote: Option<Stt>,
    fallback: Fallback,
    vocab: Arc<Mutex<Vocab>>,
) {
    let terms = || vocab.lock().unwrap().terms.clone();
    if let Some(r) = &remote
        && let Err(e) = r.warm(&terms())
    {
        failed(r, &e, fallback);
        // load Phonon-2 now rather than at the first pause
        if fallback == Fallback::Local {
            fall_back(&mut local, fallback, &[]);
        }
    }
    let mut buf: Vec<f32> = Vec::new();
    // every sample of `buf` went up the Realtime session
    let mut streamed = true;
    let mut fresh = true;
    // the jobs end when the listener drops its models: the session and Phonon-2 go with them
    for job in rx {
        match job {
            Job::Append(p) => {
                if let Some(r) = remote.as_ref().filter(|r| r.streams() && r.client.available()) {
                    if fresh {
                        fresh = false;
                        // a session closed while idle (or never opened) is opened as the turn starts
                        streamed = r.warm(&terms()).is_ok();
                    }
                    if streamed && r.append(&p).is_err() {
                        streamed = false;
                    }
                } else {
                    streamed = false;
                }
                buf.extend_from_slice(&p);
            }
            Job::Reset => {
                if !buf.is_empty()
                    && let Some(r) = remote.as_ref().filter(|r| r.streams())
                {
                    r.clear();
                }
                buf.clear();
                (streamed, fresh) = (true, true);
            }
            Job::Commit { turn, before } => {
                let audio = std::mem::take(&mut buf);
                let was_streamed = streamed && !fresh;
                (streamed, fresh) = (true, true);
                let t0 = Instant::now();
                let text = match remote.as_ref().filter(|r| r.client.available()) {
                    Some(r) => match r.commit(&audio, was_streamed, &before, &terms()) {
                        Ok(t) => {
                            r.client.recovered();
                            t
                        }
                        Err(e) => {
                            failed(r, &e, fallback);
                            fall_back(&mut local, fallback, &audio)
                        }
                    },
                    None if remote.is_some() => fall_back(&mut local, fallback, &audio),
                    None => transcribe_local(&mut local, &audio),
                };
                let piece =
                    Piece { turn, text, ms: t0.elapsed().as_secs_f32() * 1e3, secs: audio.len() as f32 / 16000.0 };
                if done.send(piece).is_err() {
                    return;
                }
            }
        }
    }
}

/// The API failed: say so once, and keep off it for `retry_after_s`.
fn failed(r: &Stt, e: &anyhow::Error, fallback: Fallback) {
    r.client.failed(e);
    let next = match fallback {
        Fallback::Local => "Phonon-2 transcribes",
        Fallback::None => "nothing is transcribed",
    };
    eprintln!(
        "{}: {e:#}; {next} for {} s, then the API is tried again",
        r.client.what,
        r.client.conn.retry_after.as_secs()
    );
}

fn fall_back(local: &mut Option<Tdt>, fallback: Fallback, audio: &[f32]) -> String {
    match fallback {
        Fallback::None => String::new(),
        Fallback::Local => {
            if local.is_none() {
                let t0 = Instant::now();
                match Tdt::load(&crate::models::final_dir(), &crate::models::ort_lib(), 4) {
                    Ok(t) => {
                        eprintln!("local recognizer loaded for the fallback in {:.1} s", t0.elapsed().as_secs_f32());
                        *local = Some(t);
                    }
                    Err(e) => {
                        eprintln!("local recognizer unavailable: {e:#}; run parlard fetch");
                        return String::new();
                    }
                }
            }
            transcribe_local(local, audio)
        }
    }
}

fn transcribe_local(local: &mut Option<Tdt>, audio: &[f32]) -> String {
    let Some(t) = local.as_mut() else { return String::new() };
    t.transcribe(audio).unwrap_or_else(|e| {
        eprintln!("recognizer: {e:#}");
        String::new()
    })
}
