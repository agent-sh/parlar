//! Words from the focused session's repo: file and directory names bias the recognizer, and a
//! spoken "router dot ts" is written back as `router.ts` when that file exists.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

/// Upper bound on biasing terms. Moonshine's context picker defaults to 200; a shorter list keeps
/// the boost focused on the names that matter.
const MAX_TERMS: usize = 150;

#[derive(Default, Clone)]
pub struct Vocab {
    /// Bumped on every change so the listener knows to reload keyterms.
    pub version: u64,
    pub terms: Vec<String>,
    files: HashSet<String>,
}

impl Vocab {
    pub fn empty(version: u64) -> Vocab {
        Vocab { version, ..Default::default() }
    }

    pub fn from_dir(dir: &Path, version: u64) -> Vocab {
        let paths = git_files(dir).unwrap_or_else(|| walk(dir));
        let mut seen = HashSet::new();
        let mut ranked: Vec<(usize, String)> = Vec::new();
        let mut files = HashSet::new();
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            seen.insert(name.to_lowercase());
            ranked.push((0, name.to_string()));
        }
        for p in &paths {
            let parts: Vec<&str> = p.split('/').collect();
            for (depth, part) in parts.iter().enumerate() {
                if part.is_empty() || part.starts_with('.') {
                    continue;
                }
                let is_file = depth == parts.len() - 1;
                if is_file {
                    files.insert(part.to_lowercase());
                }
                if seen.insert(part.to_lowercase()) {
                    ranked.push((depth + 1, part.to_string()));
                }
                if is_file {
                    if let Some((stem, _)) = part.rsplit_once('.') {
                        if stem.len() > 2 && seen.insert(stem.to_lowercase()) {
                            ranked.push((depth + 1, stem.to_string()));
                        }
                    }
                }
            }
        }
        // shallow names first: they are the ones people say
        ranked.sort_by_key(|(d, _)| *d);
        let terms = ranked
            .into_iter()
            .map(|(_, t)| t)
            .filter(|t| t.len() > 2 && !t.contains(',') && t.chars().any(|c| c.is_alphabetic()))
            .take(MAX_TERMS)
            .collect();
        Vocab { version, terms, files }
    }

    pub fn keyterms(&self) -> String {
        self.terms.join(",")
    }

    /// "open router dot ts" -> "open router.ts", only for files that exist.
    pub fn join_dots(&self, text: &str) -> String {
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut out: Vec<String> = Vec::new();
        let mut i = 0;
        while i < words.len() {
            if i + 2 < words.len() && bare(words[i + 1]) == "dot" {
                let ext_raw = words[i + 2];
                let trail: String = ext_raw.chars().rev().take_while(|c| ",.;:!?".contains(*c)).collect();
                let ext = bare(ext_raw);
                let name = format!("{}.{}", bare(words[i]), ext);
                if self.files.contains(&name) {
                    out.push(format!("{name}{}", trail.chars().rev().collect::<String>()));
                    i += 3;
                    continue;
                }
            }
            out.push(words[i].to_string());
            i += 1;
        }
        out.join(" ")
    }
}

fn bare(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-').to_lowercase()
}

fn git_files(dir: &Path) -> Option<Vec<String>> {
    let out = Command::new("git").arg("-C").arg(dir).args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.split('\0').filter(|p| !p.is_empty()).take(20000).map(str::to_string).collect())
}

/// Fallback for a directory that is not a git repo: three levels, skipping build output.
fn walk(dir: &Path) -> Vec<String> {
    const SKIP: &[&str] = &["target", "node_modules", "dist", "build", "__pycache__"];
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new(), 0)];
    while let Some((d, prefix, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if depth < 2 {
                    stack.push((e.path(), rel, depth + 1));
                }
            } else {
                out.push(rel);
            }
            if out.len() > 5000 {
                return out;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab(paths: &[&str]) -> Vocab {
        let dir = tempfile::tempdir().unwrap();
        for p in paths {
            let f = dir.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "").unwrap();
        }
        Vocab::from_dir(dir.path(), 1)
    }

    #[test]
    fn spoken_dots_join_only_for_real_files() {
        let v = vocab(&["src/router.ts", "src/app.ts"]);
        assert_eq!(v.join_dots("open router dot ts, then app dot ts."), "open router.ts, then app.ts.");
        assert_eq!(v.join_dots("open config dot ts"), "open config dot ts");
    }

    #[test]
    fn terms_prefer_shallow_names() {
        let v = vocab(&["README.md", "src/deep/thing.rs", "src/router.ts"]);
        let i_src = v.terms.iter().position(|t| t == "src").unwrap();
        let i_thing = v.terms.iter().position(|t| t == "thing").unwrap();
        assert!(i_src < i_thing);
        assert!(v.terms.contains(&"router.ts".to_string()));
    }
}
