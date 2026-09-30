//! Hook handlers. Each reads the harness hook JSON on stdin and prints the hook reply on stdout.
//! When parleyd is not running every handler exits 0 with no output, so the plugin is inert.

use std::io::Read;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::client::{Client, origin};
use crate::format;
use crate::proto::{Harness, Request, Response, TurnEvent, Utterance};

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

/// Returns the process exit code.
pub fn run(event: Event, harness: Harness) -> Result<i32> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let Some(mut c) = Client::connect() else { return Ok(0) };
    let session = input.get("session_id").and_then(Value::as_str).map(str::to_string);
    let o = origin(session);
    let quick = Some(Duration::from_secs(2));

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
            c.call(&Request::Detach { origin: o }, quick)?;
        }
        Event::Prompt => {
            c.call(&Request::Event { origin: o, event: TurnEvent::TurnStart, tool: None }, quick)?;
        }
        Event::PreTool => {
            let tool = tool_name(&input);
            c.call(&Request::Event { origin: o.clone(), event: TurnEvent::ToolStart, tool }, quick)?;
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
            let items = items(c.call(&Request::Claim { origin: o }, quick)?);
            if !items.is_empty() {
                let name = if event == Event::PostTool { "PostToolUse" } else { "PostToolUseFailure" };
                print_json(&json!({
                    "hookSpecificOutput": {
                        "hookEventName": name,
                        "additionalContext": format::utterances(&items),
                    }
                }));
            }
        }
        Event::Stop => {
            let items = items(c.call(&Request::Claim { origin: o }, quick)?);
            if !items.is_empty() {
                print_json(&json!({ "decision": "block", "reason": format::utterances(&items) }));
            }
        }
        Event::Wait => {
            let r = c.call(&Request::Wait { origin: o, timeout_ms: wait_ms() }, None)?;
            let items = items(r);
            if !items.is_empty() {
                eprintln!("{}", format::utterances(&items));
                return Ok(2);
            }
        }
        Event::StopWait => {
            let r = c.call(&Request::Wait { origin: o, timeout_ms: wait_ms() }, None)?;
            let items = items(r);
            if !items.is_empty() {
                print_json(&json!({ "decision": "block", "reason": format::utterances(&items) }));
            }
        }
    }
    Ok(0)
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
