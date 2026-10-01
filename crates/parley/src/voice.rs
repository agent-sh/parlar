//! Speech output. The Kokoro engine plugs in behind `Voice`; until then a command engine
//! (any program that speaks text from stdin) and a silent engine stand in.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::daemon::Shared;
use crate::proto::{SayKind, Ui};

pub trait Voice: Send + Sync {
    fn speak(&self, text: String, kind: SayKind, state: Shared);
}

struct Line {
    text: String,
    kind: SayKind,
    state: Shared,
}

/// Serializes speech: one line at a time, and a status line is dropped unspoken when a newer
/// status line is already waiting behind it.
pub struct Queue {
    tx: mpsc::UnboundedSender<Line>,
}

impl Queue {
    pub fn new(engine: Engine) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Line>();
        tokio::spawn(async move {
            let mut backlog: Vec<Line> = Vec::new();
            loop {
                if backlog.is_empty() {
                    match rx.recv().await {
                        Some(l) => backlog.push(l),
                        None => return,
                    }
                }
                while let Ok(l) = rx.try_recv() {
                    backlog.push(l);
                }
                let line = backlog.remove(0);
                let stale = line.kind == SayKind::Status
                    && backlog.iter().any(|l| l.kind == SayKind::Status);
                if stale {
                    continue;
                }
                if engine.play(&line).await {
                    // the user talked over this line: what was queued behind it is stale
                    backlog.clear();
                    while rx.try_recv().is_ok() {}
                }
            }
        });
        Queue { tx }
    }
}

impl Voice for Queue {
    fn speak(&self, text: String, kind: SayKind, state: Shared) {
        let _ = self.tx.send(Line { text, kind, state });
    }
}

/// A built-in speech engine. `speak` runs on a blocking thread and returns the words that were
/// cut off when `cancel` was raised mid-line (barge-in), or None when the line finished.
pub trait Speaker: Send + Sync {
    fn speak(&self, text: &str, cancel: &std::sync::atomic::AtomicBool) -> Option<String>;
    /// Current output level, 0..1, for indicators.
    fn level(&self) -> f32 {
        0.0
    }
}

pub enum Engine {
    /// No audio. Holds the speaking phase for roughly the time the line would take.
    Silent,
    /// Pipe each line to a program that speaks stdin, e.g. `espeak-ng` or `piper`.
    Command(Vec<String>),
    Speaker(std::sync::Arc<dyn Speaker>),
}

impl Engine {
    /// Returns true when the user cut the line off.
    async fn play(&self, line: &Line) -> bool {
        eprintln!("say: {}", line.text);
        line.state.lock().await.set_speaking(true);
        let (ui, cancel) = {
            let st = line.state.lock().await;
            (st.ui(), st.barge())
        };
        cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut was_cut = false;
        // engines without a level meter get an envelope stand-in; built-in speakers are metered
        // by the daemon's level task
        let metered = matches!(self, Engine::Speaker(_));
        let pulse = tokio::spawn(async move {
            let mut t = 0f32;
            while !metered {
                t += 0.033;
                let v = ((t * 4.8 * std::f32::consts::TAU).sin().abs() * 0.8).min(1.0);
                let _ = ui.send(Ui::Levels { user: 0.0, agent: v });
                tokio::time::sleep(Duration::from_millis(33)).await;
            }
        });
        match self {
            Engine::Silent => {
                let words = line.text.split_whitespace().count() as f32;
                tokio::time::sleep(Duration::from_secs_f32(words / 2.9 + 0.3)).await;
            }
            Engine::Command(argv) => {
                if let Err(e) = run(argv, &line.text).await {
                    eprintln!("voice command failed: {e:#}");
                }
            }
            Engine::Speaker(s) => {
                let (s, text, c) = (s.clone(), line.text.clone(), cancel.clone());
                let cut = tokio::task::spawn_blocking(move || s.speak(&text, &c)).await.ok().flatten();
                if let Some(cut) = cut {
                    was_cut = true;
                    line.state.lock().await.set_cut(cut);
                }
            }
        }
        pulse.abort();
        line.state.lock().await.set_speaking(false);
        was_cut
    }
}

async fn run(argv: &[String], text: &str) -> anyhow::Result<()> {
    let (prog, args) = argv.split_first().ok_or_else(|| anyhow::anyhow!("empty voice command"))?;
    let mut child = tokio::process::Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
    }
    child.wait().await?;
    Ok(())
}
