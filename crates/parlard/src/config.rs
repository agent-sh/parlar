//! User settings: the language, the recognizer and the voice, from
//! `$XDG_CONFIG_HOME/parlar/config.toml`. Every key is optional; without the file parlar speaks
//! and listens in English with Phonon-2 and Kokoro's af_heart voice, on this machine.
//!
//! ```toml
//! language = "es"                   # what you speak and what the agent's voice speaks
//!
//! [recognizer]
//! engine = "local"                  # or "openai"
//! model = "parakeet-tdt-0.6b-v3"    # or "phonon-2" (English), or a model directory
//! encoder = "int8"                  # int8, exact4x2 or fp32, when the model has them
//!
//! [voice]
//! engine = "local"                  # or "openai"
//! name = "kokoro_ef_dora"           # a voice libmoonshine knows for the language
//! command = ["espeak-ng", "-v", "es"]   # or speak through a program that reads text on stdin
//!
//! [openai]                          # the connection both OpenAI engines share
//! key_cmd = ["pass", "openai"]      # prints the key; else $OPENAI_API_KEY
//! ```
//!
//! `[voice.openai]` and `[recognizer.openai]` hold the model settings and may override any
//! `[openai]` connection key. README lists every key.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub language: Option<String>,
    pub recognizer: RecognizerConfig,
    pub voice: VoiceConfig,
    pub openai: Connection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecognizerConfig {
    pub engine: Option<String>,
    pub model: Option<String>,
    pub encoder: Option<String>,
    pub fallback: Option<String>,
    pub openai: OpenAiStt,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    pub engine: Option<String>,
    pub name: Option<String>,
    pub command: Vec<String>,
    pub fallback: Option<String>,
    pub openai: OpenAiTts,
}

/// Where an OpenAI-compatible API is and how to reach it. The same keys may appear in
/// `[voice.openai]` and `[recognizer.openai]` to override `[openai]` for one engine.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Connection {
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    pub key_cmd: Option<Vec<String>>,
    pub timeout_ms: Option<u64>,
    pub retry_after_s: Option<u64>,
    pub ip_family: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiTts {
    pub api: Option<String>,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub instructions: Option<String>,
    pub speed: Option<f32>,
    pub format: Option<String>,
    pub rate: Option<u32>,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    pub key_cmd: Option<Vec<String>>,
    pub timeout_ms: Option<u64>,
    pub retry_after_s: Option<u64>,
    pub ip_family: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiStt {
    pub api: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub keywords: Option<usize>,
    pub languages: Option<Vec<String>>,
    pub delay: Option<String>,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    pub key_cmd: Option<Vec<String>>,
    pub timeout_ms: Option<u64>,
    pub retry_after_s: Option<u64>,
    pub ip_family: Option<String>,
}

/// A setting's value and where it came from, for `parlard config`.
pub struct Setting {
    pub key: &'static str,
    pub value: String,
    pub from: &'static str,
}

fn setting<T: std::fmt::Debug>(key: &'static str, set: Option<T>, default: T) -> Setting {
    match set {
        Some(v) => Setting { key, value: format!("{v:?}"), from: "config.toml" },
        None => Setting { key, value: format!("{default:?}"), from: "default" },
    }
}

/// Which engine runs a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Local,
    OpenAi,
}

/// What a stage does when its API fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// Use the local engine until `retry_after_s` has passed, then try the API again.
    Local,
    /// Drop the line or the transcript and try the API again next time.
    None,
}

/// The OpenAI connection settings in use for one engine.
#[derive(Debug, Clone)]
pub struct Conn {
    pub base_url: String,
    pub key_env: String,
    pub key_cmd: Vec<String>,
    pub timeout: std::time::Duration,
    pub retry_after: std::time::Duration,
    pub ip_family: IpFamily,
}

/// Which addresses to connect to. Auto races IPv6 and IPv4 and keeps the first that connects,
/// so a network whose IPv6 route hangs costs one short race, not a stalled sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpFamily {
    Auto,
    V4,
    V6,
}

pub const OPENAI_URL: &str = "https://api.openai.com/v1";
/// Lowest time to first audio measured among OpenAI's speech models, and read every test line
/// word for word (docs/DESIGN.md, D19).
pub const TTS_MODEL: &str = "gpt-realtime-2.1-mini";
pub const TTS_VOICE: &str = "marin";
pub const TTS_STYLE: &str = "Calm, clear and warm, at an even pace.";
pub const TIMEOUT_MS: u64 = 10_000;
pub const RETRY_AFTER_S: u64 = 60;
pub const KEYWORDS: usize = 60;

/// A language parlar knows: its code in the config, libmoonshine's voice language (None when
/// libmoonshine has no voice for it), and whether the English turn rules apply.
pub struct Language {
    pub code: &'static str,
    pub tts: Option<&'static str>,
    pub english: bool,
}

/// Parakeet TDT 0.6B v3's 25 languages plus the ones libmoonshine can speak.
const LANGUAGES: &[(&str, Option<&str>)] = &[
    ("en", Some("en_us")),
    ("en-gb", Some("en_gb")),
    ("es", Some("es")),
    ("fr", Some("fr")),
    ("it", Some("it")),
    ("pt", Some("pt_br")),
    ("de", Some("de")),
    ("ru", Some("ru")),
    ("ja", Some("ja")),
    ("zh", Some("zh")),
    ("hi", Some("hi")),
    ("bg", None),
    ("hr", None),
    ("cs", None),
    ("da", None),
    ("nl", None),
    ("et", None),
    ("fi", None),
    ("el", None),
    ("hu", None),
    ("lv", None),
    ("lt", None),
    ("mt", None),
    ("pl", None),
    ("ro", None),
    ("sk", None),
    ("sl", None),
    ("sv", None),
    ("uk", None),
];

/// Languages each built-in recognizer understands.
const PHONON_LANGS: &[&str] = &["en", "en-gb"];
const PARAKEET_LANGS: &[&str] = &[
    "en", "en-gb", "es", "fr", "it", "pt", "de", "ru", "bg", "hr", "cs", "da", "nl", "et", "fi", "el", "hu", "lv",
    "lt", "mt", "pl", "ro", "sk", "sl", "sv", "uk",
];

pub const PHONON: &str = "phonon-2";
pub const PARAKEET: &str = "parakeet-tdt-0.6b-v3";

pub const TTS_SPEECH_MODEL: &str = "gpt-4o-mini-tts";
/// Measured best on accuracy and time (docs/DESIGN.md, D19): gpt-transcribe in a Realtime
/// session, which takes the audio while the user talks and so has the least left at a pause.
pub const STT_API: &str = "realtime";
pub const STT_FILE_MODEL: &str = "gpt-transcribe";
pub const STT_REALTIME_MODEL: &str = "gpt-transcribe";
pub const STT_PROMPT: &str = "A developer talking to a coding agent about the code in their repository.";
pub const STT_DELAY: &str = "low";

fn engine(v: Option<&str>) -> Engine {
    match v {
        Some("openai") => Engine::OpenAi,
        _ => Engine::Local,
    }
}

fn fallback(v: Option<&str>) -> Fallback {
    match v {
        Some("none") => Fallback::None,
        _ => Fallback::Local,
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// `$XDG_CONFIG_HOME/parlar/config.toml` (`%APPDATA%\\parlar\\config.toml` on Windows).
pub fn path() -> PathBuf {
    parlar::dirs::config().join("config.toml")
}

/// Read the settings once; later calls return the same. A broken file is an error, not a silent
/// fallback, so a typo does not quietly switch the language.
pub fn load() -> Result<&'static Config> {
    if let Some(c) = CONFIG.get() {
        return Ok(c);
    }
    let p = path();
    let c = match std::fs::read_to_string(&p) {
        Ok(text) => parse(&text).with_context(|| format!("read {}", p.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
    };
    c.check()?;
    Ok(CONFIG.get_or_init(|| c))
}

/// The settings, or the defaults when they were never loaded (tests, debug commands).
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

fn parse(text: &str) -> Result<Config> {
    Ok(toml::from_str(text)?)
}

impl Config {
    pub fn language(&self) -> Language {
        let code = self.language.as_deref().unwrap_or("en").to_ascii_lowercase().replace('_', "-");
        let (code, tts) = LANGUAGES.iter().find(|(c, _)| *c == code).copied().unwrap_or(("en", Some("en_us")));
        Language { code, tts, english: code.starts_with("en") }
    }

    /// The recognizer: the configured one, else Phonon-2 for English and Parakeet otherwise.
    pub fn recognizer(&self) -> String {
        match self.recognizer.model.as_deref() {
            Some(m) => m.to_string(),
            None if self.language().english => PHONON.into(),
            None => PARAKEET.into(),
        }
    }

    pub fn encoder_file(&self) -> Option<String> {
        self.recognizer.encoder.as_deref().map(|e| match e {
            "fp32" => "encoder-model.onnx".to_string(),
            other => format!("encoder-model.{other}.onnx"),
        })
    }

    pub fn voice_engine(&self) -> Engine {
        engine(self.voice.engine.as_deref())
    }

    pub fn recognizer_engine(&self) -> Engine {
        engine(self.recognizer.engine.as_deref())
    }

    pub fn voice_fallback(&self) -> Fallback {
        fallback(self.voice.fallback.as_deref())
    }

    pub fn recognizer_fallback(&self) -> Fallback {
        fallback(self.recognizer.fallback.as_deref())
    }

    /// The connection for the voice: `[voice.openai]` keys over `[openai]` over the defaults.
    pub fn voice_conn(&self) -> Conn {
        let v = &self.voice.openai;
        self.conn(&v.base_url, &v.key_env, &v.key_cmd, v.timeout_ms, v.retry_after_s, &v.ip_family)
    }

    pub fn recognizer_conn(&self) -> Conn {
        let r = &self.recognizer.openai;
        self.conn(&r.base_url, &r.key_env, &r.key_cmd, r.timeout_ms, r.retry_after_s, &r.ip_family)
    }

    fn conn(
        &self,
        base_url: &Option<String>,
        key_env: &Option<String>,
        key_cmd: &Option<Vec<String>>,
        timeout_ms: Option<u64>,
        retry_after_s: Option<u64>,
        ip_family: &Option<String>,
    ) -> Conn {
        let o = &self.openai;
        Conn {
            base_url: base_url.clone().or_else(|| o.base_url.clone()).unwrap_or_else(|| OPENAI_URL.into()),
            key_env: key_env.clone().or_else(|| o.key_env.clone()).unwrap_or_else(|| "OPENAI_API_KEY".into()),
            key_cmd: key_cmd.clone().or_else(|| o.key_cmd.clone()).unwrap_or_default(),
            timeout: std::time::Duration::from_millis(timeout_ms.or(o.timeout_ms).unwrap_or(TIMEOUT_MS)),
            retry_after: std::time::Duration::from_secs(retry_after_s.or(o.retry_after_s).unwrap_or(RETRY_AFTER_S)),
            ip_family: match ip_family.as_deref().or(o.ip_family.as_deref()) {
                Some("v4") => IpFamily::V4,
                Some("v6") => IpFamily::V6,
                _ => IpFamily::Auto,
            },
        }
    }

    /// The API language hints: the configured ones, else the conversation language.
    pub fn stt_languages(&self) -> Vec<String> {
        match &self.recognizer.openai.languages {
            Some(l) => l.clone(),
            None => vec![self.language().code.split('-').next().unwrap_or("en").to_string()],
        }
    }

    /// Every OpenAI setting in use for one stage and where it came from.
    pub fn openai_settings(&self, voice: bool) -> Vec<Setting> {
        let o = &self.openai;
        let (base_url, key_env, key_cmd, timeout_ms, retry_after_s, ip_family) = if voice {
            let v = &self.voice.openai;
            (&v.base_url, &v.key_env, &v.key_cmd, v.timeout_ms, v.retry_after_s, &v.ip_family)
        } else {
            let r = &self.recognizer.openai;
            (&r.base_url, &r.key_env, &r.key_cmd, r.timeout_ms, r.retry_after_s, &r.ip_family)
        };
        let section = if voice { "[voice.openai]" } else { "[recognizer.openai]" };
        // a connection key may come from the stage's own table or the shared one
        let layered =
            |key: &'static str, own: Option<String>, shared: Option<String>, default: String| match (own, shared) {
                (Some(v), _) => Setting { key, value: v, from: section },
                (None, Some(v)) => Setting { key, value: v, from: "[openai]" },
                (None, None) => Setting { key, value: default, from: "default" },
            };
        let q = |s: &Option<String>| s.as_ref().map(|v| format!("{v:?}"));
        let mut out = Vec::new();
        if voice {
            let v = &self.voice.openai;
            out.push(setting("api", v.api.clone(), self.tts_api().into()));
            out.push(setting("model", v.model.clone(), self.tts_model()));
            out.push(setting("voice", v.voice.clone(), TTS_VOICE.into()));
            out.push(setting("instructions", v.instructions.clone(), TTS_STYLE.into()));
            out.push(setting("speed", v.speed, 1.0));
            out.push(setting("format", v.format.clone(), "pcm".into()));
            out.push(setting("rate", v.rate, 24000));
        } else {
            let r = &self.recognizer.openai;
            out.push(setting("api", r.api.clone(), self.stt_api().into()));
            out.push(setting("model", r.model.clone(), self.stt_model()));
            out.push(setting("prompt", r.prompt.clone(), STT_PROMPT.into()));
            out.push(setting("keywords", r.keywords, KEYWORDS));
            out.push(setting("languages", r.languages.clone(), self.stt_languages()));
            out.push(setting("delay", r.delay.clone(), STT_DELAY.into()));
        }
        out.push(layered("base_url", q(base_url), q(&o.base_url), format!("{OPENAI_URL:?}")));
        out.push(layered("key_env", q(key_env), q(&o.key_env), "\"OPENAI_API_KEY\"".into()));
        let cmd = |c: &Option<Vec<String>>| c.as_ref().map(|c| format!("{c:?}"));
        out.push(layered("key_cmd", cmd(key_cmd), cmd(&o.key_cmd), "[] (use key_env)".into()));
        let n = |v: Option<u64>| v.map(|v| v.to_string());
        out.push(layered("timeout_ms", n(timeout_ms), n(o.timeout_ms), TIMEOUT_MS.to_string()));
        out.push(layered("retry_after_s", n(retry_after_s), n(o.retry_after_s), RETRY_AFTER_S.to_string()));
        out.push(layered("ip_family", q(ip_family), q(&o.ip_family), "\"auto\"".into()));
        out
    }

    /// "realtime" (a WebSocket session) or "speech" (`POST /audio/speech`).
    pub fn tts_api(&self) -> &str {
        self.voice.openai.api.as_deref().unwrap_or("realtime")
    }

    pub fn tts_model(&self) -> String {
        match (&self.voice.openai.model, self.tts_api()) {
            (Some(m), _) => m.clone(),
            (None, "speech") => TTS_SPEECH_MODEL.into(),
            (None, _) => TTS_MODEL.into(),
        }
    }

    /// "realtime" (a transcription session fed while the user talks) or "file"
    /// (`POST /audio/transcriptions` at each pause).
    pub fn stt_api(&self) -> &str {
        self.recognizer.openai.api.as_deref().unwrap_or(STT_API)
    }

    pub fn stt_model(&self) -> String {
        match (&self.recognizer.openai.model, self.stt_api()) {
            (Some(m), _) => m.clone(),
            (None, "file") => STT_FILE_MODEL.into(),
            (None, _) => STT_REALTIME_MODEL.into(),
        }
    }

    fn check(&self) -> Result<()> {
        if let Some(l) = self.language.as_deref() {
            let code = l.to_ascii_lowercase().replace('_', "-");
            if !LANGUAGES.iter().any(|(c, _)| *c == code) {
                let known: Vec<&str> = LANGUAGES.iter().map(|(c, _)| *c).collect();
                bail!("language {l:?} is not one parlar knows; use one of {}", known.join(", "));
            }
        }
        let lang = self.language();
        let model = self.recognizer();
        let langs = match model.as_str() {
            PHONON => Some(PHONON_LANGS),
            PARAKEET => Some(PARAKEET_LANGS),
            // a model directory: its languages are the user's business
            _ => None,
        };
        if let Some(langs) = langs
            && !langs.contains(&lang.code)
        {
            bail!("the {model} recognizer does not understand {:?}; use {PARAKEET} or another model", lang.code);
        }
        if let Some(e) = self.recognizer.encoder.as_deref() {
            let ok = crate::models::encoders(&model);
            if !ok.contains(&e) {
                bail!("encoder {e:?}: {model} has {}", ok.join(", "));
            }
        }
        for (what, v) in [("[voice] engine", &self.voice.engine), ("[recognizer] engine", &self.recognizer.engine)] {
            if let Some(v) = v.as_deref()
                && !["local", "openai"].contains(&v)
            {
                bail!("{what} {v:?}: use \"local\" or \"openai\"");
            }
        }
        for (what, v) in
            [("[voice] fallback", &self.voice.fallback), ("[recognizer] fallback", &self.recognizer.fallback)]
        {
            if let Some(v) = v.as_deref()
                && !["local", "none"].contains(&v)
            {
                bail!("{what} {v:?}: use \"local\" or \"none\"");
            }
        }
        let t = &self.voice.openai;
        if !["realtime", "speech"].contains(&self.tts_api()) {
            bail!("[voice.openai] api {:?}: use \"realtime\" or \"speech\"", self.tts_api());
        }
        if let Some(f) = t.format.as_deref() {
            let ok: &[&str] = if self.tts_api() == "speech" { &["pcm", "wav"] } else { &["pcm"] };
            if !ok.contains(&f) {
                bail!("[voice.openai] format {f:?}: the {} API here takes {}", self.tts_api(), ok.join(", "));
            }
        }
        if let Some(r) = t.rate
            && (self.tts_api() == "realtime" && r != 24000 || !(8000..=48000).contains(&r))
        {
            bail!("[voice.openai] rate {r}: realtime audio is always 24000; the speech API takes 8000 to 48000");
        }
        if let Some(s) = t.speed {
            let (lo, hi) = if self.tts_api() == "speech" { (0.25, 4.0) } else { (0.25, 1.5) };
            if !(lo..=hi).contains(&s) {
                bail!("[voice.openai] speed {s}: the {} API takes {lo} to {hi}", self.tts_api());
            }
        }
        if !["file", "realtime"].contains(&self.stt_api()) {
            bail!("[recognizer.openai] api {:?}: use \"file\" or \"realtime\"", self.stt_api());
        }
        if let Some(d) = self.recognizer.openai.delay.as_deref()
            && !["minimal", "low", "medium", "high", "xhigh"].contains(&d)
        {
            bail!("[recognizer.openai] delay {d:?}: use minimal, low, medium, high or xhigh");
        }
        for f in [&self.openai.ip_family, &t.ip_family, &self.recognizer.openai.ip_family].into_iter().flatten() {
            if !["auto", "v4", "v6"].contains(&f.as_str()) {
                bail!("ip_family {f:?}: use \"auto\", \"v4\" or \"v6\"");
            }
        }
        for t in [self.openai.timeout_ms, t.timeout_ms, self.recognizer.openai.timeout_ms].into_iter().flatten() {
            if t < 100 {
                bail!("timeout_ms {t}: use 100 or more");
            }
        }
        for c in [&self.openai.key_cmd, &t.key_cmd, &self.recognizer.openai.key_cmd].into_iter().flatten() {
            if c.is_empty() {
                bail!("key_cmd is empty; leave it out to read the key from key_env");
            }
        }
        let speaks = self.voice_engine() == Engine::OpenAi || !self.voice.command.is_empty();
        if lang.tts.is_none() && !speaks {
            bail!("libmoonshine has no voice for {:?}; set [voice] command to a program that speaks it", lang.code);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_english_with_phonon() {
        let c = parse("").unwrap();
        assert_eq!(c.language().code, "en");
        assert_eq!(c.recognizer(), PHONON);
        c.check().unwrap();
    }

    #[test]
    fn another_language_picks_parakeet_and_its_voice_language() {
        let c = parse("language = \"es\"").unwrap();
        assert_eq!(c.recognizer(), PARAKEET);
        assert_eq!(c.language().tts, Some("es"));
        assert!(!c.language().english);
        c.check().unwrap();
    }

    #[test]
    fn mismatches_are_refused_with_a_reason() {
        let err = |t: &str| parse(t).and_then(|c| c.check()).unwrap_err().to_string();
        assert!(err("language = \"es\"\n[recognizer]\nmodel = \"phonon-2\"").contains("does not understand"));
        assert!(err("language = \"pl\"").contains("no voice"));
        assert!(err("language = \"xx\"").contains("not one parlar knows"));
        assert!(err("[recognizer]\nencoder = \"int4\"").contains("encoder"));
        assert!(err("language = \"es\"\n[recognizer]\nencoder = \"exact4x2\"").contains("has int8, fp32"));
        assert!(parse("languge = \"es\"").is_err(), "unknown keys are typos");
        parse("language = \"pl\"\n[voice]\ncommand = [\"espeak-ng\", \"-v\", \"pl\"]").unwrap().check().unwrap();
        // an API voice speaks languages libmoonshine cannot
        parse("language = \"pl\"\n[voice]\nengine = \"openai\"").unwrap().check().unwrap();
        assert!(err("[voice]\nengine = \"cloud\"").contains("\"local\" or \"openai\""));
        assert!(err("[voice.openai]\napi = \"realtime\"\nformat = \"wav\"").contains("takes pcm"));
        assert!(err("[voice.openai]\nspeed = 2.0").contains("0.25 to 1.5"));
        parse("[voice.openai]\napi = \"speech\"\nspeed = 2.0\nformat = \"wav\"").unwrap().check().unwrap();
        assert!(err("[voice.openai]\nrate = 16000").contains("always 24000"));
        assert!(err("[recognizer.openai]\ndelay = \"fast\"").contains("minimal"));
        assert!(err("[openai]\nkey_cmd = []").contains("key_cmd is empty"));
        assert!(parse("[voice.openai]\nvoise = \"marin\"").is_err(), "unknown keys are typos");
    }

    #[test]
    fn openai_settings_layer_stage_over_shared_over_default() {
        let c = parse(
            "[openai]\nbase_url = \"http://localhost:8000/v1\"\ntimeout_ms = 5000\n\
             [voice]\nengine = \"openai\"\n[voice.openai]\ntimeout_ms = 3000\nvoice = \"cedar\"\n\
             [recognizer]\nengine = \"openai\"\nfallback = \"none\"",
        )
        .unwrap();
        c.check().unwrap();
        assert_eq!(c.voice_engine(), Engine::OpenAi);
        assert_eq!(c.recognizer_fallback(), Fallback::None);
        assert_eq!(c.voice_fallback(), Fallback::Local);
        let v = c.voice_conn();
        assert_eq!(v.base_url, "http://localhost:8000/v1");
        assert_eq!(v.timeout, std::time::Duration::from_millis(3000));
        assert_eq!(c.recognizer_conn().timeout, std::time::Duration::from_millis(5000));
        assert_eq!(c.recognizer_conn().key_env, "OPENAI_API_KEY");
        assert_eq!(c.recognizer_conn().ip_family, IpFamily::Auto);
        assert!(parse("[openai]\nip_family = \"v5\"").unwrap().check().is_err());
        let s = c.openai_settings(true);
        let get = |k: &str| s.iter().find(|x| x.key == k).map(|x| (x.value.clone(), x.from)).unwrap();
        assert_eq!(get("voice"), ("\"cedar\"".into(), "config.toml"));
        assert_eq!(get("model").1, "default");
        assert_eq!(get("timeout_ms"), ("3000".into(), "[voice.openai]"));
        assert_eq!(get("base_url").1, "[openai]");
        assert_eq!(get("retry_after_s").1, "default");
        // the speech API has its own default model
        let c = parse("[voice.openai]\napi = \"speech\"").unwrap();
        assert_eq!(c.tts_model(), TTS_SPEECH_MODEL);
        assert_eq!(parse("language = \"en-gb\"").unwrap().stt_languages(), vec!["en".to_string()]);
    }
}
