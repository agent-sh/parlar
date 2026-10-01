//! Text that crosses the boundary to the model or to the speaker.

use crate::proto::Utterance;

/// MCP server instructions. Codex prioritizes the first 512 characters and Claude Code truncates
/// at 2048, so the load-bearing rule comes first.
pub const INSTRUCTIONS: &str = "\
When voice mode is on (you receive [voice] messages), talk to the user only through the say tool: \
short spoken sentences, never code, file paths, logs or markdown. Before a long action, say what \
you are about to do. Answer questions directly. Keep tool calls, diffs and results in the session \
as usual; say only what a colleague would say out loud while working.

Details:
- kind: answer (reply to what they asked), status (progress), next (the step you are about to \
take), question (you need their input; then stop and wait).
- One to three sentences per call. Name files by their short name, not the full path. Round \
numbers. Never read code aloud.
- [voice] messages are speech recognition output and may contain recognition errors. When the \
user corrects themselves, the last version wins. Use the repo context to fix misheard names.
- A [voice] message that arrives while you work is steering: take it into account right away.
- If the user says stop, stop the current action and say what state things are in.
- say returns anything the user said meanwhile; treat it like a new [voice] message.";

/// Reminder added when voice mode is switched on for a running session.
pub const ACTIVATED: &str = "Voice mode is on. Talk to the user through the say tool as described \
in the parley server instructions: short spoken sentences, no code or paths.";

/// Closes every delivery: models drift back to plain text replies without it.
pub const REPLY_CUE: &str = "(Reply out loud with the say tool.)";

pub fn utterances(items: &[Utterance]) -> String {
    let mut out = String::new();
    for u in items {
        if !out.is_empty() {
            out.push('\n');
        }
        let tag = match u.revises {
            Some(prev) => format!("[voice u{} continues u{}]", u.id, prev),
            None => format!("[voice u{}]", u.id),
        };
        out.push_str(&tag);
        out.push(' ');
        out.push_str(u.text.trim());
        if let Some(cut) = &u.interrupted_after {
            out.push_str(&format!("\n(they talked over you; you had said: \"{}\")", cut.trim()));
        }
        if let Some(heard) = &u.heard {
            out.push_str(&format!(
                "\n(heard: \"{}\". Speech recognition, may contain errors. When the user corrects \
                 themselves, the last version wins.)",
                heard.trim()
            ));
        }
    }
    if !out.is_empty() {
        out.push('\n');
        out.push_str(REPLY_CUE);
    }
    out
}

/// The first one or two sentences of a reply, made speakable, capped near 240 characters.
pub fn opening(text: &str) -> String {
    let s = speakable(text);
    let mut out = String::new();
    let mut sentences = 0;
    let mut rest = s.as_str();
    while sentences < 2 && !rest.is_empty() {
        let end = rest
            .char_indices()
            .find(|&(i, c)| ".!?".contains(c) && rest[i + 1..].starts_with(' '))
            .map(|(i, _)| i + 1)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(end);
        if !out.is_empty() && out.len() + head.len() > 240 {
            break;
        }
        out.push_str(head);
        rest = tail;
        sentences += 1;
    }
    if out.len() > 240 {
        let cut = out[..240].rfind(' ').unwrap_or(240);
        out.truncate(cut);
        out.push_str("...");
    }
    out.trim().to_string()
}

const STOP_WORDS: &[&str] = &[
    "stop", "wait", "hold on", "hold it", "cancel", "abort", "don't", "do not", "no no", "halt",
    "pause", "never mind", "nevermind",
];

/// True when an utterance reads as a request to stop the current action. Only the opening words
/// count: "wait, use the other file" is steering that starts with a stop, and it still should
/// block the next tool call, but "can we wait for CI" should not.
pub fn is_stop(text: &str) -> bool {
    let t = text.trim().trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    STOP_WORDS.iter().any(|w| {
        t.strip_prefix(w)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric()))
    })
}

/// Make agent text safe to speak: drop markdown, code, URLs, and shorten paths. This is the
/// safety net behind the instructions, not a substitute for them.
pub fn speakable(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || l.is_empty() {
            continue;
        }
        let l = l.trim_start_matches(['#', '>', '-', '*', '+', ' ']);
        let l = strip_list_number(l);
        if !out.is_empty() {
            let ends = out.ends_with(['.', '!', '?', ':', ';']);
            out.push_str(if ends { " " } else { ". " });
        }
        out.push_str(&clean_words(l));
    }
    out.trim().to_string()
}

fn strip_list_number(l: &str) -> &str {
    let digits = l.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && l[digits..].starts_with(['.', ')']) {
        l[digits + 1..].trim_start()
    } else {
        l
    }
}

fn clean_words(l: &str) -> String {
    let mut words = Vec::new();
    let mut in_code = false;
    for w in l.split_whitespace() {
        // inline code spans are dropped whole
        let ticks = w.matches('`').count();
        if in_code {
            if ticks % 2 == 1 {
                in_code = false;
            }
            continue;
        }
        if w.starts_with('`') {
            if ticks % 2 == 1 {
                in_code = true;
                continue;
            }
            // `word` on its own: keep the word, it is usually a name
            words.push(shorten(w.trim_matches('`')));
            continue;
        }
        let bare = w.trim_matches(|c: char| "*_~[]()<>\"".contains(c));
        if bare.starts_with("http://") || bare.starts_with("https://") {
            words.push("a link".to_string());
            continue;
        }
        if bare.is_empty() {
            continue;
        }
        words.push(shorten(bare));
    }
    words.join(" ")
}

/// A path becomes its last component: "src/app/router.ts," -> "router.ts,".
fn shorten(w: &str) -> String {
    let trail: String = w.chars().rev().take_while(|c| ",.;:!?".contains(*c)).collect();
    let core = &w[..w.len() - trail.len()];
    let core = if core.contains('/') && !core.ends_with('/') {
        core.rsplit('/').next().unwrap_or(core)
    } else {
        core
    };
    format!("{core}{}", trail.chars().rev().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_core_rule_fits_codex_window() {
        let head: String = INSTRUCTIONS.chars().take(512).collect();
        assert!(head.contains("say tool"));
        assert!(head.contains("never code"));
        assert!(INSTRUCTIONS.len() <= 2048, "Claude Code truncates at 2048");
    }

    #[test]
    fn stop_detection_uses_opening_words() {
        assert!(is_stop("Stop."));
        assert!(is_stop("wait, use the other file"));
        assert!(is_stop("hold on"));
        assert!(is_stop("no no, not that one"));
        assert!(!is_stop("can we wait for CI"));
        assert!(!is_stop("stopwatch is broken"));
        assert!(!is_stop("open the router file"));
    }

    #[test]
    fn speakable_strips_markdown_and_paths() {
        let s = speakable(
            "## Done\n- Removed the copy in `src/app.ts`\n- See https://example.com/x\n```rust\nfn x() {}\n```\n1. Tests pass",
        );
        assert_eq!(s, "Done. Removed the copy in app.ts. See a link. Tests pass");
    }

    #[test]
    fn speakable_drops_multiword_code_spans() {
        assert_eq!(speakable("run `npm test -- router` now"), "run now");
    }

    #[test]
    fn opening_takes_two_sentences() {
        assert_eq!(opening("Done. Tests pass. Next I will open a PR."), "Done. Tests pass.");
        assert_eq!(opening("## Result\n- Removed `src/app.ts` copy"), "Result. Removed app.ts copy");
        assert!(opening(&"word ".repeat(200)).len() <= 243);
    }

    #[test]
    fn utterance_format() {
        let u = Utterance {
            id: 3,
            text: "open the router file".into(),
            heard: Some("open the router config, no wait, the router file".into()),
            revises: None,
            interrupted_after: None,
        };
        let s = utterances(&[u]);
        assert!(s.starts_with("[voice u3] open the router file\n(heard: "));
    }
}
