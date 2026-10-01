//! Safe wrappers over the libmoonshine C API (header version 30000): Kokoro synthesis, sentence
//! splitting and the voice download manifest.
//!
//! The library is loaded at run time with [`open`], so nothing links against it and a binary
//! installed anywhere finds it wherever the caller keeps it.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};

pub const HEADER_VERSION: i32 = 30000;

mod sys {
    use super::*;

    #[repr(C)]
    pub struct Opt {
        pub name: *const c_char,
        pub value: *const c_char,
    }

    #[repr(C)]
    pub struct Chunk {
        pub audio_data: *const f32,
        pub audio_data_count: u64,
        pub sample_rate: i32,
        pub text: *const c_char,
        pub utterance_id: u64,
        pub is_final: i8,
    }

    pub const TTS_NEED_TEXT: i32 = 1;
    pub const TTS_END_OF_STREAM: i32 = 2;
    pub const TTS_CANCELLED: i32 = 3;

    type Opts = *const Opt;

    macro_rules! api {
        ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
            pub struct Api {
                _lib: libloading::Library,
                $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
            }

            impl Api {
                pub fn load(path: &Path) -> Result<Api> {
                    // SAFETY: libmoonshine runs no unsound initializers; the symbols are copied
                    // out as plain fn pointers and the library lives as long as they do
                    unsafe {
                        let lib = libloading::Library::new(path)?;
                        $(let $name = *lib.get(concat!(stringify!($name), "\0").as_bytes())?;)*
                        Ok(Api { _lib: lib, $($name,)* })
                    }
                }
            }
        };
    }

    api! {
        moonshine_get_version: fn() -> i32;
        moonshine_error_to_string: fn(i32) -> *const c_char;
        moonshine_create_tts_synthesizer_from_files: fn(*const c_char, *const *const c_char, u64, Opts, u64, i32) -> i32;
        moonshine_free_tts_synthesizer: fn(i32);
        moonshine_text_to_speech: fn(i32, *const c_char, Opts, u64, *mut *mut f32, *mut u64, *mut i32) -> i32;
        moonshine_tts_split_utterances: fn(*const c_char, *const c_char, Opts, u64, *mut *mut c_char) -> i32;
        moonshine_free_buffer: fn(*mut c_void);
        moonshine_tts_push_text: fn(i32, *const c_char) -> i32;
        moonshine_tts_flush: fn(i32) -> i32;
        moonshine_tts_end_input: fn(i32) -> i32;
        moonshine_tts_cancel: fn(i32) -> i32;
        moonshine_tts_next_chunk: fn(i32, u32, *mut *const Chunk) -> i32;
        moonshine_get_tts_dependencies: fn(*const c_char, Opts, u64, *mut *mut c_char) -> i32;
    }
}

static API: OnceLock<sys::Api> = OnceLock::new();

/// Load libmoonshine from `path` (libmoonshine.so; the ONNX Runtime it needs sits next to it).
/// Later calls are no-ops.
pub fn open(path: &Path) -> Result<()> {
    if API.get().is_some() {
        return Ok(());
    }
    let api = sys::Api::load(path).with_context(|| format!("load {}", path.display()))?;
    let _ = API.set(api);
    Ok(())
}

fn api() -> Result<&'static sys::Api> {
    API.get().ok_or_else(|| anyhow!("libmoonshine is not loaded"))
}

fn check(code: i32) -> Result<i32> {
    if code < 0 {
        // SAFETY: returns a pointer to a static string for any code
        let msg = unsafe { CStr::from_ptr((api()?.moonshine_error_to_string)(code)) };
        bail!("moonshine error {code}: {}", msg.to_string_lossy());
    }
    Ok(code)
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the library hands out NUL-terminated UTF-8 strings
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Owned option list; keeps the C strings alive for the duration of a call.
struct Opts {
    _keep: Vec<CString>,
    raw: Vec<sys::Opt>,
}

impl Opts {
    fn new(pairs: &[(&str, &str)]) -> Result<Opts> {
        let mut keep = Vec::new();
        let mut raw = Vec::new();
        for (k, v) in pairs {
            let k = CString::new(*k)?;
            let v = CString::new(*v)?;
            raw.push(sys::Opt { name: k.as_ptr(), value: v.as_ptr() });
            keep.push(k);
            keep.push(v);
        }
        Ok(Opts { _keep: keep, raw })
    }
    fn ptr(&self) -> *const sys::Opt {
        if self.raw.is_empty() { ptr::null() } else { self.raw.as_ptr() }
    }
    fn len(&self) -> u64 {
        self.raw.len() as u64
    }
}

pub fn version() -> Result<i32> {
    // SAFETY: no arguments, no state
    Ok(unsafe { (api()?.moonshine_get_version)() })
}

/// JSON download manifest for G2P plus a TTS vocoder.
pub fn tts_manifest(lang: &str, opts: &[(&str, &str)]) -> Result<String> {
    let f = api()?.moonshine_get_tts_dependencies;
    let lang = CString::new(lang)?;
    let o = Opts::new(opts)?;
    let mut out: *mut c_char = ptr::null_mut();
    // SAFETY: valid strings and option array; out receives a malloc'd buffer we free
    check(unsafe { f(lang.as_ptr(), o.ptr(), o.len(), &mut out) })?;
    let s = cstr(out);
    // SAFETY: the manifest buffers are allocated with malloc
    unsafe { libc_free(out as *mut c_void) };
    Ok(s)
}

unsafe extern "C" {
    #[link_name = "free"]
    fn libc_free(p: *mut c_void);
}

/// Split a passage into the sentences a synthesizer speaks one at a time.
pub fn split_utterances(lang: &str, text: &str) -> Result<Vec<String>> {
    let l = CString::new(lang)?;
    let t = CString::new(text)?;
    let mut out: *mut c_char = ptr::null_mut();
    // SAFETY: valid strings; out receives a buffer released with moonshine_free_buffer
    check(unsafe { (api()?.moonshine_tts_split_utterances)(l.as_ptr(), t.as_ptr(), ptr::null(), 0, &mut out) })?;
    let json = cstr(out);
    // SAFETY: documented to be released with moonshine_free_buffer
    unsafe { (api()?.moonshine_free_buffer)(out as *mut c_void) };
    Ok(parse_string_array(&json))
}

/// The JSON array of strings the splitter returns, without pulling in a JSON crate.
fn parse_string_array(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut in_str, mut esc) = (false, false);
    for c in json.chars() {
        if !in_str {
            if c == '"' {
                in_str = true;
                cur.clear();
            }
            continue;
        }
        if esc {
            cur.push(match c {
                'n' => '\n',
                't' => '\t',
                other => other,
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            in_str = false;
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out
}

pub struct Tts {
    h: i32,
}

// SAFETY: calls on one synthesizer are serialized by the library
unsafe impl Send for Tts {}

pub enum Next {
    Audio { pcm: Vec<f32>, sample_rate: i32, text: String, utterance: u64, last: bool },
    NeedText,
    End,
    Cancelled,
}

impl Tts {
    /// `root` holds the downloaded G2P and vocoder assets; `voice` like `kokoro_af_heart`.
    pub fn load(root: &Path, lang: &str, voice: &str, extra: &[(&str, &str)]) -> Result<Tts> {
        let l = CString::new(lang)?;
        let root = root.to_string_lossy().into_owned();
        let mut pairs = vec![("g2p_root", root.as_str())];
        // empty: libmoonshine's default voice for the language
        if !voice.is_empty() {
            pairs.push(("voice", voice));
        }
        pairs.extend_from_slice(extra);
        let o = Opts::new(&pairs)?;
        // SAFETY: valid strings; no explicit file list, assets resolve under g2p_root
        let h = check(unsafe {
            (api()?.moonshine_create_tts_synthesizer_from_files)(
                l.as_ptr(),
                ptr::null(),
                0,
                o.ptr(),
                o.len(),
                HEADER_VERSION,
            )
        })?;
        Ok(Tts { h })
    }

    /// Synthesize a whole text in one call. Slower to first audio than streaming, but with the
    /// prosody of the full phrase and no seams inside it.
    pub fn synthesize(&self, text: &str) -> Result<(Vec<f32>, i32)> {
        let t = CString::new(text)?;
        let (mut audio, mut size, mut rate): (*mut f32, u64, i32) = (ptr::null_mut(), 0, 0);
        // SAFETY: valid handle and string; the buffer is malloc'd by the library and freed below
        check(unsafe {
            (api()?.moonshine_text_to_speech)(self.h, t.as_ptr(), ptr::null(), 0, &mut audio, &mut size, &mut rate)
        })?;
        if audio.is_null() {
            return Ok((Vec::new(), rate));
        }
        // SAFETY: size samples follow audio
        let pcm = unsafe { std::slice::from_raw_parts(audio, size as usize) }.to_vec();
        // SAFETY: allocated with malloc by the library
        unsafe { libc_free(audio as *mut c_void) };
        Ok((pcm, rate))
    }

    pub fn push(&self, text: &str) -> Result<()> {
        let t = CString::new(text)?;
        // SAFETY: valid handle and string
        check(unsafe { (api()?.moonshine_tts_push_text)(self.h, t.as_ptr()) })?;
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        // SAFETY: valid handle
        check(unsafe { (api()?.moonshine_tts_flush)(self.h) })?;
        Ok(())
    }

    pub fn end_input(&self) -> Result<()> {
        // SAFETY: valid handle
        check(unsafe { (api()?.moonshine_tts_end_input)(self.h) })?;
        Ok(())
    }

    /// Barge-in: drop the reply in flight.
    pub fn cancel(&self) -> Result<()> {
        // SAFETY: valid handle, safe when idle
        check(unsafe { (api()?.moonshine_tts_cancel)(self.h) })?;
        Ok(())
    }

    pub fn next(&self) -> Result<Next> {
        let mut out: *const sys::Chunk = ptr::null();
        // SAFETY: valid handle; the chunk is valid until the next call, so we copy it
        let code = unsafe { (api()?.moonshine_tts_next_chunk)(self.h, 0, &mut out) };
        match code {
            0 => {
                // SAFETY: success guarantees a non-null chunk
                let c = unsafe { &*out };
                // SAFETY: audio_data_count samples follow audio_data
                let pcm = unsafe { std::slice::from_raw_parts(c.audio_data, c.audio_data_count as usize) };
                Ok(Next::Audio {
                    pcm: pcm.to_vec(),
                    sample_rate: c.sample_rate,
                    text: cstr(c.text),
                    utterance: c.utterance_id,
                    last: c.is_final != 0,
                })
            }
            sys::TTS_NEED_TEXT => Ok(Next::NeedText),
            sys::TTS_END_OF_STREAM => Ok(Next::End),
            sys::TTS_CANCELLED => Ok(Next::Cancelled),
            e => {
                check(e)?;
                bail!("unexpected tts status {e}")
            }
        }
    }
}

impl Drop for Tts {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            // SAFETY: handle owned by us
            unsafe { (a.moonshine_free_tts_synthesizer)(self.h) }
        }
    }
}
