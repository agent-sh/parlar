use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use parley::client::{self, Client};
use parley::proto::{Harness, Phase, Request, Response};
use parley::{hook, mcp};

#[derive(Parser)]
#[command(name = "parley", version, about = "Voice conversation mode for coding-agent harnesses")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum HarnessArg {
    Claude,
    Codex,
    Other,
}

impl From<HarnessArg> for Harness {
    fn from(h: HarnessArg) -> Self {
        match h {
            HarnessArg::Claude => Harness::Claude,
            HarnessArg::Codex => Harness::Codex,
            HarnessArg::Other => Harness::Other,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run parleyd in the foreground (the parleyd binary next to this one). Extra arguments are
    /// passed through.
    Daemon {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// MCP server over stdio, launched by the harness.
    Mcp {
        #[arg(long, value_enum, default_value = "claude")]
        harness: HarnessArg,
    },
    /// Hook handler, launched by the harness.
    Hook {
        #[arg(value_enum)]
        event: hook::Event,
        #[arg(long, value_enum, default_value = "claude")]
        harness: HarnessArg,
    },
    /// One-line voice state for a harness status line.
    Status,
    /// Control a running parleyd.
    Ctl {
        #[command(subcommand)]
        cmd: Ctl,
    },
}

#[derive(Subcommand)]
enum Ctl {
    /// Print the daemon state as JSON.
    State,
    /// Deliver text as if it had been spoken.
    Hear {
        text: String,
        /// What the recognizer heard before cleanup.
        #[arg(long)]
        heard: Option<String>,
    },
    /// Start conversation mode.
    On,
    /// Stop conversation mode.
    Off,
    Mute,
    Unmute,
    /// Speak `say` lines out loud.
    VoiceOn,
    /// Captions only.
    VoiceOff,
    /// Give voice focus to a session id.
    Focus { session: String },
    /// List audio devices.
    Devices,
    /// Switch the mic to a device id from `devices`.
    Input { id: String },
    /// Switch the speaker to a device id from `devices`.
    Output { id: String },
    /// Print indicator events as JSON lines.
    Watch,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { args } => exec_daemon(args),
        Cmd::Mcp { harness } => mcp::serve(harness.into()),
        Cmd::Hook { event, harness } => {
            let code = hook::run(event, harness.into()).unwrap_or_else(|e| {
                // a broken hook must never break the session
                eprintln!("parley hook: {e:#}");
                0
            });
            std::process::exit(code)
        }
        Cmd::Status => {
            println!("{}", status_line());
            Ok(())
        }
        Cmd::Ctl { cmd } => ctl(cmd),
    }
}

fn exec_daemon(args: Vec<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let me = std::env::current_exe()?;
    let bin = me.with_file_name("parleyd");
    let err = std::process::Command::new(&bin).args(args).exec();
    anyhow::bail!("could not run {}: {err}", bin.display())
}

fn status_line() -> String {
    let Some(mut c) = Client::connect() else { return "voice off".into() };
    match c.call(&Request::State, Some(Duration::from_millis(300))) {
        Ok(Response::State(s)) => {
            let word = match s.phase {
                Phase::Stopped => "off",
                Phase::Connecting => "connecting",
                Phase::Ready => "ready",
                Phase::Listening | Phase::Interrupting => "listening",
                Phase::Working => "working",
                Phase::Speaking => "speaking",
            };
            let word = if s.mic_muted && s.phase != Phase::Stopped { "mic muted" } else { word };
            format!("voice {word}")
        }
        _ => "voice off".into(),
    }
}

fn ctl(cmd: Ctl) -> Result<()> {
    let Some(mut c) = Client::connect() else {
        anyhow::bail!("parleyd is not running on {}", client::socket_path().display());
    };
    let set = |active, mic_muted, voice_off| Request::Set {
        active,
        mic_muted,
        voice_off,
        focus: None,
        input: None,
        output: None,
    };
    let dev = |input, output| Request::Set {
        active: None,
        mic_muted: None,
        voice_off: None,
        focus: None,
        input,
        output,
    };
    let req = match cmd {
        Ctl::State => Request::State,
        Ctl::Hear { text, heard } => Request::Hear { text, heard },
        Ctl::On => set(Some(true), None, None),
        Ctl::Off => set(Some(false), None, None),
        Ctl::Mute => set(None, Some(true), None),
        Ctl::Unmute => set(None, Some(false), None),
        Ctl::VoiceOn => set(None, None, Some(false)),
        Ctl::VoiceOff => set(None, None, Some(true)),
        Ctl::Focus { session } => Request::Set {
            active: None,
            mic_muted: None,
            voice_off: None,
            focus: Some(session),
            input: None,
            output: None,
        },
        Ctl::Devices => Request::Devices,
        Ctl::Input { id } => dev(Some(id), None),
        Ctl::Output { id } => dev(None, Some(id)),
        Ctl::Watch => {
            use std::io::Write;
            let mut v = serde_json::to_vec(&Request::Subscribe)?;
            v.push(b'\n');
            let mut s = std::os::unix::net::UnixStream::connect(client::socket_path())?;
            s.write_all(&v)?;
            drop(c);
            let mut c = Client::from_stream(s)?;
            loop {
                print!("{}", c.read_line()?);
            }
        }
    };
    let r = c.call(&req, Some(Duration::from_secs(5)))?;
    println!("{}", serde_json::to_string_pretty(&r)?);
    if let Response::Error { .. } = r {
        std::process::exit(1);
    }
    Ok(())
}
