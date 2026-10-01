//! User settings: the language, the recognizer and the voice, from
//! `$XDG_CONFIG_HOME/parlar/config.toml`. Every key is optional; without the file parlar speaks
//! and listens in English with Phonon-2 and Kokoro's af_heart voice.
//!
//! ```toml
//! language = "es"                   # what you speak and what the agent's voice speaks
//!
//! [recognizer]
//! model = "parakeet-tdt-0.6b-v3"    # or "phonon-2" (English), or a model directory
//! encoder = "int8"                  # int8, exact4x2 or fp32, when the model has them
//!
//! [voice]
//! name = "kokoro_ef_dora"           # a voice libmoonshine knows for the language
//! command = ["espeak-ng", "-v", "es"]   # or speak through a program that reads text on stdin
//! ```

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
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecognizerConfig {
    pub model: Option<String>,
    pub encoder: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceConfig {
    pub name: Option<String>,
    pub command: Vec<String>,
}

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

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".config"));
    base.join("parlar/config.toml")
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
        if lang.tts.is_none() && self.voice.command.is_empty() {
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
    }
}
