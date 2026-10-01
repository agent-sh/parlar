mod aec;
mod audio;
mod listen;
mod models;
mod speak;
mod stt;
mod turn;
mod vad;
mod vocab;
mod wav;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use parlar::{client, daemon, voice};

#[derive(Parser)]
#[command(name = "parlard", version, about = "parlar daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Speak through a program that reads text on stdin instead of the built-in voice.
    #[arg(long, num_args = 1.., allow_hyphen_values = true)]
    voice_cmd: Vec<String>,
    /// No audio output.
    #[arg(long)]
    silent: bool,
    /// No mic: utterances only through `parlar ctl hear`.
    #[arg(long)]
    no_mic: bool,
    /// Input device (substring of its name, or its id). Default: the system default.
    #[arg(long)]
    input: Option<String>,
    /// Output device (substring of its name, or its id).
    #[arg(long)]
    output: Option<String>,
    #[arg(long, default_value = "kokoro_af_heart")]
    voice: String,
    /// Turn off echo cancellation (use with headphones, or to compare).
    #[arg(long)]
    no_aec: bool,
    /// Test input: play these WAV files into the listener at real-time pace, in order, with
    /// silence after each, instead of using the mic.
    #[arg(long, num_args = 1..)]
    input_wav: Vec<std::path::PathBuf>,
    /// Seconds before the first test WAV plays.
    #[arg(long, default_value_t = 0.0)]
    input_delay: f32,
    /// Seconds of silence after each test WAV.
    #[arg(long, default_value_t = 2.5)]
    input_gap: f32,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download the speech models.
    Fetch {
        #[arg(long, default_value = "kokoro_af_heart")]
        voice: String,
    },
    /// List audio devices.
    Devices,
    /// Write a WAV through the capture cleanup (noise suppression, gain), for inspection.
    Clean { wav: std::path::PathBuf, out: std::path::PathBuf },
    /// Transcribe a WAV with the recognizer (Phonon-2, ONNX).
    Final {
        wav: std::path::PathBuf,
        /// Model directory in the onnx-asr layout.
        #[arg(long)]
        model: Option<std::path::PathBuf>,
        #[arg(long, default_value_t = 4)]
        threads: usize,
    },
    /// Score a 16 kHz WAV with the end-of-turn model and dump its features.
    Turn {
        wav: std::path::PathBuf,
        /// Write the features as little-endian f32 here.
        #[arg(long)]
        dump: Option<std::path::PathBuf>,
    },
    /// Synthesize text to a WAV file and report timing.
    Speak {
        text: String,
        #[arg(long, default_value = "parlar-speak.wav")]
        out: std::path::PathBuf,
        #[arg(long, default_value = "kokoro_af_heart")]
        voice: String,
        /// One synthesis call per sentence instead of streamed chunks.
        #[arg(long)]
        sentences: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Fetch { voice }) => return models::fetch(&voice),
        Some(Cmd::Speak { text, out, voice, sentences }) => return speak(&text, &out, &voice, sentences),
        Some(Cmd::Final { wav, model, threads }) => {
            let dir = model.unwrap_or_else(models::final_dir);
            let t0 = std::time::Instant::now();
            let mut m = stt::Tdt::load(&dir, &models::ort_lib(), threads)?;
            let load = t0.elapsed();
            let (pcm, rate) = wav::read(&wav)?;
            let pcm = audio::resample(&pcm, rate, 16000);
            let t1 = std::time::Instant::now();
            let text = m.transcribe(&pcm)?;
            println!("{text}");
            eprintln!(
                "load {:.0} ms, {:.1} s of audio in {:.0} ms",
                load.as_secs_f32() * 1e3,
                pcm.len() as f32 / 16000.0,
                t1.elapsed().as_secs_f32() * 1e3
            );
            return Ok(());
        }
        Some(Cmd::Turn { wav, dump }) => {
            let (pcm, rate) = wav::read(&wav)?;
            let pcm = audio::resample(&pcm, rate, turn::RATE as u32);
            if let Some(d) = dump {
                let f = turn::Features::new().compute(&pcm);
                std::fs::write(d, f.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())?;
            }
            let mut st = turn::SmartTurn::load(&models::turn_model(), &models::ort_lib())?;
            let t0 = std::time::Instant::now();
            let p = st.complete(&pcm)?;
            println!("prob={p:.4} ({:.0} ms)", t0.elapsed().as_secs_f32() * 1e3);
            return Ok(());
        }
        Some(Cmd::Clean { wav, out }) => {
            let (pcm, rate) = wav::read(&wav)?;
            let x = audio::resample(&pcm, rate, aec::RATE);
            let y = aec::Aec::cleanup_only().process(&x);
            wav::write(&out, &y, aec::RATE)?;
            return Ok(());
        }
        Some(Cmd::Devices) => {
            let (ins, outs) = audio::list()?;
            println!("input:");
            ins.iter().for_each(|d| println!("  {}  {}", d.name, d.id));
            println!("output:");
            outs.iter().for_each(|d| println!("  {}  {}", d.name, d.id));
            return Ok(());
        }
        None => {}
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(serve(cli))
}

async fn serve(cli: Cli) -> Result<()> {
    use parlar::proto::Ui;
    use std::sync::atomic::{AtomicU32, Ordering};

    let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut kokoro = None;
    let engine = if !cli.voice_cmd.is_empty() {
        voice::Engine::Command(cli.voice_cmd.clone())
    } else if cli.silent {
        voice::Engine::Silent
    } else {
        let player = audio::Player::new();
        let out = cli.output.clone().or_else(|| audio::Saved::load().output);
        if let Err(e) = player.open(out.as_deref()) {
            // a saved device may be gone (headphones off); fall back to the default
            eprintln!("speaker {out:?}: {e:#}; using the default");
            player.open(None)?;
        }
        // load the library now, so a missing install shows at start; the voice itself loads with
        // the first conversation
        models::moonshine()?;
        let k = speak::Kokoro::new(&cli.voice, player.clone(), active.clone());
        kokoro = Some((k.clone(), player));
        voice::Engine::Speaker(k)
    };
    let mut d = daemon::Daemon::new(Arc::new(voice::Queue::new(engine)));
    d.state.lock().await.share_voice_gate(active);
    if std::env::var_os("PARLAR_SOCKET").is_none() && cli.input_wav.is_empty() {
        d.state.lock().await.restore_saved();
    }
    let user_level = Arc::new(AtomicU32::new(0));
    let gate = d.state.lock().await.mic_gate();

    let mut mic = None;
    let frames = if cli.no_mic {
        None
    } else if cli.input_wav.is_empty() {
        let (m, frames) = audio::Mic::new(gate);
        let inp = cli.input.clone().or_else(|| audio::Saved::load().input);
        if let Err(e) = m.open(inp.as_deref()) {
            eprintln!("mic {inp:?}: {e:#}; using the default");
            m.open(None)?;
        }
        mic = Some(m);
        Some(frames)
    } else {
        Some(audio::from_wavs(cli.input_wav.clone(), cli.input_delay, cli.input_gap, gate)?)
    };
    d.audio = Some(Arc::new(audio::Control { mic, player: kokoro.as_ref().map(|(_, p)| p.clone()) }));
    let d = Arc::new(d);

    if let Some(frames) = frames {
        let (playing, echo) = match &kokoro {
            Some((k, _)) => (k.playing.clone(), k.echo.clone()),
            None => (Arc::default(), listen::Echo::default()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let vocab = Arc::new(std::sync::Mutex::new(vocab::Vocab::default()));
        let aec = match &kokoro {
            Some((_, p)) if !cli.no_aec && cli.input_wav.is_empty() => Some(aec::Aec::new(p.far.clone())),
            // test WAVs go through the same cleanup as the mic, without an echo to cancel
            _ => Some(aec::Aec::cleanup_only()),
        };
        let cfg = listen::Config { aec, gate: d.state.lock().await.mic_gate(), vocab: vocab.clone() };
        // follow the focused session's repo
        {
            let state = d.state.clone();
            tokio::spawn(async move {
                let (mut last, mut version) = (None::<String>, 0u64);
                loop {
                    let cwd = state.lock().await.focused_cwd();
                    if cwd != last {
                        last = cwd.clone();
                        version += 1;
                        let v = match cwd {
                            Some(dir) => tokio::task::spawn_blocking(move || {
                                vocab::Vocab::from_dir(std::path::Path::new(&dir), version)
                            })
                            .await
                            .unwrap_or_default(),
                            None => vocab::Vocab::empty(version),
                        };
                        eprintln!("vocabulary: {} terms", v.terms.len());
                        *vocab.lock().unwrap() = v;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            });
        }
        listen::spawn(frames, cfg, playing, echo, tx)?;
        let (state, lvl) = (d.state.clone(), user_level.clone());
        tokio::spawn(async move {
            while let Some(h) = rx.recv().await {
                match h {
                    listen::Heard::Level(v) => lvl.store(v.to_bits(), Ordering::Relaxed),
                    listen::Heard::Talking(on) => state.lock().await.set_listening(on),
                    listen::Heard::Partial(text) => {
                        eprintln!("hearing: {text}");
                        let st = state.lock().await;
                        let _ = st.ui().send(Ui::Caption { who: "user".into(), text });
                    }
                    listen::Heard::BargeIn => {
                        eprintln!("barge-in");
                        state.lock().await.barge().store(true, Ordering::SeqCst);
                    }
                    listen::Heard::Turn { text, heard } => {
                        let mut st = state.lock().await;
                        st.set_listening(false);
                        match st.deliver_spoken(text.clone(), heard) {
                            Some(id) => eprintln!("heard u{id}: {text}"),
                            None => eprintln!("heard (dropped, voice mode off): {text}"),
                        }
                    }
                }
            }
        });
    }

    // one level stream for indicators: the user's mic and the agent's speaker
    {
        let ui = d.ui.clone();
        let player = kokoro.as_ref().map(|(_, p)| p.clone());
        let lvl = user_level.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(33));
            loop {
                tick.tick().await;
                let user = f32::from_bits(lvl.load(Ordering::Relaxed));
                let agent = player.as_ref().map(|p| p.level()).unwrap_or(0.0);
                if ui.receiver_count() > 0 && (user > 0.01 || agent > 0.01) {
                    let _ = ui.send(Ui::Levels { user, agent });
                }
            }
        });
    }

    let path = client::socket_path();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let r = tokio::select! {
        r = d.serve(&path) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
        _ = term.recv() => Ok(()),
    };
    r
}

fn speak(text: &str, out: &std::path::Path, voice: &str, sentences: bool) -> Result<()> {
    use parlar_moonshine::{Next, Tts};
    use std::time::Instant;
    models::moonshine()?;
    let t0 = Instant::now();
    let tts = Tts::load(&models::tts_dir(), "en_us", voice, &[])?;
    let load = t0.elapsed();
    let t1 = Instant::now();
    if sentences {
        let (mut pcm, mut rate, mut first) = (Vec::new(), 24000, None);
        for s in parlar_moonshine::split_utterances("en_us", text)? {
            let (p, r) = tts.synthesize(&s)?;
            first.get_or_insert(t1.elapsed());
            rate = r;
            pcm.extend_from_slice(&speak::tighten(&p, r as u32));
        }
        let total = t1.elapsed();
        wav::write(out, &pcm, rate as u32)?;
        eprintln!(
            "load {:.0} ms, first audio {:.0} ms, synth {:.0} ms for {:.1} s of audio, per sentence -> {}",
            load.as_secs_f32() * 1e3,
            first.unwrap_or_default().as_secs_f32() * 1e3,
            total.as_secs_f32() * 1e3,
            pcm.len() as f32 / rate as f32,
            out.display()
        );
        return Ok(());
    }
    tts.push(text)?;
    tts.end_input()?;
    let (mut pcm, mut rate, mut first) = (Vec::new(), 24000, None);
    loop {
        match tts.next()? {
            Next::Audio { pcm: p, sample_rate, .. } => {
                first.get_or_insert(t1.elapsed());
                rate = sample_rate;
                pcm.extend_from_slice(&p);
            }
            Next::NeedText => tts.flush()?,
            Next::End | Next::Cancelled => break,
        }
    }
    let total = t1.elapsed();
    let secs = pcm.len() as f32 / rate as f32;
    wav::write(out, &pcm, rate as u32)?;
    eprintln!(
        "load {:.0} ms, first audio {:.0} ms, synth {:.0} ms for {:.1} s of audio (RTF {:.2}), {} Hz -> {}",
        load.as_secs_f32() * 1e3,
        first.unwrap_or_default().as_secs_f32() * 1e3,
        total.as_secs_f32() * 1e3,
        secs,
        total.as_secs_f32() / secs.max(0.01),
        rate,
        out.display()
    );
    Ok(())
}
