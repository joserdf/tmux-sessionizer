//! `search_sessions`: rank windows by how well a free-text query matches what
//! they are about: names, the agent's title, category, branch, cwd, recent
//! output and the agent's own transcript (Claude, Codex; OpenCode keeps its
//! in SQLite and Pi has no session id, so theirs aren't read). Plain word
//! matching; the calling model does the semantic part by phrasing queries
//! and reading the snippets.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agent::AgentKind;
use crate::control::{Inventory, capture};

/// Weights by field: what a window is named matters most.
const NAME: f64 = 4.0;
const BRANCH: f64 = 3.0;
const CATEGORY: f64 = 3.0;
const CWD: f64 = 2.0;
const TEXT: f64 = 1.0;
/// Bytes read from the end of a transcript.
const TRANSCRIPT_TAIL: u64 = 8 << 20;
/// Lines of recent output searched per window.
const OUTPUT_LINES: usize = 200;

/// Lowercase words of at least two characters.
pub fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect()
}

/// Score `fields` (weight, text) against query `terms`: each term adds the
/// weight of every field holding it as a word (half for a word it starts),
/// and the total grows with the share of terms found. Returns the score and
/// the terms found.
pub fn score(terms: &[String], fields: &[(f64, &str)]) -> (f64, Vec<String>) {
    let words: Vec<(f64, Vec<String>)> = fields.iter().map(|(w, t)| (*w, tokens(t))).collect();
    let mut total = 0.0;
    let mut found = Vec::new();
    for term in terms {
        let mut s = 0.0;
        for (w, toks) in &words {
            if toks.iter().any(|t| t == term) {
                s += w;
            } else if toks.iter().any(|t| t.starts_with(term.as_str())) {
                s += w / 2.0;
            }
        }
        if s > 0.0 {
            total += s;
            found.push(term.clone());
        }
    }
    if terms.is_empty() {
        return (0.0, found);
    }
    let coverage = found.len() as f64 / terms.len() as f64;
    (total * (0.5 + coverage), found)
}

/// The line of `texts` holding the most query terms, cut to about 200 chars.
fn snippet(terms: &[String], texts: &[&str]) -> Option<String> {
    let (hits, line) = texts
        .iter()
        .flat_map(|t| t.lines())
        .map(|l| {
            let toks = tokens(l);
            (terms.iter().filter(|t| toks.contains(t)).count(), l)
        })
        .max_by_key(|(n, _)| *n)?;
    if hits == 0 {
        return None;
    }
    let line = line.trim();
    let lower = line.to_lowercase();
    let at = terms
        .iter()
        .filter_map(|t| lower.find(t.as_str()))
        .min()
        .unwrap_or(0);
    let start = line
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i + 80 <= at)
        .last()
        .unwrap_or(0);
    Some(line[start..].chars().take(200).collect())
}

/// Titles and message text of a Claude transcript (JSONL).
pub fn claude_text(jsonl: &str) -> (Vec<String>, String) {
    let mut titles = Vec::new();
    let mut body = String::new();
    for d in jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        match d["type"].as_str() {
            Some("custom-title") => push_str(&mut titles, &d["customTitle"]),
            Some("summary") => push_str(&mut titles, &d["summary"]),
            Some("user" | "assistant") => match &d["message"]["content"] {
                Value::String(s) if !s.starts_with('<') => add_line(&mut body, s),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .filter(|s| !s.starts_with('<'))
                    .for_each(|s| add_line(&mut body, s)),
                _ => {}
            },
            _ => {}
        }
    }
    titles.dedup();
    (titles, body)
}

/// Message text of a Codex rollout (JSONL).
pub fn codex_text(jsonl: &str) -> String {
    let mut body = String::new();
    for d in jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        let p = &d["payload"];
        if d["type"] == "event_msg"
            && matches!(p["type"].as_str(), Some("user_message" | "agent_message"))
            && let Some(m) = p["message"].as_str()
        {
            add_line(&mut body, m);
        }
    }
    body
}

fn push_str(out: &mut Vec<String>, v: &Value) {
    if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
        out.push(s.to_string());
    }
}

fn add_line(body: &mut String, s: &str) {
    body.push_str(s);
    body.push('\n');
}

/// The last [`TRANSCRIPT_TAIL`] bytes of a file, from a line start.
fn read_tail(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let skip = len.saturating_sub(TRANSCRIPT_TAIL);
    f.seek(SeekFrom::Start(skip)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Some(match (skip, text.find('\n')) {
        (0, _) | (_, None) => text,
        (_, Some(i)) => text[i + 1..].to_string(),
    })
}

pub(crate) fn claude_transcript(home: &Path, id: &str) -> Option<PathBuf> {
    std::fs::read_dir(home.join(".claude/projects"))
        .ok()?
        .flatten()
        .map(|d| d.path().join(format!("{id}.jsonl")))
        .find(|p| p.is_file())
}

pub(crate) fn codex_transcript(dir: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{id}.jsonl");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = codex_transcript(&path, id) {
                return Some(found);
            }
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&suffix))
        {
            return Some(path);
        }
    }
    None
}

/// Titles and text of an agent's transcript, when it can be read.
fn transcript(kind: AgentKind, id: &str) -> Option<(Vec<String>, String)> {
    // The id comes from hook payloads: keep it out of path tricks.
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return None;
    }
    let home = dirs::home_dir()?;
    match kind {
        AgentKind::Claude => Some(claude_text(&read_tail(&claude_transcript(&home, id)?)?)),
        AgentKind::Codex => {
            let path = codex_transcript(&home.join(".codex/sessions"), id)?;
            Some((Vec::new(), codex_text(&read_tail(&path)?)))
        }
        AgentKind::Pi | AgentKind::OpenCode => None,
    }
}

/// The `limit` windows best matching `query`, best first.
pub fn search(inv: &Inventory, query: &str, limit: usize, now: u64) -> Value {
    let terms = {
        let mut t = tokens(query);
        t.dedup();
        t
    };
    if terms.is_empty() {
        return json!({"error": "query has no words of two or more characters"});
    }
    let mut hits: Vec<(f64, Value)> = Vec::new();
    for p in inv.windows() {
        let st = inv.states.get(&p.id);
        let branch = git_branch(&p.cwd).unwrap_or_default();
        let output = capture(&p.id, OUTPUT_LINES).unwrap_or_default();
        let (titles, body) = st
            .and_then(|s| {
                let kind = AgentKind::from_id(s.agent.as_deref()?)?;
                transcript(kind, s.agent_session_id.as_deref()?)
            })
            .unwrap_or_default();
        let titles = titles.join("\n");
        let fields = [
            (NAME, p.session.as_str()),
            (NAME, p.window_name.as_str()),
            (
                NAME,
                if st.is_some_and(|s| s.agent.is_some()) {
                    p.title.as_str()
                } else {
                    ""
                },
            ),
            (NAME, titles.as_str()),
            (BRANCH, branch.as_str()),
            (CATEGORY, p.category.as_str()),
            (CWD, p.cwd.as_str()),
            (TEXT, output.as_str()),
            (TEXT, body.as_str()),
        ];
        let (s, found) = score(&terms, &fields);
        if s <= 0.0 {
            continue;
        }
        let mut v = inv.window_json(p, now);
        v["score"] = json!((s * 10.0).round() / 10.0);
        v["matched"] = json!(found);
        if !branch.is_empty() {
            v["branch"] = json!(branch);
        }
        if !titles.is_empty() {
            v["transcript_title"] = json!(titles.lines().last());
        }
        if let Some(sn) = snippet(&terms, &[&output, &body]) {
            v["snippet"] = json!(sn);
        }
        hits.push((s, v));
    }
    hits.sort_by(|a, b| b.0.total_cmp(&a.0));
    json!(
        hits.into_iter()
            .take(limit.clamp(1, 50))
            .map(|(_, v)| v)
            .collect::<Vec<_>>()
    )
}

fn git_branch(cwd: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(q: &str) -> Vec<String> {
        tokens(q)
    }

    #[test]
    fn names_outweigh_output_and_coverage_counts() {
        let q = terms("auth login");
        let (named, found) = score(&q, &[(NAME, "auth-service"), (TEXT, "login page")]);
        assert_eq!(found, ["auth", "login"]);
        let (output_only, _) = score(&q, &[(TEXT, "auth login")]);
        assert!(named > output_only);
        let (half, found) = score(&q, &[(NAME, "auth")]);
        assert_eq!(found, ["auth"]);
        assert!(half < named);
        // A word the term only starts counts half.
        let (prefix, _) = score(&terms("auth"), &[(NAME, "authentication")]);
        let (exact, _) = score(&terms("auth"), &[(NAME, "auth")]);
        assert_eq!(prefix * 2.0, exact);
        assert_eq!(score(&q, &[(NAME, "billing")]).0, 0.0);
    }

    #[test]
    fn snippet_is_the_line_with_most_terms() {
        let q = terms("deploy staging");
        let text = "nothing\ndeploy failed\nretry deploy to staging now\n";
        assert_eq!(snippet(&q, &[text]).unwrap(), "retry deploy to staging now");
        assert!(snippet(&q, &["unrelated"]).is_none());
    }

    #[test]
    fn claude_transcripts_keep_titles_and_prose() {
        let jsonl = r#"{"type":"custom-title","customTitle":"Fix login bug"}
{"type":"user","message":{"content":"the login form rejects emails"}}
{"type":"user","message":{"content":"<command-name>/clear</command-name>"}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"secret tool output"}]}}
{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Found the regex."}]}}
{"type":"custom-title","customTitle":"Fix login bug"}
not json"#;
        let (titles, body) = claude_text(jsonl);
        assert_eq!(titles, ["Fix login bug"]);
        assert_eq!(body, "the login form rejects emails\nFound the regex.\n");
    }

    #[test]
    fn codex_rollouts_keep_user_and_agent_messages() {
        let jsonl = r#"{"type":"session_meta","payload":{"id":"x"}}
{"type":"event_msg","payload":{"type":"user_message","message":"add retries"}}
{"type":"event_msg","payload":{"type":"token_count"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"Added retries."}}"#;
        assert_eq!(codex_text(jsonl), "add retries\nAdded retries.\n");
    }

    #[test]
    fn transcript_ids_cannot_escape() {
        assert!(transcript(AgentKind::Claude, "../../etc/passwd").is_none());
        assert!(transcript(AgentKind::Claude, "").is_none());
    }
}
