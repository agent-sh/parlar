//! Safe wrappers over the libmoonshine C API (header version 30000): streaming transcription,
//! streaming Kokoro synthesis, and the model download manifests.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr;

use anyhow::{Result, bail};

pub const HEADER_VERSION: i32 = 30000;
pub const ARCH_SMALL_STREAMING: u32 = 4;

mod sys {
    use super::*;

    #[repr(C)]
    pub struct Opt {
        pub name: *const c_char,
        pub value: *const c_char,
    }

    #[repr(C)]
    pub struct Word {
        pub text: *const c_char,
        pub start: f32,
        pub end: f32,
        pub confidence: f32,
    }

    #[repr(C)]
    pub struct SpeakerSpan {
        pub start_time: f32,
        pub duration: f32,
        pub speaker_id: u64,
        pub speaker_index: u32,
        pub start_char: u64,
        pub end_char: u64,
    }

    #[repr(C)]
    pub struct Line {
        pub text: *const c_char,
        pub audio_data: *const f32,
        pub audio_data_count: usize,
        pub start_time: f32,
        pub duration: f32,
        pub id: u64,
        pub is_complete: i8,
        pub is_updated: i8,
        pub is_new: i8,
        pub has_text_changed: i8,
        pub have_speakers_changed: i8,
        pub speaker_spans: *const SpeakerSpan,
        pub speaker_span_count: u64,
        pub last_transcription_latency_ms: u32,
        pub words: *const Word,
        pub word_count: u64,
    }

    #[repr(C)]
    pub struct Transcript {
        pub lines: *mut Line,
        pub line_count: u64,
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

    unsafe extern "C" {
        pub fn moonshine_get_version() -> i32;
        pub fn moonshine_error_to_string(error: i32) -> *const c_char;

        pub fn moonshine_load_transcriber_from_files(
            path: *const c_char,
            model_arch: u32,
            options: *const Opt,
            options_count: u64,
            version: i32,
        ) -> i32;
        pub fn moonshine_free_transcriber(handle: i32);
        pub fn moonshine_transcriber_set_keyterms(handle: i32, keyterms: *const c_char) -> i32;

        pub fn moonshine_create_stream(handle: i32, flags: u32) -> i32;
        pub fn moonshine_free_stream(handle: i32, stream: i32) -> i32;
        pub fn moonshine_start_stream(handle: i32, stream: i32) -> i32;
        pub fn moonshine_stop_stream(handle: i32, stream: i32) -> i32;
        pub fn moonshine_transcribe_add_audio_to_stream(
            handle: i32,
            stream: i32,
            audio: *const f32,
            len: u64,
            sample_rate: i32,
            flags: u32,
        ) -> i32;
        pub fn moonshine_transcribe_stream(
            handle: i32,
            stream: i32,
            flags: u32,
            out: *mut *mut Transcript,
        ) -> i32;

        pub fn moonshine_create_tts_synthesizer_from_files(
            language: *const c_char,
            filenames: *const *const c_char,
            filenames_count: u64,
            options: *const Opt,
            options_count: u64,
            version: i32,
        ) -> i32;
        pub fn moonshine_free_tts_synthesizer(handle: i32);
        pub fn moonshine_tts_push_text(handle: i32, text: *const c_char) -> i32;
        pub fn moonshine_tts_flush(handle: i32) -> i32;
        pub fn moonshine_tts_end_input(handle: i32) -> i32;
        pub fn moonshine_tts_cancel(handle: i32) -> i32;
        pub fn moonshine_tts_next_chunk(handle: i32, flags: u32, out: *mut *const Chunk) -> i32;

        pub fn moonshine_get_stt_dependencies(
            language: *const c_char,
            options: *const Opt,
            options_count: u64,
            out: *mut *mut c_char,
        ) -> i32;
        pub fn moonshine_get_tts_dependencies(
            languages: *const c_char,
            options: *const Opt,
            options_count: u64,
            out: *mut *mut c_char,
        ) -> i32;
    }
}

fn check(code: i32) -> Result<i32> {
    if code < 0 {
        // SAFETY: returns a pointer to a static string for any code
        let msg = unsafe { CStr::from_ptr(sys::moonshine_error_to_string(code)) };
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

pub fn version() -> i32 {
    // SAFETY: no arguments, no state
    unsafe { sys::moonshine_get_version() }
}

fn manifest(
    f: unsafe extern "C" fn(*const c_char, *const sys::Opt, u64, *mut *mut c_char) -> i32,
    lang: &str,
    opts: &[(&str, &str)],
) -> Result<String> {
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

/// JSON download manifest for a speech-to-text model.
pub fn stt_manifest(lang: &str, opts: &[(&str, &str)]) -> Result<String> {
    manifest(sys::moonshine_get_stt_dependencies, lang, opts)
}

/// JSON download manifest for G2P plus a TTS vocoder.
pub fn tts_manifest(lang: &str, opts: &[(&str, &str)]) -> Result<String> {
    manifest(sys::moonshine_get_tts_dependencies, lang, opts)
}

pub struct Transcriber {
    h: i32,
}

// SAFETY: the library serializes calls per handle; we only move handles across threads
unsafe impl Send for Transcriber {}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub id: u64,
    pub text: String,
    pub start: f32,
    pub duration: f32,
    pub complete: bool,
    pub text_changed: bool,
    pub latency_ms: u32,
}

impl Transcriber {
    pub fn load(dir: &Path, arch: u32, opts: &[(&str, &str)]) -> Result<Transcriber> {
        let p = CString::new(dir.to_string_lossy().as_bytes())?;
        let o = Opts::new(opts)?;
        // SAFETY: valid path and options
        let h = check(unsafe {
            sys::moonshine_load_transcriber_from_files(p.as_ptr(), arch, o.ptr(), o.len(), HEADER_VERSION)
        })?;
        Ok(Transcriber { h })
    }

    /// Replace the biasing vocabulary, comma separated. Takes effect mid-stream.
    pub fn set_keyterms(&self, terms: &str) -> Result<()> {
        let t = CString::new(terms)?;
        // SAFETY: valid handle and string
        check(unsafe { sys::moonshine_transcriber_set_keyterms(self.h, t.as_ptr()) })?;
        Ok(())
    }

    pub fn stream(&self) -> Result<Stream<'_>> {
        // SAFETY: valid handle
        let s = check(unsafe { sys::moonshine_create_stream(self.h, 0) })?;
        Ok(Stream { t: self, s })
    }
}

impl Drop for Transcriber {
    fn drop(&mut self) {
        // SAFETY: handle owned by us, streams borrow us so they are gone
        unsafe { sys::moonshine_free_transcriber(self.h) }
    }
}

pub struct Stream<'a> {
    t: &'a Transcriber,
    s: i32,
}

impl Stream<'_> {
    pub fn start(&mut self) -> Result<()> {
        // SAFETY: valid handles
        check(unsafe { sys::moonshine_start_stream(self.t.h, self.s) })?;
        Ok(())
    }

    /// Finalize the current lines.
    pub fn stop(&mut self) -> Result<()> {
        // SAFETY: valid handles
        check(unsafe { sys::moonshine_stop_stream(self.t.h, self.s) })?;
        Ok(())
    }

    pub fn add_audio(&mut self, pcm: &[f32], sample_rate: i32) -> Result<()> {
        // SAFETY: slice is valid for its length
        check(unsafe {
            sys::moonshine_transcribe_add_audio_to_stream(
                self.t.h,
                self.s,
                pcm.as_ptr(),
                pcm.len() as u64,
                sample_rate,
                0,
            )
        })?;
        Ok(())
    }

    /// Transcribe everything added so far. Lines are only ever appended; the last may be
    /// incomplete.
    pub fn transcribe(&mut self) -> Result<Vec<Line>> {
        let mut out: *mut sys::Transcript = ptr::null_mut();
        // SAFETY: valid handles; the transcript stays valid until the next call on the handle
        check(unsafe { sys::moonshine_transcribe_stream(self.t.h, self.s, 0, &mut out) })?;
        if out.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: out points at a transcript owned by the library
        let t = unsafe { &*out };
        let lines = if t.lines.is_null() {
            &[][..]
        } else {
            // SAFETY: line_count lines follow lines
            unsafe { std::slice::from_raw_parts(t.lines, t.line_count as usize) }
        };
        Ok(lines
            .iter()
            .map(|l| Line {
                id: l.id,
                text: cstr(l.text),
                start: l.start_time,
                duration: l.duration,
                complete: l.is_complete != 0,
                text_changed: l.has_text_changed != 0,
                latency_ms: l.last_transcription_latency_ms,
            })
            .collect())
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        // SAFETY: stream owned by us
        unsafe { sys::moonshine_free_stream(self.t.h, self.s) };
    }
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
        let mut pairs = vec![("g2p_root", root.as_str()), ("voice", voice)];
        pairs.extend_from_slice(extra);
        let o = Opts::new(&pairs)?;
        // SAFETY: valid strings; no explicit file list, assets resolve under g2p_root
        let h = check(unsafe {
            sys::moonshine_create_tts_synthesizer_from_files(
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

    pub fn push(&self, text: &str) -> Result<()> {
        let t = CString::new(text)?;
        // SAFETY: valid handle and string
        check(unsafe { sys::moonshine_tts_push_text(self.h, t.as_ptr()) })?;
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        // SAFETY: valid handle
        check(unsafe { sys::moonshine_tts_flush(self.h) })?;
        Ok(())
    }

    pub fn end_input(&self) -> Result<()> {
        // SAFETY: valid handle
        check(unsafe { sys::moonshine_tts_end_input(self.h) })?;
        Ok(())
    }

    /// Barge-in: drop the reply in flight.
    pub fn cancel(&self) -> Result<()> {
        // SAFETY: valid handle, safe when idle
        check(unsafe { sys::moonshine_tts_cancel(self.h) })?;
        Ok(())
    }

    pub fn next(&self) -> Result<Next> {
        let mut out: *const sys::Chunk = ptr::null();
        // SAFETY: valid handle; the chunk is valid until the next call, so we copy it
        let code = unsafe { sys::moonshine_tts_next_chunk(self.h, 0, &mut out) };
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
        // SAFETY: handle owned by us
        unsafe { sys::moonshine_free_tts_synthesizer(self.h) }
    }
}
