//! Text that crosses the boundary to the model or to the speaker.

use crate::proto::Utterance;

/// MCP server instructions. Codex prioritizes the first 512 characters and Claude Code truncates
/// at 2048, so the load-bearing rule comes first.
pub const INSTRUCTIONS: &str = "\
When voice mode is on (you receive [voice] messages, or spoken messages delivered as context), talk to the user only through the say tool: \
short spoken sentences, never code, file paths, logs or markdown. Before a long action, say what \
you are about to do. Answer questions directly. Keep tool calls, diffs and results in the session \
as usual; say only what a colleague would say out loud while working.

Details:
- kind: answer (reply to what they asked), status (progress), next (the step you are about to \
take), question (you need their input; then stop and wait).
- One to three sentences per call. Name files by their short name, not the full path. Round \
numbers. Never read code aloud.
- Spoken messages are speech recognition output and may contain recognition errors. When the \
user corrects themselves, the last version wins. Use the repo context to fix misheard names.
- A spoken message that arrives while you work is steering: take it into account right away.
- If the user says stop, stop the current action and say what state things are in.
- say returns anything the user said meanwhile; treat it like a new spoken message.
- The plugin prints the spoken exchange in the session; do not restate it in text.";

/// Reminder added when voice mode is switched on for a running session.
pub const ACTIVATED: &str = "Voice mode is on. Talk to the user through the say tool as described \
in the parlar server instructions: short spoken sentences, no code or paths.";

/// Closes every delivery: models drift back to plain text replies without it.
pub const REPLY_CUE: &str = "(Reply out loud with the say tool.)";

/// Delivery wording for harnesses that present a Stop reason as context rather than as a new
/// user message (Muse's foreign-hook translation): no transcript tags for the model to
/// continue, and the newest speech is named as the only thing to answer.
pub fn latest_spoken(items: &[Utterance]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "The user just spoke. Treat this as their newest message; it supersedes earlier spoken \
         messages. Do not repeat or continue a transcript. They said:",
    );
    if items.len() > 1 {
        out.push_str(
            " (In order; the last is the newest, earlier ones are context it may revise. \
             If they ask distinct things, answer each.)",
        );
    }
    if items.iter().any(|u| u.text.trim().starts_with(ACTIVATED)) {
        out.push_str("\nVoice mode was just switched on for this session.");
    }
    for u in items {
        let text = u.text.trim();
        let text = text.strip_prefix(ACTIVATED).map_or(text, |rest| rest.trim_start());
        out.push_str(&format!("\n\"{}\"", text.trim()));
        if u.revises.is_some() {
            out.push_str(" (This continues or corrects their previous spoken message; this newest wording wins.)");
        }
        if let Some(cut) = u.interrupted_after.as_ref().filter(|c| !c.trim().is_empty()) {
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
    out.push_str("\nAnswer this newest message now out loud with the say tool.");
    out
}

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
        if let Some(cut) = u.interrupted_after.as_ref().filter(|c| !c.trim().is_empty()) {
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
        let mut end = 240;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        let cut = out[..end].rfind(' ').unwrap_or(end);
        out.truncate(cut);
        out.push_str("...");
    }
    out.trim().to_string()
}

const STOP_WORDS: &[&str] = &[
    "stop",
    "wait",
    "hold on",
    "hold it",
    "cancel",
    "abort",
    "don't",
    "do not",
    "no no",
    "halt",
    "pause",
    "never mind",
    "nevermind",
];

/// True when an utterance reads as a request to stop the current action. Only the opening words
/// count: "wait, use the other file" is steering that starts with a stop, and it still should
/// block the next tool call, but "can we wait for CI" should not.
pub fn is_stop(text: &str) -> bool {
    let t = text.trim().trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    STOP_WORDS.iter().any(|w| {
        t.strip_prefix(w).is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| !c.is_alphanumeric()))
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
    if digits > 0 && l[digits..].starts_with(['.', ')']) { l[digits + 1..].trim_start() } else { l }
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
    let core = if core.contains('/') && !core.ends_with('/') { core.rsplit('/').next().unwrap_or(core) } else { core };
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
        // a multibyte character across the cut must not panic
        assert!(opening(&format!("{}é{}", "a".repeat(239), " b".repeat(20))).len() <= 243);
        assert!(opening(&"日本語".repeat(40)).ends_with("..."));
    }

    #[test]
    fn latest_spoken_is_an_instruction_not_a_transcript() {
        let u = Utterance {
            id: 7,
            text: format!("{ACTIVATED}\nwhat are we doing today"),
            heard: None,
            revises: Some(6),
            interrupted_after: None,
        };
        let s = latest_spoken(&[u]);
        assert!(s.contains("newest message"), "{s}");
        assert!(s.contains("\"what are we doing today\""), "{s}");
        assert!(s.contains("newest wording wins"), "{s}");
        assert!(!s.contains("[voice"), "transcript tags invite continuation: {s}");
        assert!(!s.contains(ACTIVATED), "activation rides MCP instructions already: {s}");
        assert!(s.contains("Voice mode was just switched on"), "activation is still named: {s}");
        assert!(latest_spoken(&[]).is_empty());
    }

    #[test]
    fn latest_spoken_renders_heard_interruptions_and_batches() {
        let items = vec![
            Utterance {
                id: 1,
                text: "first question".into(),
                heard: Some("first qestion".into()),
                revises: None,
                interrupted_after: Some("I was saying".into()),
            },
            Utterance { id: 2, text: "second question".into(), heard: None, revises: None, interrupted_after: None },
        ];
        let s = latest_spoken(&items);
        assert!(s.contains("\"first question\""), "{s}");
        assert!(s.contains("heard: \"first qestion\""), "{s}");
        assert!(s.contains("they talked over you; you had said: \"I was saying\""), "{s}");
        assert!(s.contains("\"second question\""), "{s}");
        assert!(s.contains("If they ask distinct things, answer each"), "{s}");
        assert!(!s.contains("Voice mode was just switched on"), "{s}");
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
