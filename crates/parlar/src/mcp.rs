//! Minimal MCP server over stdio: the `say` tool plus the conversation-mode instructions.

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::client::{Client, mcp_origin};
use crate::format;
use crate::proto::{Harness, Request, Response, SayKind};

pub const SAY_TOOL: &str = "say";
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub fn serve(harness: Harness) -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut daemon: Option<Client> = None;
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let id = msg.get("id").cloned();
        let result = match method {
            "initialize" => {
                let asked = msg.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = if VERSIONS.contains(&asked) { asked } else { VERSIONS[1] };
                Some(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "parlar", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": format::INSTRUCTIONS,
                }))
            }
            "notifications/initialized" => {
                daemon = attach(harness);
                None
            }
            "ping" => Some(json!({})),
            "tools/list" => Some(json!({ "tools": [say_tool()] })),
            "tools/call" => Some(call(&mut daemon, harness, &msg)),
            _ if id.is_none() => None,
            _ => {
                reply_error(&mut out, id, -32601, &format!("method not found: {method}"))?;
                continue;
            }
        };
        if let (Some(id), Some(result)) = (id, result) {
            let mut v = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))?;
            v.push(b'\n');
            out.write_all(&v)?;
            out.flush()?;
        }
    }
    Ok(())
}

fn attach(harness: Harness) -> Option<Client> {
    let mut c = Client::connect()?;
    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let req = Request::Attach { origin: mcp_origin(None), harness, cwd, mcp: true };
    c.call(&req, Some(Duration::from_secs(2))).ok()?;
    Some(c)
}

fn say_tool() -> Value {
    json!({
        "name": SAY_TOOL,
        "description": "Speak to the user out loud. Use only in voice mode. For answers, progress, \
            the step you are about to take, and questions. One to three short spoken sentences, \
            no code, paths or markdown. Returns anything the user said meanwhile.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "What to say, as spoken words." },
                "kind": {
                    "type": "string",
                    "enum": ["answer", "status", "next", "question"],
                    "description": "answer: reply to the user. status: progress. next: the step \
                        you are about to take. question: you need their input."
                }
            },
            "required": ["text"],
            "additionalProperties": false
        },
        "annotations": { "readOnlyHint": true, "openWorldHint": false }
    })
}

fn call(daemon: &mut Option<Client>, harness: Harness, msg: &Value) -> Value {
    let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
    if name != SAY_TOOL {
        return text_result(&format!("Unknown tool {name}."), true);
    }
    let args = msg.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
    let Some(text) = args.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) else {
        return text_result("say needs non-empty text.", true);
    };
    let kind: SayKind = args.get("kind").and_then(|k| serde_json::from_value(k.clone()).ok()).unwrap_or_default();
    // Codex sends its session id with every call; Claude Code does not, and is matched by pid
    let session = ["/params/_meta/sessionId", "/params/_meta/threadId"]
        .iter()
        .find_map(|p| msg.pointer(p).and_then(Value::as_str))
        .map(str::to_string);
    let req = Request::Say { origin: mcp_origin(session), text: text.to_string(), kind };
    // the daemon may have started or restarted since the last call
    for attempt in 0..2 {
        if daemon.is_none() || attempt == 1 {
            *daemon = attach(harness);
        }
        let Some(c) = daemon.as_mut() else { break };
        match c.call(&req, Some(Duration::from_secs(5))) {
            Ok(Response::Said { spoken, items }) => {
                let mut s = String::from(if spoken {
                    "Said."
                } else {
                    "Not spoken: this session does not have voice focus or voice output is off. Shown as text."
                });
                if !items.is_empty() {
                    s.push_str("\nThe user said meanwhile:\n");
                    s.push_str(&format::utterances(&items));
                }
                return text_result(&s, false);
            }
            Ok(Response::Error { message }) => return text_result(&message, true),
            Ok(_) => return text_result("Unexpected reply from parlard.", true),
            Err(_) => continue,
        }
    }
    text_result("Voice mode is off (parlard is not running). Continue in text.", false)
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn reply_error(out: &mut impl Write, id: Option<Value>, code: i64, message: &str) -> Result<()> {
    let mut v = serde_json::to_vec(&json!({
        "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message }
    }))?;
    v.push(b'\n');
    out.write_all(&v)?;
    out.flush()?;
    Ok(())
}
