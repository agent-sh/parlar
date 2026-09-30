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
                engine.play(&line).await;
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

pub enum Engine {
    /// No audio. Holds the speaking phase for roughly the time the line would take.
    Silent,
    /// Pipe each line to a program that speaks stdin, e.g. `espeak-ng` or `piper`.
    Command(Vec<String>),
}

impl Engine {
    async fn play(&self, line: &Line) {
        eprintln!("say: {}", line.text);
        line.state.lock().await.set_speaking(true);
        let ui = line.state.lock().await.ui();
        let pulse = tokio::spawn(async move {
            // envelope stand-in until the TTS engine reports real output levels
            let mut t = 0f32;
            loop {
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
        }
        pulse.abort();
        line.state.lock().await.set_speaking(false);
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
