//! One-time harness setup that a plugin cannot do for itself.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The permission rule that lets `say` run without a prompt on every spoken line.
pub const CLAUDE_SAY_RULE: &str = "mcp__plugin_parlar_parlar__say";

fn claude_settings() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("settings.json");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".claude/settings.json")
}

/// Add the `say` allow rule to the user's Claude Code settings. Other keys keep their order.
pub fn claude() -> Result<()> {
    let path = claude_settings();
    let mut v: Value = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?
        }
        Ok(_) => json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let obj = v.as_object_mut().context("settings.json is not an object")?;
    let perms = obj.entry("permissions").or_insert_with(|| json!({}));
    let allow = perms
        .as_object_mut()
        .context("permissions is not an object")?
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("permissions.allow is not a list")?;
    if allow.iter().any(|r| r.as_str() == Some(CLAUDE_SAY_RULE)) {
        println!("{} already allows {CLAUDE_SAY_RULE}", path.display());
        return Ok(());
    }
    allow.push(json!(CLAUDE_SAY_RULE));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.parlar-tmp");
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&v)?))?;
    std::fs::rename(&tmp, &path)?;
    println!("allowed {CLAUDE_SAY_RULE} in {}", path.display());
    Ok(())
}
