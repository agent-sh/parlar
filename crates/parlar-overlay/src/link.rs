//! The overlay's connection to parlard: a subscription that reconnects on its own thread and
//! hands events over a channel, plus one-shot requests for the menu and the clicks.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use parlar::client::Client;
use parlar::proto::{Request, Response, Ui};
use parlar::transport;

/// What the subscription thread reports.
pub enum Event {
    Connected(bool),
    Ui(Ui),
}

/// Keep a subscription to parlard open, reconnecting every second while it is away.
pub fn subscribe(tx: mpsc::Sender<Event>) {
    std::thread::Builder::new()
        .name("parlar-overlay-link".into())
        .spawn(move || {
            loop {
                match subscribe_once(&tx) {
                    // the receiver is gone: the overlay is closing
                    Err(e) if e.downcast_ref::<mpsc::SendError<Event>>().is_some() => return,
                    _ => {}
                }
                if tx.send(Event::Connected(false)).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("spawn link thread");
}

fn subscribe_once(tx: &mpsc::Sender<Event>) -> Result<()> {
    let mut conn = transport::connect()?;
    let mut line = serde_json::to_vec(&Request::Subscribe)?;
    line.push(b'\n');
    conn.write_all(&line)?;
    tx.send(Event::Connected(true))?;
    let mut rd = BufReader::new(conn);
    let mut buf = String::new();
    loop {
        buf.clear();
        if rd.read_line(&mut buf)? == 0 {
            return Ok(());
        }
        if let Ok(ev) = serde_json::from_str::<Ui>(&buf) {
            tx.send(Event::Ui(ev))?;
        }
    }
}

/// One request with a short timeout; None when parlard is not running.
pub fn request(req: &Request) -> Option<Response> {
    let mut c = Client::connect()?;
    c.call(req, Some(Duration::from_millis(1500))).ok()
}

/// A `set` request with one field.
pub fn set(f: impl FnOnce(&mut SetReq)) -> Option<Response> {
    let mut s = SetReq::default();
    f(&mut s);
    request(&Request::Set {
        active: s.active,
        mic_muted: s.mic_muted,
        voice_off: s.voice_off,
        focus: s.focus,
        input: s.input,
        output: s.output,
    })
}

#[derive(Default)]
pub struct SetReq {
    pub active: Option<bool>,
    pub mic_muted: Option<bool>,
    pub voice_off: Option<bool>,
    pub focus: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
}
