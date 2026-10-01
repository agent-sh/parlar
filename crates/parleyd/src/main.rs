mod aec;
mod audio;
mod listen;
mod models;
mod speak;
mod turn;
mod vocab;
mod wav;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use parley::{client, daemon, voice};

#[derive(Parser)]
#[command(name = "parleyd", version, about = "parley daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Speak through a program that reads text on stdin instead of the built-in voice.
    #[arg(long, num_args = 1.., allow_hyphen_values = true)]
    voice_cmd: Vec<String>,
    /// No audio output.
    #[arg(long)]
    silent: bool,
    /// No mic: utterances only through `parley ctl hear`.
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
    /// Decode only finished phrases: about half the recognizer cost, slower final text.
    #[arg(long)]
    no_partials: bool,
    /// Turn off echo cancellation (use with headphones, or to compare).
    #[arg(long)]
    no_aec: bool,
    /// Comma-separated words to bias recognition toward.
    #[arg(long)]
    keyterms: Option<String>,
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
    /// Stream a WAV file through the recognizer in 100 ms chunks, as live audio would arrive.
    Transcribe {
        wav: std::path::PathBuf,
        /// Comma-separated terms to bias recognition toward.
        #[arg(long)]
        keyterms: Option<String>,
        /// Transcribe every N ms of audio.
        #[arg(long, default_value_t = 100)]
        every_ms: usize,
        /// Only decode lines once they are complete.
        #[arg(long)]
        no_partials: bool,
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
        #[arg(long, default_value = "parley-speak.wav")]
        out: std::path::PathBuf,
        #[arg(long, default_value = "kokoro_af_heart")]
        voice: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Fetch { voice }) => return models::fetch(&voice),
        Some(Cmd::Speak { text, out, voice }) => return speak(&text, &out, &voice),
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
        Some(Cmd::Devices) => {
            let (ins, outs) = audio::list()?;
            println!("input:");
            ins.iter().for_each(|d| println!("  {}  {}", d.name, d.id));
            println!("output:");
            outs.iter().for_each(|d| println!("  {}  {}", d.name, d.id));
            return Ok(());
        }
        Some(Cmd::Transcribe { wav, keyterms, every_ms, no_partials }) => {
            return transcribe(&wav, keyterms.as_deref(), every_ms, no_partials);
        }
        None => {}
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(serve(cli))
}

async fn serve(cli: Cli) -> Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};
    use parley::proto::Ui;

    let mut kokoro = None;
    let engine = if !cli.voice_cmd.is_empty() {
        voice::Engine::Command(cli.voice_cmd.clone())
    } else if cli.silent {
        voice::Engine::Silent
    } else {
        let player = audio::Player::new();
        player.open(cli.output.as_deref())?;
        let tts = parley_moonshine::Tts::load(&models::tts_dir(), "en_us", &cli.voice, &[])?;
        let k = Arc::new(speak::Kokoro::new(tts, player.clone()));
        kokoro = Some((k.clone(), player));
        voice::Engine::Speaker(k)
    };
    let mut d = daemon::Daemon::new(Arc::new(voice::Queue::new(engine)));
    let user_level = Arc::new(AtomicU32::new(0));
    let gate = d.state.lock().await.mic_gate();

    let mut mic = None;
    let frames = if cli.no_mic {
        None
    } else if cli.input_wav.is_empty() {
        let (m, frames) = audio::Mic::new(gate);
        m.open(cli.input.as_deref())?;
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
            None => (Arc::default(), Arc::default()),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let vocab = Arc::new(std::sync::Mutex::new(vocab::Vocab::default()));
        let turn = match turn::SmartTurn::load(&models::turn_model(), &models::ort_lib()) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("end-of-turn model unavailable, using the word rule only: {e:#}");
                None
            }
        };
        let aec = match &kokoro {
            Some((_, p)) if !cli.no_aec && cli.input_wav.is_empty() => Some(aec::Aec::new(p.far.clone())),
            _ => None,
        };
        let cfg = listen::Config {
            every_ms: 250,
            aec,
            turn,
            partials: !cli.no_partials,
            keyterms: cli.keyterms.clone(),
            vocab: vocab.clone(),
        };
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
                            Some(dir) => tokio::task::spawn_blocking(move || vocab::Vocab::from_dir(std::path::Path::new(&dir), version))
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
    let _ = std::fs::remove_file(&path);
    r
}

fn speak(text: &str, out: &std::path::Path, voice: &str) -> Result<()> {
    use parley_moonshine::{Next, Tts};
    use std::time::Instant;
    let t0 = Instant::now();
    let tts = Tts::load(&models::tts_dir(), "en_us", voice, &[])?;
    let load = t0.elapsed();
    let t1 = Instant::now();
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

fn transcribe(path: &std::path::Path, keyterms: Option<&str>, every_ms: usize, no_partials: bool) -> Result<()> {
    use parley_moonshine::{ARCH_SMALL_STREAMING, Transcriber};
    use std::time::Instant;
    let (pcm, rate) = wav::read(path)?;
    let t0 = Instant::now();
    let opts: &[(&str, &str)] = if no_partials { &[("decode_incomplete_lines", "false")] } else { &[] };
    let t = Transcriber::load(&models::stt_dir(), ARCH_SMALL_STREAMING, opts)?;
    if let Some(k) = keyterms {
        t.set_keyterms(k)?;
    }
    eprintln!("load {:.0} ms", t0.elapsed().as_secs_f32() * 1e3);
    let mut s = t.stream()?;
    s.start()?;
    let chunk = rate as usize * every_ms / 1000;
    let (mut busy, mut last) = (0f32, String::new());
    for (i, c) in pcm.chunks(chunk).enumerate() {
        s.add_audio(c, rate as i32)?;
        let t1 = Instant::now();
        let lines = s.transcribe()?;
        busy += t1.elapsed().as_secs_f32();
        let now: String = lines.iter().map(|l| format!("{}{}", l.text, if l.complete { " |" } else { " ..." })).collect::<Vec<_>>().join(" ");
        if now != last {
            eprintln!("{:5.1}s  {now}", ((i + 1) * every_ms) as f32 / 1000.0);
            last = now;
        }
    }
    let t1 = Instant::now();
    s.stop()?;
    let lines = s.transcribe()?;
    let fin = t1.elapsed().as_secs_f32();
    let audio = pcm.len() as f32 / rate as f32;
    println!("{}", lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" "));
    eprintln!("audio {audio:.1} s, streaming compute {busy:.2} s (RTF {:.2}), finalize {:.0} ms", busy / audio, fin * 1e3);
    Ok(())
}
