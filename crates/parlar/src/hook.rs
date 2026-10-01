//! Hook handlers. Each reads the harness hook JSON on stdin and prints the hook reply on stdout.
//! When parlard is not running every handler exits 0 with no output, so the plugin is inert.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::client::{Client, origin};
use crate::format;
use crate::proto::{Harness, Origin, Request, Response, TurnEvent, Utterance};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Event {
    SessionStart,
    SessionEnd,
    Prompt,
    PreTool,
    PostTool,
    PostToolFailure,
    /// Synchronous Stop: deliver anything already pending by blocking the stop.
    Stop,
    /// Background Stop waiter (Claude Code asyncRewake): exit 2 with the utterance to wake the
    /// idle session.
    Wait,
    /// Blocking Stop waiter for harnesses without asyncRewake: hold the turn open until the user
    /// speaks, then block the stop with the utterance.
    StopWait,
    /// The user interrupted the turn (Codex's Interrupt event).
    Interrupt,
}

/// A hook runs on every tool call, so a slow or stuck daemon must cost little.
const QUICK: Duration = Duration::from_millis(400);

/// Returns the process exit code.
pub fn run(event: Event, harness: Harness) -> Result<i32> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let session = input.get("session_id").and_then(Value::as_str).map(str::to_string);
    let mut o = origin(session);
    o.cwd = input.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string);
    o.harness = Some(harness);
    o.transcript = input.get("transcript_path").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_string);
    if event == Event::Wait {
        return wait(&o);
    }
    let Some(mut c) = Client::connect() else { return Ok(0) };
    let quick = Some(QUICK);
    // a subagent's tool calls carry the parent's session id; speech is for the main thread
    let subagent = input.get("agent_id").and_then(Value::as_str).is_some_and(|a| !a.is_empty());
    // Claude Code shows a hook's systemMessage to the person without giving it to the model,
    // which is how the conversation gets printed at no token cost
    let shows = harness == Harness::Claude;

    match event {
        Event::SessionStart => {
            let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or_default().to_string();
            let r = c.call(&Request::Attach { origin: o, harness, cwd, mcp: false }, quick)?;
            if let Response::Attached { active: true, focused: true } = r {
                print_json(&json!({
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": format::ACTIVATED,
                    }
                }));
            }
        }
        Event::SessionEnd => {
            // /clear ends the session and starts a new id in the same harness: keep its focus
            let rebind = input.get("reason").and_then(Value::as_str) == Some("clear");
            c.call(&Request::Detach { origin: o, rebind }, quick)?;
        }
        Event::Prompt => {
            // attach again: a session that started before parlard gets its folder and harness
            let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or_default().to_string();
            c.call(&Request::Attach { origin: o.clone(), harness, cwd, mcp: false }, quick)?;
            c.call(
                &Request::Event { origin: o, event: TurnEvent::TurnStart, tool: None, detail: None, call: None },
                quick,
            )?;
        }
        Event::PreTool => {
            let tool = tool_name(&input);
            let (detail, call) = if subagent { (None, None) } else { (tool_detail(&input), Some(call_id(&input))) };
            c.call(&Request::Event { origin: o.clone(), event: TurnEvent::ToolStart, tool, detail, call }, quick)?;
            if subagent {
                return Ok(0);
            }
            let items = items(c.call(&Request::ClaimStop { origin: o, call: Some(call_id(&input)) }, quick)?);
            if !items.is_empty() {
                let reason = format!(
                    "The user asked you to stop, so this tool call was not run.\n{}",
                    format::utterances(&items)
                );
                print_json(&json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": reason,
                    }
                }));
            }
        }
        Event::PostTool | Event::PostToolFailure => {
            let failed = event == Event::PostToolFailure || tool_failed(&input);
            let ev = if failed { TurnEvent::ToolError } else { TurnEvent::ToolEnd };
            let call = (!subagent).then(|| call_id(&input));
            c.call(
                &Request::Event { origin: o.clone(), event: ev, tool: tool_name(&input), detail: None, call },
                quick,
            )?;
            if subagent {
                return Ok(0);
            }
            let items = items(c.call(&Request::Claim { origin: o.clone(), at_stop: false }, quick)?);
            let mut out = Map::new();
            if !items.is_empty() {
                let name = if event == Event::PostTool { "PostToolUse" } else { "PostToolUseFailure" };
                out.insert(
                    "hookSpecificOutput".into(),
                    json!({ "hookEventName": name, "additionalContext": format::utterances(&items) }),
                );
            }
            if shows {
                show(&mut c, &o, &mut out);
            }
            print_map(out);
        }
        Event::Stop => {
            c.call(&Request::TurnEnd { origin: o.clone(), last_message: last_message(&input) }, quick)?;
            let items = items(c.call(&Request::Claim { origin: o.clone(), at_stop: true }, quick)?);
            let mut out = Map::new();
            if !items.is_empty() {
                out.insert("decision".into(), json!("block"));
                out.insert("reason".into(), json!(format::utterances(&items)));
            }
            if shows {
                show(&mut c, &o, &mut out);
            }
            print_map(out);
        }
        Event::StopWait => {
            c.call(&Request::TurnEnd { origin: o.clone(), last_message: last_message(&input) }, quick)?;
            // this waiter holds the turn open; parlard only lets it wait while the conversation
            // is on and this session has focus, and releases it when either changes
            drop(c);
            let items = wait_for(&o, true);
            if !items.is_empty() {
                print_json(&json!({ "decision": "block", "reason": format::utterances(&items) }));
            }
        }
        Event::Interrupt => {
            c.call(
                &Request::Event { origin: o, event: TurnEvent::Interrupted, tool: None, detail: None, call: None },
                quick,
            )?;
        }
        Event::Wait => unreachable!("handled above"),
    }
    Ok(0)
}

/// How long a waiter keeps trying to reach a parlard that went away (a restart takes seconds).
const RIDE_OUT: Duration = Duration::from_secs(300);

/// Background idle waiter. It is detached from the harness, so it rides out parlard restarts:
/// on a lost connection it reconnects for `RIDE_OUT` instead of leaving the session deaf. With
/// no parlard at the start it exits at once, so a stopped parlar leaves nothing running.
fn wait(o: &Origin) -> Result<i32> {
    let items = wait_for(o, false);
    if items.is_empty() {
        return Ok(0);
    }
    eprintln!("{}", format::utterances(&items));
    Ok(2)
}

/// Wait for speech for this session, reconnecting through parlard restarts. Empty when the wait
/// ends without speech: timed out, replaced by a newer waiter, released, or parlard gone.
/// `holds_turn` is the blocking Stop waiter of harnesses without a background wake.
fn wait_for(o: &Origin, holds_turn: bool) -> Vec<Utterance> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms());
    let mut down_since: Option<Instant> = None;
    let mut first = true;
    loop {
        if Instant::now() >= deadline {
            return Vec::new();
        }
        let Some(mut c) = Client::connect() else {
            let down = *down_since.get_or_insert_with(Instant::now);
            if first || down.elapsed() >= RIDE_OUT {
                return Vec::new();
            }
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        first = false;
        down_since = None;
        let left = deadline.saturating_duration_since(Instant::now()).as_millis() as u64;
        let req = Request::Wait { origin: o.clone(), timeout_ms: left, holds_turn };
        match c.call(&req, None) {
            // speech, or replaced by a newer waiter, released, or timed out: this one is done
            Ok(r) => return items(r),
            Err(_) => std::thread::sleep(Duration::from_secs(2)),
        }
    }
}

/// Add the conversation lines not yet printed as the hook's system message.
fn show(c: &mut Client, o: &Origin, out: &mut Map<String, Value>) {
    if let Ok(Response::Transcript { lines }) = c.call(&Request::Transcript { origin: o.clone() }, Some(QUICK))
        && !lines.is_empty()
    {
        out.insert("systemMessage".into(), json!(lines.join("\n")));
    }
}

fn items(r: Response) -> Vec<Utterance> {
    match r {
        Response::Utterances { items, .. } => items,
        _ => Vec::new(),
    }
}

fn wait_ms() -> u64 {
    std::env::var("PARLAR_WAIT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(23 * 3600 * 1000)
}

fn last_message(input: &Value) -> Option<String> {
    input.get("last_assistant_message").and_then(Value::as_str).map(str::to_string)
}

fn tool_name(input: &Value) -> Option<String> {
    input.get("tool_name").and_then(Value::as_str).map(str::to_string)
}

/// The agent's own words for what a tool call does: Claude Code's shell tool carries a
/// `description`, Codex's a `justification`.
fn tool_detail(input: &Value) -> Option<String> {
    let args = input.get("tool_input")?;
    ["description", "justification"].iter().find_map(|k| args.get(*k).and_then(Value::as_str)).map(str::to_string)
}

/// Pairs a tool call's start with its end. A harness without ids gets one shared id, so any end
/// closes every open call.
fn call_id(input: &Value) -> String {
    input.get("tool_use_id").and_then(Value::as_str).unwrap_or("main").to_string()
}

fn tool_failed(input: &Value) -> bool {
    let r = input.get("tool_response");
    r.and_then(|r| r.get("is_error")).and_then(Value::as_bool).unwrap_or(false)
        || r.and_then(|r| r.get("interrupted")).and_then(Value::as_bool).unwrap_or(false)
}

fn print_json(v: &Value) {
    println!("{v}");
}

fn print_map(m: Map<String, Value>) {
    if !m.is_empty() {
        print_json(&Value::Object(m));
    }
}
