//! Speech through an OpenAI-compatible API: the voice over a Realtime session (or the speech
//! endpoint), and the recognizer through the transcription endpoint (or a Realtime transcription
//! session). Both run on blocking threads, like the local engines they stand in for.
//!
//! The key comes from `key_cmd` or the `key_env` variable and lives only in memory. It is never
//! logged; error text from the API is scrubbed of anything that looks like a key.

use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::config::{Conn, IpFamily};

/// Why a call failed, for the fallback log line.
#[derive(Debug)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(401) => write!(f, "the API refused the key (401)"),
            Some(429) => write!(f, "rate limited or out of credit (429): {}", self.message),
            Some(s) => write!(f, "HTTP {s}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ApiError {}

fn api_err(status: Option<u16>, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ApiError { status, message: scrub(&message.into()) })
}

/// Drop anything shaped like an API key from text that may be logged. A 401 body quotes part of
/// the key it refused.
pub fn scrub(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for w in text.split_whitespace() {
        let bare = w.trim_matches(|c: char| !c.is_alphanumeric());
        // OpenAI keys start with sk-; other providers' are long mixed-case tokens. Model names
        // ("gpt-4o-mini-transcribe-2025-12-15") are lowercase and stay.
        let keyish = bare.starts_with("sk-")
            || (bare.len() >= 32
                && bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && bare.chars().any(|c| c.is_ascii_digit())
                && bare.chars().any(|c| c.is_ascii_uppercase())
                && bare.chars().any(|c| c.is_ascii_lowercase()));
        out.push(if keyish { "[key]".into() } else { w.into() });
    }
    let s = out.join(" ");
    if s.len() > 300 { format!("{}...", &s[..s.char_indices().nth(300).map(|(i, _)| i).unwrap_or(s.len())]) } else { s }
}

/// The API's own message from an error body, else the body itself.
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.trim().to_string())
}

/// One OpenAI-compatible endpoint: its key, the address family that reaches it, and when it may
/// be tried again after a failure.
pub struct Client {
    pub conn: Conn,
    key: Mutex<Option<String>>,
    /// The family that connected, once a race (or the config) settled it.
    family: Mutex<Option<IpFamily>>,
    agent: Mutex<Option<ureq::Agent>>,
    down_until: Mutex<Option<Instant>>,
    /// The connection was opened ahead of use (and has not failed since).
    warmed: std::sync::atomic::AtomicBool,
    /// What the stage is called in log lines.
    pub what: &'static str,
}

impl Client {
    pub fn new(conn: Conn, what: &'static str) -> Client {
        let family = (conn.ip_family != IpFamily::Auto).then_some(conn.ip_family);
        Client {
            conn,
            key: Mutex::default(),
            family: Mutex::new(family),
            agent: Mutex::default(),
            down_until: Mutex::default(),
            warmed: Default::default(),
            what,
        }
    }

    /// False while a recent failure keeps the stage on its local engine.
    pub fn available(&self) -> bool {
        self.down_until.lock().unwrap().is_none_or(|t| Instant::now() >= t)
    }

    /// Note a failure: the API is not tried again for `retry_after`.
    pub fn failed(&self, e: &anyhow::Error) {
        *self.down_until.lock().unwrap() = Some(Instant::now() + self.conn.retry_after);
        self.warmed.store(false, std::sync::atomic::Ordering::SeqCst);
        if matches!(e.downcast_ref::<ApiError>(), Some(ApiError { status: Some(401), .. })) {
            // the key may have been rotated: read it again next time
            *self.key.lock().unwrap() = None;
        }
        if e.downcast_ref::<ApiError>().is_none_or(|a| a.status.is_none()) {
            // a network failure: race the families again next time
            if self.conn.ip_family == IpFamily::Auto {
                *self.family.lock().unwrap() = None;
            }
            *self.agent.lock().unwrap() = None;
        }
    }

    pub fn recovered(&self) {
        *self.down_until.lock().unwrap() = None;
    }

    /// The API key, read once from `key_cmd` or `key_env`.
    fn key(&self) -> Result<String> {
        let mut k = self.key.lock().unwrap();
        if let Some(k) = k.as_ref() {
            return Ok(k.clone());
        }
        let key = if let Some((prog, args)) = self.conn.key_cmd.split_first() {
            let out = std::process::Command::new(prog)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .with_context(|| format!("run key_cmd {prog}"))?;
            if !out.status.success() {
                bail!("key_cmd {prog} exited with {}", out.status);
            }
            String::from_utf8(out.stdout).context("key_cmd printed something that is not text")?.trim().to_string()
        } else {
            std::env::var(&self.conn.key_env).unwrap_or_default().trim().to_string()
        };
        if key.is_empty() {
            bail!("no API key: set ${} or key_cmd in [openai]", self.conn.key_env);
        }
        *k = Some(key.clone());
        Ok(key)
    }

    fn auth(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.key()?))
    }

    fn host(&self) -> Result<(String, u16, bool)> {
        split_url(&self.conn.base_url)
    }

    /// The address family to use, racing IPv6 against IPv4 the first time when set to auto.
    fn family(&self) -> Result<IpFamily> {
        if let Some(f) = *self.family.lock().unwrap() {
            return Ok(f);
        }
        let (host, port, _) = self.host()?;
        let t0 = Instant::now();
        let s = race(&host, port, IpFamily::Auto, self.connect_timeout())?;
        let f = if s.peer_addr()?.is_ipv4() { IpFamily::V4 } else { IpFamily::V6 };
        eprintln!(
            "{}: {host} reached over {} in {:.0} ms",
            self.what,
            if f == IpFamily::V4 { "IPv4" } else { "IPv6" },
            t0.elapsed().as_secs_f32() * 1e3
        );
        *self.family.lock().unwrap() = Some(f);
        Ok(f)
    }

    /// Short enough that a dead route falls over to the other family well inside one sentence.
    fn connect_timeout(&self) -> Duration {
        self.conn.timeout.min(Duration::from_secs(3))
    }

    fn agent(&self) -> Result<ureq::Agent> {
        let fam = self.family()?;
        let mut a = self.agent.lock().unwrap();
        if let Some(a) = a.as_ref() {
            return Ok(a.clone());
        }
        let cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .ip_family(match fam {
                IpFamily::V6 => ureq::config::IpFamily::Ipv6Only,
                _ => ureq::config::IpFamily::Ipv4Only,
            })
            .timeout_connect(Some(self.connect_timeout()))
            .timeout_recv_response(Some(self.conn.timeout))
            // a long line streams for a while; only a stalled one is cut
            .timeout_recv_body(Some(self.conn.timeout * 6))
            .user_agent(concat!("parlar/", env!("CARGO_PKG_VERSION")))
            .build();
        let agent: ureq::Agent = cfg.into();
        *a = Some(agent.clone());
        Ok(agent)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.conn.base_url.trim_end_matches('/'), path.trim_start_matches('/'))
    }

    /// Open the connection now (DNS, the family race, TLS), so the first line does not pay for
    /// it. Errors are left for the first real call to report.
    pub fn warm(&self) {
        if self.warmed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if let Ok(agent) = self.agent() {
            let _ = agent.get(&self.url("models")).header("Authorization", &self.auth().unwrap_or_default()).call();
        }
    }

    /// POST JSON; the response body as a reader, or an error carrying the API's message.
    fn post_json(&self, path: &str, body: &Value) -> Result<ureq::BodyReader<'static>> {
        let agent = self.agent()?;
        let r = agent
            .post(&self.url(path))
            .header("Authorization", &self.auth()?)
            .content_type("application/json")
            .send(body.to_string().as_bytes())
            .map_err(|e| api_err(None, e.to_string()))?;
        let status = r.status().as_u16();
        let mut body = r.into_body();
        if status != 200 {
            let text = body.read_to_string().unwrap_or_default();
            return Err(api_err(Some(status), error_message(&text)));
        }
        Ok(body.into_reader())
    }

    fn post_multipart(&self, path: &str, boundary: &str, body: Vec<u8>) -> Result<String> {
        let agent = self.agent()?;
        let r = agent
            .post(&self.url(path))
            .header("Authorization", &self.auth()?)
            .content_type(&format!("multipart/form-data; boundary={boundary}"))
            .send(&body[..])
            .map_err(|e| api_err(None, e.to_string()))?;
        let status = r.status().as_u16();
        let text = r.into_body().read_to_string().map_err(|e| api_err(None, e.to_string()))?;
        if status != 200 {
            return Err(api_err(Some(status), error_message(&text)));
        }
        Ok(text)
    }

    /// A Realtime WebSocket at `query` (for example `model=...`), with reads that time out every
    /// `poll` so the caller can watch for barge-in.
    fn websocket(&self, query: &str, poll: Duration) -> Result<Ws> {
        use tungstenite::client::IntoClientRequest;
        let (host, port, tls) = self.host()?;
        let base = self.conn.base_url.trim_end_matches('/');
        let ws_base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        let mut req = format!("{ws_base}/realtime?{query}").into_client_request()?;
        req.headers_mut().insert("Authorization", self.auth()?.parse()?);
        let fam = self.family()?;
        let tcp = race(&host, port, fam, self.connect_timeout()).map_err(|e| api_err(None, format!("{e:#}")))?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(self.conn.timeout))?;
        // a stalled route must not hold a send (and the thread doing it) for minutes
        tcp.set_write_timeout(Some(self.conn.timeout))?;
        let connector = tls.then(|| {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let cfg = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            tungstenite::Connector::Rustls(std::sync::Arc::new(cfg))
        });
        let (sock, _) = match tungstenite::client_tls_with_config(req, tcp, None, connector) {
            Ok(s) => s,
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r))) => {
                let status = r.status().as_u16();
                let body = r.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default().to_string();
                return Err(api_err(Some(status), error_message(&body)));
            }
            Err(e) => return Err(api_err(None, format!("websocket: {e}"))),
        };
        let ws = Ws { sock, born: Instant::now(), timeout: self.conn.timeout, draining: false };
        ws.set_poll(poll)?;
        Ok(ws)
    }
}

/// (host, port, tls) of a base URL.
fn split_url(url: &str) -> Result<(String, u16, bool)> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("base_url {url:?} must start with https:// or http://");
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let default = if tls { 443 } else { 80 };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or_else(|| anyhow!("base_url {url:?}: unclosed ["))?;
        (h, after.strip_prefix(':'))
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(p) => p.parse().with_context(|| format!("base_url {url:?}: port"))?,
        None => default,
    };
    if host.is_empty() {
        bail!("base_url {url:?} has no host");
    }
    Ok((host.to_string(), port, tls))
}

/// Connect to `host`, trying every address of the allowed families at once and keeping the first
/// that answers (happy eyeballs, without the head start for IPv6).
fn race(host: &str, port: u16, family: IpFamily, timeout: Duration) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}"))?
        .filter(|a| match family {
            IpFamily::V4 => a.is_ipv4(),
            IpFamily::V6 => a.is_ipv6(),
            IpFamily::Auto => true,
        })
        .collect();
    // two of each family is plenty: the rest are the same service
    let mut picked: Vec<SocketAddr> = addrs.iter().filter(|a| a.is_ipv6()).take(2).copied().collect();
    picked.extend(addrs.iter().filter(|a| a.is_ipv4()).take(2));
    if picked.is_empty() {
        bail!("{host} has no address for {family:?}");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    for a in picked.iter().copied() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(TcpStream::connect_timeout(&a, timeout).map_err(|e| (a, e)));
        });
    }
    drop(tx);
    let mut last = None;
    for r in rx {
        match r {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    let (a, e) = last.expect("one attempt per address");
    Err(anyhow!("connect {host} ({a}): {e}"))
}

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

struct Ws {
    sock: Socket,
    born: Instant,
    /// Longest wait for the server's next event before the call counts as failed.
    timeout: Duration,
    /// A response the user talked over is still coming; its events are read off before the
    /// next request.
    draining: bool,
}

/// What a read found.
enum Got {
    Event(Value),
    /// Nothing within the poll interval.
    Idle,
}

impl Ws {
    fn set_poll(&self, poll: Duration) -> Result<()> {
        let tcp = match self.sock.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(s) => s,
            tungstenite::stream::MaybeTlsStream::Rustls(s) => s.get_ref(),
            _ => bail!("unexpected stream"),
        };
        tcp.set_read_timeout(Some(poll))?;
        Ok(())
    }

    fn send(&mut self, v: &Value) -> Result<()> {
        self.sock.send(tungstenite::Message::text(v.to_string())).map_err(|e| api_err(None, format!("websocket: {e}")))
    }

    fn read(&mut self) -> Result<Got> {
        loop {
            match self.sock.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    let v: Value = serde_json::from_str(t.as_str()).context("realtime event")?;
                    if v["type"] == "error"
                        && v.pointer("/error/code").and_then(Value::as_str) == Some("response_cancel_not_active")
                    {
                        // the response ended before the cancel reached it: nothing to undo
                        continue;
                    }
                    if v["type"] == "error" {
                        let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("error event");
                        // the same failures the HTTP API reports by status, for the fallback rules
                        let status = match v.pointer("/error/code").and_then(Value::as_str) {
                            Some("invalid_api_key") => Some(401),
                            Some("rate_limit_exceeded" | "insufficient_quota") => Some(429),
                            _ => None,
                        };
                        return Err(api_err(status, format!("realtime: {msg}")));
                    }
                    return Ok(Got::Event(v));
                }
                Ok(tungstenite::Message::Close(c)) => {
                    let why = c.map(|c| c.reason.to_string()).unwrap_or_default();
                    return Err(api_err(None, format!("realtime session closed {why}")));
                }
                Ok(_) => continue,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    return Ok(Got::Idle);
                }
                Err(e) => return Err(api_err(None, format!("websocket: {e}"))),
            }
        }
    }

    /// Read until an event of type `want` (others are passed to `seen`), giving up after the
    /// timeout without any event.
    fn until(&mut self, want: &str, mut seen: impl FnMut(&Value)) -> Result<Value> {
        let mut last = Instant::now();
        loop {
            match self.read()? {
                Got::Event(v) if v["type"] == want => return Ok(v),
                Got::Event(v) => {
                    last = Instant::now();
                    seen(&v);
                }
                Got::Idle if last.elapsed() > self.timeout => {
                    return Err(api_err(None, format!("no {want} within {} ms", self.timeout.as_millis())));
                }
                Got::Idle => {}
            }
        }
    }

    /// Realtime sessions end after 60 minutes; one older than this is replaced before a line.
    fn stale(&self) -> bool {
        self.born.elapsed() > Duration::from_secs(55 * 60)
    }
}

/// Little-endian 16-bit PCM arriving in pieces of any length: an odd byte waits for its pair.
#[derive(Default)]
pub struct Pcm16 {
    carry: Option<u8>,
}

impl Pcm16 {
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) {
        let mut b = bytes;
        if let (Some(lo), Some(&hi)) = (self.carry, b.first()) {
            out.push(i16::from_le_bytes([lo, hi]) as f32 / 32768.0);
            self.carry = None;
            b = &b[1..];
        }
        let (pairs, rest) = b.as_chunks::<2>();
        out.extend(pairs.iter().map(|s| i16::from_le_bytes(*s) as f32 / 32768.0));
        if let [last] = rest {
            self.carry = Some(*last);
        }
    }
}

/// A streamed WAV: the header is read as it arrives, then the samples. Streamed WAVs carry a
/// placeholder data length, so the data runs to the end of the stream.
#[derive(Default)]
pub struct WavStream {
    head: Vec<u8>,
    /// (rate, channels) once the data chunk starts.
    fmt: Option<(u32, u16)>,
    data: bool,
    pcm: Pcm16,
    /// Samples of the current frame still to skip (channels after the first).
    frame: Vec<f32>,
}

impl WavStream {
    /// The sample rate, once the header has been read.
    pub fn rate(&self) -> Option<u32> {
        self.fmt.filter(|_| self.data).map(|(r, _)| r)
    }

    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) -> Result<()> {
        if self.data {
            let ch = self.fmt.map(|(_, c)| c).unwrap_or(1).max(1) as usize;
            if ch == 1 {
                self.pcm.push(bytes, out);
            } else {
                self.pcm.push(bytes, &mut self.frame);
                let whole = self.frame.len() / ch * ch;
                out.extend(self.frame[..whole].chunks_exact(ch).map(|f| f[0]));
                self.frame.drain(..whole);
            }
            return Ok(());
        }
        self.head.extend_from_slice(bytes);
        let b = &self.head;
        if b.len() < 12 {
            return Ok(());
        }
        if &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            bail!("the speech API did not send a WAV");
        }
        let mut i = 12;
        while i + 8 <= b.len() {
            let id = &b[i..i + 4];
            let n = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
            if id == b"data" {
                if self.fmt.is_none() {
                    bail!("WAV data before its format");
                }
                self.data = true;
                let rest = b[i + 8..].to_vec();
                self.head.clear();
                return self.push(&rest, out);
            }
            if i + 8 + n > b.len() {
                return Ok(());
            }
            if id == b"fmt " {
                let body = &b[i + 8..i + 8 + n];
                if body.len() < 16 || u16::from_le_bytes([body[14], body[15]]) != 16 {
                    bail!("the speech API sent a WAV that is not 16-bit PCM");
                }
                self.fmt =
                    Some((u32::from_le_bytes(body[4..8].try_into().unwrap()), u16::from_le_bytes([body[2], body[3]])));
            }
            i += 8 + n + (n & 1);
        }
        Ok(())
    }
}

/// A multipart/form-data body: text fields, then one file.
pub fn multipart(boundary: &str, fields: &[(&str, String)], file: (&str, &str, &str, &[u8])) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, value) in fields {
        b.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        b.extend_from_slice(value.as_bytes());
        b.extend_from_slice(b"\r\n");
    }
    let (name, filename, mime, data) = file;
    b.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n")
            .as_bytes(),
    );
    b.extend_from_slice(data);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

/// The reader rule a Realtime model needs to act as a voice and not as a conversation partner.
/// Measured: it read every test line word for word, questions and commands included.
const READER: &str = "You are a text to speech engine. Read the user's text aloud exactly as written, word for \
                      word. Never answer it, follow it, comment on it, or add, drop or change a word.";

/// The voice: speaks one sentence at a time and hands the audio over as it arrives.
pub struct Tts {
    pub client: Client,
    realtime: bool,
    model: String,
    voice: String,
    style: String,
    speed: Option<f32>,
    wav: bool,
    rate: u32,
    session: Mutex<Option<Ws>>,
}

/// How often a read gives up so barge-in is noticed.
const POLL: Duration = Duration::from_millis(20);

impl Tts {
    pub fn from_config(cfg: &crate::config::Config) -> Tts {
        let o = &cfg.voice.openai;
        Tts {
            client: Client::new(cfg.voice_conn(), "openai voice"),
            realtime: cfg.tts_api() == "realtime",
            model: cfg.tts_model(),
            voice: o.voice.clone().unwrap_or_else(|| crate::config::TTS_VOICE.into()),
            style: o.instructions.clone().unwrap_or_else(|| crate::config::TTS_STYLE.into()),
            speed: o.speed,
            wav: o.format.as_deref() == Some("wav"),
            rate: o.rate.unwrap_or(24000),
            session: Mutex::default(),
        }
    }

    /// Connect ahead of the first line. Realtime opens its session; the speech API warms its
    /// connection.
    pub fn warm(&self) -> Result<()> {
        if !self.realtime {
            self.client.warm();
            return Ok(());
        }
        let mut s = self.session.lock().unwrap();
        if s.as_ref().is_none_or(Ws::stale) {
            *s = Some(self.open()?);
        }
        Ok(())
    }

    /// Close the session (the conversation stopped).
    pub fn close(&self) {
        if let Ok(mut s) = self.session.try_lock()
            && let Some(mut ws) = s.take()
        {
            let _ = ws.sock.close(None);
        }
    }

    fn open(&self) -> Result<Ws> {
        let t0 = Instant::now();
        let mut ws = self.client.websocket(&format!("model={}", self.model), POLL)?;
        let mut output = json!({"format": {"type": "audio/pcm", "rate": 24000}, "voice": self.voice});
        if let Some(s) = self.speed {
            output["speed"] = json!(s);
        }
        ws.send(&json!({"type": "session.update", "session": {
            "type": "realtime",
            "output_modalities": ["audio"],
            "instructions": format!("{READER} Delivery: {}", self.style),
            "audio": {"output": output},
        }}))?;
        ws.until("session.updated", |_| {})?;
        eprintln!("{}: {} session open in {:.0} ms", self.client.what, self.model, t0.elapsed().as_secs_f32() * 1e3);
        Ok(ws)
    }

    /// Speak one sentence: `audio` gets samples and their rate as they arrive (and empty calls
    /// while waiting) and returns false to stop. An error before any audio means the sentence was
    /// not spoken at all.
    pub fn sentence(&self, text: &str, audio: &mut dyn FnMut(&[f32], u32) -> bool) -> Result<()> {
        if self.realtime { self.realtime_sentence(text, audio) } else { self.speech_sentence(text, audio) }
    }

    fn realtime_sentence(&self, text: &str, audio: &mut dyn FnMut(&[f32], u32) -> bool) -> Result<()> {
        let mut guard = self.session.lock().unwrap();
        // the session may have failed to open while this line waited for the lock
        if !self.client.available() {
            return Err(api_err(None, "the API is unavailable"));
        }
        let mut retried = false;
        loop {
            if let Some(ws) = guard.as_mut()
                && ws.draining
            {
                ws.draining = false;
                if ws.until("response.done", |_| {}).is_err() {
                    *guard = None;
                }
            }
            let reused = guard.as_ref().is_some_and(|w| !w.stale());
            if !reused {
                *guard = Some(self.open()?);
            }
            let ws = guard.as_mut().expect("opened above");
            let mut heard = false;
            let r = ws
                .send(&json!({"type": "response.create", "response": {
                    "conversation": "none",
                    "output_modalities": ["audio"],
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}],
                }}))
                .and_then(|_| {
                    Self::stream(
                        ws,
                        text,
                        &mut |p, r| {
                            heard |= !p.is_empty();
                            audio(p, r)
                        },
                        &self.client,
                    )
                });
            if let Err(e) = &r {
                *guard = None;
                // a session the server ended while it sat idle: one more try on a new one
                if reused && !heard && !retried {
                    retried = true;
                    eprintln!("{}: {e:#}; reconnecting", self.client.what);
                    continue;
                }
            }
            return r;
        }
    }

    fn stream(ws: &mut Ws, text: &str, audio: &mut dyn FnMut(&[f32], u32) -> bool, client: &Client) -> Result<()> {
        let mut pcm = Pcm16::default();
        let mut buf = Vec::new();
        let mut said = String::new();
        let mut last = Instant::now();
        let mut response: Option<String> = None;
        loop {
            let v = match ws.read()? {
                Got::Event(v) => v,
                Got::Idle => {
                    if last.elapsed() > ws.timeout {
                        return Err(api_err(None, format!("no audio within {} ms", ws.timeout.as_millis())));
                    }
                    // nothing new: still give the caller a chance to stop
                    if !audio(&[], 24000) {
                        return Self::cancel(ws, response.as_deref());
                    }
                    continue;
                }
            };
            last = Instant::now();
            match v["type"].as_str().unwrap_or("") {
                "response.created" => response = v.pointer("/response/id").and_then(Value::as_str).map(str::to_string),
                "response.output_audio.delta" => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(v["delta"].as_str().unwrap_or(""))
                        .context("audio delta")?;
                    buf.clear();
                    pcm.push(&bytes, &mut buf);
                    if !audio(&buf, 24000) {
                        return Self::cancel(ws, response.as_deref());
                    }
                }
                "response.output_audio_transcript.delta" => said.push_str(v["delta"].as_str().unwrap_or("")),
                "response.done" => {
                    let status = v.pointer("/response/status").and_then(Value::as_str).unwrap_or("");
                    if status != "completed" {
                        let why = v.pointer("/response/status_details").map(|d| d.to_string()).unwrap_or_default();
                        return Err(api_err(None, format!("response {status}: {why}")));
                    }
                    if !close_enough(text, &said) {
                        eprintln!("{}: the model said {said:?} for {text:?}", client.what);
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    /// Stop the response the user talked over and return at once: the session stays open, and
    /// what is still in flight is read off before the next line.
    fn cancel(ws: &mut Ws, response: Option<&str>) -> Result<()> {
        // an out-of-band response is cancelled by id; before it has one, it is left to finish
        if let Some(id) = response {
            ws.send(&json!({"type": "response.cancel", "response_id": id}))?;
        }
        ws.draining = true;
        Ok(())
    }

    fn speech_sentence(&self, text: &str, audio: &mut dyn FnMut(&[f32], u32) -> bool) -> Result<()> {
        let mut body = json!({
            "model": self.model,
            "voice": self.voice,
            "input": text,
            "response_format": if self.wav { "wav" } else { "pcm" },
        });
        // tts-1 and tts-1-hd refuse instructions
        if !self.model.starts_with("tts-1") && !self.style.is_empty() {
            body["instructions"] = json!(self.style);
        }
        if let Some(s) = self.speed {
            body["speed"] = json!(s);
        }
        let mut r = self.client.post_json("audio/speech", &body)?;
        let (mut pcm, mut wav) = (Pcm16::default(), WavStream::default());
        let mut chunk = vec![0u8; 4800];
        let mut buf = Vec::new();
        loop {
            let n = r.read(&mut chunk).map_err(|e| api_err(None, format!("speech stream: {e}")))?;
            if n == 0 {
                return Ok(());
            }
            buf.clear();
            let rate = if self.wav {
                wav.push(&chunk[..n], &mut buf)?;
                wav.rate().unwrap_or(self.rate)
            } else {
                pcm.push(&chunk[..n], &mut buf);
                self.rate
            };
            if !audio(&buf, rate) {
                // dropping the reader ends the request
                return Ok(());
            }
        }
    }
}

/// True when the spoken transcript has most of the words of the text.
fn close_enough(text: &str, said: &str) -> bool {
    let w = |t: &str| -> Vec<String> {
        t.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()).collect()
    };
    let (a, b) = (w(text), w(said));
    if b.is_empty() {
        return true; // no transcript to compare
    }
    let hit = a.iter().filter(|x| b.contains(x)).count();
    hit * 10 >= a.len() * 8 && b.len() <= a.len() * 3 / 2 + 2
}

/// The recognizer. A Realtime session takes the turn's audio as it is spoken and transcribes it
/// at each pause (commit); the file API sends the speech since the last pause in one request.
pub struct Stt {
    pub client: Client,
    realtime: bool,
    model: String,
    prompt: String,
    pub keywords: usize,
    languages: Vec<String>,
    delay: String,
    session: Mutex<Option<Ws>>,
    /// The keyword list the open session was configured with.
    sent_keywords: Mutex<Vec<String>>,
    /// 16 to 24 kHz for streamed audio, one state per segment so chunks join without a seam.
    up: Mutex<crate::audio::Resampler>,
}

impl Stt {
    pub fn from_config(cfg: &crate::config::Config) -> Stt {
        let o = &cfg.recognizer.openai;
        Stt {
            client: Client::new(cfg.recognizer_conn(), "openai recognizer"),
            realtime: cfg.stt_api() == "realtime",
            model: cfg.stt_model(),
            prompt: o.prompt.clone().unwrap_or_else(|| crate::config::STT_PROMPT.into()),
            keywords: o.keywords.unwrap_or(crate::config::KEYWORDS),
            languages: cfg.stt_languages(),
            delay: o.delay.clone().unwrap_or_else(|| crate::config::STT_DELAY.into()),
            session: Mutex::default(),
            sent_keywords: Mutex::default(),
            up: Mutex::new(crate::audio::Resampler::new(16000, 24000)),
        }
    }

    /// True when audio goes up while the user talks (a Realtime session).
    pub fn streams(&self) -> bool {
        self.realtime
    }

    pub fn warm(&self, terms: &[String]) -> Result<()> {
        if self.realtime {
            let keywords = keywords(terms, self.keywords);
            let mut s = self.session.lock().unwrap();
            if s.as_ref().is_none_or(Ws::stale) {
                *s = Some(self.open(&keywords)?);
            }
        } else {
            self.client.warm();
        }
        Ok(())
    }

    /// Stream 16 kHz audio into the open session (Realtime only).
    pub fn append(&self, pcm: &[f32]) -> Result<()> {
        let mut guard = self.session.lock().unwrap();
        let Some(ws) = guard.as_mut() else { bail!("no transcription session") };
        let mut up = Vec::new();
        self.up.lock().unwrap().process(pcm, &mut up);
        let r = send_pcm24(ws, &up);
        if r.is_err() {
            *guard = None;
        }
        r
    }

    /// Drop what was streamed since the last commit (the turn was dropped).
    pub fn clear(&self) {
        self.new_segment();
        if let Some(ws) = self.session.lock().unwrap().as_mut() {
            let _ = ws.send(&json!({"type": "input_audio_buffer.clear"}));
        }
    }

    fn new_segment(&self) {
        *self.up.lock().unwrap() = crate::audio::Resampler::new(16000, 24000);
    }

    /// Transcribe the speech since the last pause. `whole` is that speech at 16 kHz; `streamed`
    /// says it already went up through `append`. `before` is the turn so far (context for the
    /// file API), `terms` the repo's names, shallowest first.
    pub fn commit(&self, whole: &[f32], streamed: bool, before: &str, terms: &[String]) -> Result<String> {
        let keywords = keywords(terms, self.keywords);
        if self.realtime {
            self.new_segment();
            return self.realtime_commit(whole, streamed, &keywords);
        }
        let mut prompt = self.prompt.clone();
        if !before.trim().is_empty() {
            prompt.push_str(&format!(" The speaker just said: \"{}\"", before.trim()));
        }
        let mut fields: Vec<(&str, String)> = vec![("model", self.model.clone()), ("response_format", "json".into())];
        if !prompt.trim().is_empty() && !self.model.contains("whisper") {
            fields.push(("prompt", prompt));
        }
        // gpt-transcribe takes keywords and a list of languages; older models one language
        if self.model.starts_with("gpt-transcribe") {
            fields.extend(keywords.iter().map(|k| ("keywords[]", k.clone())));
            fields.extend(self.languages.iter().map(|l| ("languages[]", l.clone())));
        } else if let Some(l) = self.languages.first() {
            fields.push(("language", l.clone()));
        }
        let boundary = format!("parlar{:016x}", rand64());
        let body = multipart(&boundary, &fields, ("file", "turn.wav", "audio/wav", &crate::wav::encode(whole, 16000)));
        let text = self.client.post_multipart("audio/transcriptions", &boundary, body)?;
        let v: Value = serde_json::from_str(&text).context("transcription reply")?;
        Ok(v["text"].as_str().unwrap_or("").trim().to_string())
    }

    fn open(&self, keywords: &[String]) -> Result<Ws> {
        let t0 = Instant::now();
        let mut ws = self.client.websocket("intent=transcription", POLL)?;
        ws.send(&self.session_update(keywords))?;
        ws.until("session.updated", |_| {})?;
        *self.sent_keywords.lock().unwrap() = keywords.to_vec();
        eprintln!("{}: {} session open in {:.0} ms", self.client.what, self.model, t0.elapsed().as_secs_f32() * 1e3);
        Ok(ws)
    }

    fn session_update(&self, keywords: &[String]) -> Value {
        let mut tr = json!({"model": self.model});
        let whisper = self.model.contains("whisper");
        if !self.prompt.is_empty() && !whisper {
            tr["prompt"] = json!(self.prompt);
        }
        if !keywords.is_empty() && !whisper {
            tr["keywords"] = json!(keywords);
        }
        if !self.languages.is_empty() {
            if whisper {
                tr["language"] = json!(self.languages[0]);
            } else {
                tr["languages"] = json!(self.languages);
            }
        }
        if self.model.contains("live") {
            tr["delay"] = json!(self.delay);
        }
        json!({"type": "session.update", "session": {"type": "transcription", "audio": {"input": {
            "format": {"type": "audio/pcm", "rate": 24000},
            "transcription": tr,
            "turn_detection": null,
        }}}})
    }

    fn realtime_commit(&self, whole: &[f32], streamed: bool, keywords: &[String]) -> Result<String> {
        let mut guard = self.session.lock().unwrap();
        let mut streamed = streamed;
        if guard.as_ref().is_none_or(Ws::stale) {
            *guard = Some(self.open(keywords)?);
            streamed = false;
        }
        let ws = guard.as_mut().expect("opened above");
        let r = (|| {
            if *self.sent_keywords.lock().unwrap() != keywords {
                // the focused repo changed: its names apply from this commit on
                ws.send(&self.session_update(keywords))?;
                ws.until("session.updated", |_| {})?;
                *self.sent_keywords.lock().unwrap() = keywords.to_vec();
            }
            if !streamed {
                ws.send(&json!({"type": "input_audio_buffer.clear"}))?;
                send_audio(ws, whole)?;
            }
            ws.send(&json!({"type": "input_audio_buffer.commit"}))?;
            let mut item: Option<String> = None;
            let mut done: Vec<(String, String)> = Vec::new();
            let t0 = Instant::now();
            loop {
                if let Some(id) = &item
                    && let Some((_, t)) = done.iter().find(|(i, _)| i == id)
                {
                    return Ok(t.trim().to_string());
                }
                if t0.elapsed() > ws.timeout {
                    return Err(api_err(None, format!("no transcript within {} ms", ws.timeout.as_millis())));
                }
                if let Got::Event(v) = ws.read()? {
                    match v["type"].as_str().unwrap_or("") {
                        "input_audio_buffer.committed" => item = v["item_id"].as_str().map(str::to_string),
                        "conversation.item.input_audio_transcription.completed" => done.push((
                            v["item_id"].as_str().unwrap_or("").to_string(),
                            v["transcript"].as_str().unwrap_or("").to_string(),
                        )),
                        "conversation.item.input_audio_transcription.failed" => {
                            let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("failed");
                            return Err(api_err(None, format!("transcription: {msg}")));
                        }
                        _ => {}
                    }
                }
            }
        })();
        if r.is_err() {
            *guard = None;
        }
        r
    }
}

/// 16 kHz samples up a Realtime session in one piece of speech.
fn send_audio(ws: &mut Ws, pcm: &[f32]) -> Result<()> {
    send_pcm24(ws, &crate::audio::resample(pcm, 16000, 24000))
}

/// 24 kHz samples up a Realtime session as PCM16, in pieces of 1 s at most.
fn send_pcm24(ws: &mut Ws, up: &[f32]) -> Result<()> {
    if up.is_empty() {
        return Ok(());
    }
    let bytes: Vec<u8> = up.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
    for c in bytes.chunks(48000) {
        ws.send(&json!({"type": "input_audio_buffer.append",
            "audio": base64::engine::general_purpose::STANDARD.encode(c)}))?;
    }
    Ok(())
}

/// The repo names worth sending as keyword hints: the first `max`, cleaned of what the API
/// refuses (angle brackets, line breaks).
pub fn keywords(terms: &[String], max: usize) -> Vec<String> {
    terms
        .iter()
        .map(|t| t.replace(['<', '>', '\r', '\n'], " ").trim().to_string())
        .filter(|t| !t.is_empty())
        .take(max)
        .collect()
}

fn rand64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn pcm_split_at_any_byte_decodes_the_same() {
        let samples: Vec<i16> = vec![0, 1, -1, 16384, -32768, 32767, 1234, -4321];
        let bytes = le(&samples);
        let whole: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        for cut in [1, 3, 5, 7] {
            let (mut d, mut out) = (Pcm16::default(), Vec::new());
            for piece in bytes.chunks(cut) {
                d.push(piece, &mut out);
            }
            assert_eq!(out, whole, "pieces of {cut} bytes");
        }
    }

    #[test]
    fn streamed_wav_header_then_samples() {
        let mut head = Vec::new();
        head.extend_from_slice(b"RIFF\xff\xff\xff\xffWAVEfmt ");
        head.extend_from_slice(&16u32.to_le_bytes());
        head.extend_from_slice(&1u16.to_le_bytes());
        head.extend_from_slice(&2u16.to_le_bytes()); // stereo: the first channel is kept
        head.extend_from_slice(&22050u32.to_le_bytes());
        head.extend_from_slice(&(22050u32 * 4).to_le_bytes());
        head.extend_from_slice(&4u16.to_le_bytes());
        head.extend_from_slice(&16u16.to_le_bytes());
        head.extend_from_slice(b"data\xff\xff\xff\xff");
        let all = [head, le(&[100, -1, 200, -2, 300, -3])].concat();
        let (mut w, mut out) = (WavStream::default(), Vec::new());
        for piece in all.chunks(5) {
            w.push(piece, &mut out).unwrap();
        }
        assert_eq!(w.rate(), Some(22050));
        assert_eq!(out, vec![100.0 / 32768.0, 200.0 / 32768.0, 300.0 / 32768.0]);
        assert!(WavStream::default().push(b"ID3\x04 not a wav at all", &mut Vec::new()).is_err());
    }

    #[test]
    fn base_urls_name_host_port_and_tls() {
        let u = |s: &str| split_url(s).unwrap();
        assert_eq!(u("https://api.openai.com/v1"), ("api.openai.com".into(), 443, true));
        assert_eq!(u("http://localhost:8000/v1"), ("localhost".into(), 8000, false));
        assert_eq!(u("http://[::1]/v1"), ("::1".into(), 80, false));
        assert_eq!(u("https://[2001:db8::1]:8443"), ("2001:db8::1".into(), 8443, true));
        assert!(split_url("api.openai.com/v1").is_err());
        assert!(split_url("https://host:port/v1").is_err());
    }

    #[test]
    fn keys_never_reach_a_log_line() {
        let s = scrub("Incorrect API key provided: sk-proj-abc123XYZ***********. You can find your key");
        assert!(!s.contains("abc123"), "{s}");
        assert!(s.contains("Incorrect API key provided"));
        assert_eq!(scrub("rate limit reached for requests"), "rate limit reached for requests");
        let model = "model gpt-4o-mini-transcribe-2025-12-15 is not available";
        assert_eq!(scrub(model), model);
        assert!(scrub("token AbCdEfGh1234567890IjKlMnOp0987654321QrSt").contains("[key]"));
    }

    #[test]
    fn multipart_body_has_fields_then_the_file() {
        let b = multipart(
            "XX",
            &[("model", "m".into()), ("keywords[]", "router.ts".into())],
            ("file", "a.wav", "audio/wav", b"RIFF"),
        );
        let s = String::from_utf8(b).unwrap();
        assert!(s.starts_with("--XX\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n"));
        assert!(s.contains("name=\"keywords[]\"\r\n\r\nrouter.ts\r\n"));
        assert!(s.ends_with("Content-Type: audio/wav\r\n\r\nRIFF\r\n--XX--\r\n"));
    }

    #[test]
    fn keyword_hints_are_capped_and_cleaned() {
        let t: Vec<String> = ["parlar", "<x>", "a\nb", "", "crates"].iter().map(|s| s.to_string()).collect();
        assert_eq!(keywords(&t, 3), vec!["parlar", "x", "a b"]);
    }

    #[test]
    fn transcript_check_allows_punctuation_but_not_an_answer() {
        assert!(close_enough("Next I will rebase on main.", "Next, I will rebase on main."));
        assert!(!close_enough("What do you think, keep it?", "That's a good question. It depends on what you want."));
    }
}
