//! Hook handlers. Each reads the harness hook JSON on stdin and prints the hook reply on stdout.
//! When parleyd is not running every handler exits 0 with no output, so the plugin is inert.

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
}

/// A hook runs on every tool call, so a slow or stuck daemon must cost little.
const QUICK: Duration = Duration::from_millis(400);

/// Returns the process exit code.
pub fn run(event: Event, harness: Harness) -> Result<i32> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let session = input.get("session_id").and_then(Value::as_str).map(str::to_string);
    let o = origin(session);
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
            // attach again: a session that started before parleyd gets its folder and harness
            let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or_default().to_string();
            c.call(&Request::Attach { origin: o.clone(), harness, cwd, mcp: false }, quick)?;
            c.call(&Request::Event { origin: o, event: TurnEvent::TurnStart, tool: None }, quick)?;
        }
        Event::PreTool => {
            let tool = tool_name(&input);
            c.call(&Request::Event { origin: o.clone(), event: TurnEvent::ToolStart, tool }, quick)?;
            if subagent {
                return Ok(0);
            }
            let items = items(c.call(&Request::ClaimStop { origin: o }, quick)?);
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
            c.call(&Request::Event { origin: o.clone(), event: ev, tool: tool_name(&input) }, quick)?;
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
            // this waiter holds the turn open; parleyd only lets it wait while the conversation
            // is on and this session has focus, and releases it when either changes
            let r = c.call(&Request::Wait { origin: o, timeout_ms: wait_ms(), holds_turn: true }, None)?;
            let items = items(r);
            if !items.is_empty() {
                print_json(&json!({ "decision": "block", "reason": format::utterances(&items) }));
            }
        }
        Event::Wait => unreachable!("handled above"),
    }
    Ok(0)
}

/// Background idle waiter. It is detached from the harness, so it rides out parleyd restarts:
/// on a lost connection it reconnects until its deadline instead of leaving the session deaf.
fn wait(o: &Origin) -> Result<i32> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms());
    loop {
        if Instant::now() >= deadline {
            return Ok(0);
        }
        let Some(mut c) = Client::connect() else {
            std::thread::sleep(Duration::from_secs(3));
            continue;
        };
        let left = deadline.saturating_duration_since(Instant::now()).as_millis() as u64;
        let req = Request::Wait { origin: o.clone(), timeout_ms: left, holds_turn: false };
        match c.call(&req, None) {
            Ok(Response::Utterances { items, superseded }) => {
                if !items.is_empty() {
                    eprintln!("{}", format::utterances(&items));
                    return Ok(2);
                }
                // replaced by a newer waiter, released, or timed out: this one is done
                let _ = superseded;
                return Ok(0);
            }
            Ok(_) => return Ok(0),
            Err(_) => std::thread::sleep(Duration::from_secs(2)),
        }
    }
}

/// Add the conversation lines not yet printed as the hook's system message.
fn show(c: &mut Client, o: &Origin, out: &mut Map<String, Value>) {
    if let Ok(Response::Transcript { lines }) = c.call(&Request::Transcript { origin: o.clone() }, Some(QUICK))
        && !lines.is_empty() {
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
    std::env::var("PARLEY_WAIT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(23 * 3600 * 1000)
}

fn last_message(input: &Value) -> Option<String> {
    input.get("last_assistant_message").and_then(Value::as_str).map(str::to_string)
}

fn tool_name(input: &Value) -> Option<String> {
    input.get("tool_name").and_then(Value::as_str).map(str::to_string)
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
