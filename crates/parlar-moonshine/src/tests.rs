//! Exercise the safe wrapper against C ABI functions without loading a model or daemon.
use super::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

type Pause = (mpsc::Sender<()>, mpsc::Receiver<()>);
thread_local! {
    static COPY_PAUSE: RefCell<Option<Pause>> = const { RefCell::new(None) };
    static ENTERED: RefCell<Option<mpsc::Sender<()>>> = const { RefCell::new(None) };
}

pub(super) fn before_chunk_copy() {
    COPY_PAUSE.with(|pause| {
        if let Some((entered, resume)) = pause.borrow_mut().take() {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    });
}

fn entered() {
    ENTERED.with(|entered| {
        if let Some(tx) = entered.borrow().as_ref() {
            tx.send(()).unwrap();
        }
    });
}

unsafe extern "C" fn version() -> i32 {
    HEADER_VERSION
}
unsafe extern "C" fn error(_: i32) -> *const c_char {
    c"test error".as_ptr()
}
unsafe extern "C" fn create(
    _: *const c_char,
    _: *const *const c_char,
    _: u64,
    _: *const sys::Opt,
    _: u64,
    _: i32,
) -> i32 {
    static HANDLE: AtomicI32 = AtomicI32::new(6);
    HANDLE.fetch_add(1, Ordering::Relaxed)
}
unsafe extern "C" fn free(_: i32) {
    entered();
}
unsafe extern "C" fn synthesize(
    _: i32,
    _: *const c_char,
    _: *const sys::Opt,
    _: u64,
    audio: *mut *mut f32,
    size: *mut u64,
    rate: *mut i32,
) -> i32 {
    entered();
    // SAFETY: the wrapper supplies valid output slots. No allocation is needed for empty audio.
    unsafe {
        *audio = ptr::null_mut();
        *size = 0;
        *rate = 24_000;
    }
    0
}
unsafe extern "C" fn json(_: *const c_char, _: *const c_char, _: *const sys::Opt, _: u64, _: *mut *mut c_char) -> i32 {
    -1
}
unsafe extern "C" fn manifest(_: *const c_char, _: *const sys::Opt, _: u64, _: *mut *mut c_char) -> i32 {
    -1
}
unsafe extern "C" fn free_buffer(_: *mut c_void) {}
unsafe extern "C" fn push(_: i32, _: *const c_char) -> i32 {
    entered();
    0
}
unsafe extern "C" fn mutate(_: i32) -> i32 {
    entered();
    0
}
unsafe extern "C" fn next(h: i32, _: u32, out: *mut *const sys::Chunk) -> i32 {
    entered();
    static PCM: [f32; 2] = [0.25, -0.5];
    static CHUNK: OnceLock<usize> = OnceLock::new();
    let chunk = *CHUNK.get_or_init(|| {
        Box::into_raw(Box::new(sys::Chunk {
            audio_data: PCM.as_ptr(),
            audio_data_count: 2,
            sample_rate: 24_000,
            text: c"hello".as_ptr(),
            utterance_id: 7,
            is_final: 1,
        })) as usize
    }) as *const sys::Chunk;
    // SAFETY: the wrapper supplies a valid output slot; the immutable fixture lives forever.
    unsafe {
        *out = if h == 0 { ptr::null() } else { chunk };
    }
    match h {
        2 => sys::TTS_NEED_TEXT,
        3 => sys::TTS_END_OF_STREAM,
        4 => sys::TTS_CANCELLED,
        5 => -1,
        _ => 0,
    }
}

fn install_api() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        assert!(
            API.set(sys::Api {
                _lib: libloading::os::unix::Library::this().into(),
                moonshine_get_version: version,
                moonshine_error_to_string: error,
                moonshine_create_tts_synthesizer_from_files: create,
                moonshine_free_tts_synthesizer: free,
                moonshine_text_to_speech: synthesize,
                moonshine_tts_split_utterances: json,
                moonshine_free_buffer: free_buffer,
                moonshine_tts_push_text: push,
                moonshine_tts_flush: mutate,
                moonshine_tts_end_input: mutate,
                moonshine_tts_cancel: mutate,
                moonshine_tts_next_chunk: next,
                moonshine_get_tts_dependencies: manifest,
            })
            .is_ok()
        );
    });
}

fn audio(result: Next) {
    let Next::Audio { pcm, sample_rate, text, utterance, last } = result else { panic!("expected audio") };
    assert_eq!(pcm, [0.25, -0.5]);
    assert_eq!(sample_rate, 24_000);
    assert_eq!(text, "hello");
    assert_eq!(utterance, 7);
    assert!(last);
}

#[test]
fn borrowed_chunk_is_copied_before_any_other_handle_call() {
    install_api();
    for operation in ["next", "push", "flush", "end_input", "cancel", "synthesize"] {
        let tts = Arc::new(Tts::load(Path::new("unused"), "en", "", &[]).unwrap());
        let (paused_tx, paused_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let reader = {
            let tts = Arc::clone(&tts);
            thread::spawn(move || {
                COPY_PAUSE.with(|pause| *pause.borrow_mut() = Some((paused_tx, resume_rx)));
                tts.next().unwrap()
            })
        };
        // Stop precisely after the native call has returned, before Rust reads any borrowed data.
        paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A separate synthesizer must remain usable while this one's chunk is borrowed.
        Tts::load(Path::new("unused"), "en", "", &[]).unwrap().push("independent").unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            ENTERED.with(|entered| *entered.borrow_mut() = Some(entered_tx));
            started_tx.send(()).unwrap();
            match operation {
                "next" => audio(tts.next().unwrap()),
                "push" => tts.push("hello").unwrap(),
                "flush" => tts.flush().unwrap(),
                "end_input" => tts.end_input().unwrap(),
                "cancel" => tts.cancel().unwrap(),
                "synthesize" => assert_eq!(tts.synthesize("hello").unwrap(), (vec![], 24_000)),
                _ => unreachable!(),
            }
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let overlapped = entered_rx.recv_timeout(Duration::from_millis(50)).is_ok();
        // Always release and join before asserting, so the unpatched regression also exits.
        resume_tx.send(()).unwrap();
        audio(reader.join().unwrap());
        writer.join().unwrap();
        assert!(!overlapped, "{operation} entered native code while a borrowed chunk was being copied");
    }
}

#[test]
fn statuses_errors_and_poisoned_handle_keep_their_contract() {
    install_api();
    for (h, expected) in [(2, "need"), (3, "end"), (4, "cancelled"), (0, "null"), (5, "error")] {
        let tts = Tts { h: Mutex::new(h) };
        match expected {
            "need" => assert!(matches!(tts.next().unwrap(), Next::NeedText)),
            "end" => assert!(matches!(tts.next().unwrap(), Next::End)),
            "cancelled" => assert!(matches!(tts.next().unwrap(), Next::Cancelled)),
            "null" => assert!(tts.next().err().unwrap().to_string().contains("null chunk")),
            "error" => assert!(tts.next().err().unwrap().to_string().contains("test error")),
            _ => unreachable!(),
        }
    }
    let tts = Arc::new(Tts { h: Mutex::new(1) });
    let poison = Arc::clone(&tts);
    assert!(
        thread::spawn(move || {
            let _guard = poison.h.lock().unwrap();
            panic!("poison");
        })
        .join()
        .is_err()
    );
    assert!(tts.next().err().unwrap().to_string().contains("poisoned"));
    assert!(tts.push("hello").is_err());
    assert!(tts.flush().is_err());
    assert!(tts.end_input().is_err());
    assert!(tts.cancel().is_err());
    assert!(tts.synthesize("hello").is_err());
    let (tx, rx) = mpsc::channel();
    ENTERED.with(|entered| *entered.borrow_mut() = Some(tx));
    drop(tts);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    ENTERED.with(|entered| *entered.borrow_mut() = None);
}
