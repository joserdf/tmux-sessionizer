//! Token usage of an agent session, parsed from its transcript, with an
//! incremental per-path cache so multi-MB transcripts are only ever read once.
//!
//! Claude's transcript (JSONL under `~/.claude/projects/*/<id>.jsonl`) carries
//! one `usage` object per content block, so the same message id recurs on
//! consecutive lines; each id is counted once and summed over the file. Codex's
//! transcript (JSONL under `~/.codex/sessions`, `rollout-<id>.jsonl`) reports
//! cumulative `token_count` events; the last one with a non-null `info` wins.
//! OpenCode (SQLite) and Pi (no session id) aren't supported yet.
//!
//! Costing is opt-in: prices are user-supplied USD-per-million-token values in
//! the config (`[pricing."<model-id>"]`); with no price for the session's model
//! `cost` returns `None` rather than inventing numbers that would be wrong.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::agent::AgentKind;
use crate::config::Pricing;

/// Token counts of an agent session, plus the model that produced them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Prompt tokens that were not served from cache.
    pub input: u64,
    /// Completion tokens generated.
    pub output: u64,
    /// Prompt tokens written to cache this request.
    pub cache_write: u64,
    /// Prompt tokens read from cache this request.
    pub cache_read: u64,
    /// The model id (Claude; Codex's transcript doesn't name one).
    pub model: Option<String>,
}

impl TokenUsage {
    /// All tokens attributed to the session, cache hits included.
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

/// Per-path parse state so only bytes appended since the last read are parsed.
#[derive(Debug, Clone, Default)]
struct Cached {
    /// Bytes already parsed.
    offset: u64,
    usage: TokenUsage,
    /// Claude message ids already counted.
    seen: HashSet<String>,
}

/// A cache of parsed transcript prefixes, keyed by transcript path.
pub struct UsageCache(Mutex<HashMap<PathBuf, Cached>>);

impl UsageCache {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    /// Usage for the transcript at `path`, parsing only the bytes appended
    /// since the previous call. A file that shrank resets the cache (its
    /// contents may have been replaced); malformed lines are ignored.
    pub fn transcript(&self, path: &Path, kind: AgentKind) -> Option<TokenUsage> {
        let mut map = self.0.lock().ok()?;
        let mut f = File::open(path).ok()?;
        let len = f.metadata().ok()?.len();
        let (mut offset, mut usage, mut seen) = match map.get(path) {
            Some(c) if c.offset <= len => (c.offset, c.usage.clone(), c.seen.clone()),
            // Not cached yet, or the file was truncated.
            _ => (0, TokenUsage::default(), HashSet::new()),
        };
        if offset < len {
            f.seek(SeekFrom::Start(offset)).ok()?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).ok()?;
            let text = String::from_utf8_lossy(&buf);
            match kind {
                AgentKind::Claude => usage = claude_usage_into(&text, &mut seen, usage),
                AgentKind::Codex => usage = codex_usage_into(&text, usage),
                AgentKind::Pi | AgentKind::OpenCode => return None,
            }
            offset = len;
        }
        map.insert(
            path.to_path_buf(),
            Cached {
                offset,
                usage: usage.clone(),
                seen,
            },
        );
        Some(usage)
    }
}

impl Default for UsageCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide cache, so every caller reuses parsed prefixes.
fn global() -> &'static UsageCache {
    static CACHE: OnceLock<UsageCache> = OnceLock::new();
    CACHE.get_or_init(UsageCache::new)
}

/// Token usage of an agent session by its kind and conversation id, when its
/// transcript can be located and read. OpenCode and Pi return `None`.
pub fn token_usage(kind: AgentKind, id: &str) -> Option<TokenUsage> {
    // The id comes from hook payloads: keep it out of path tricks.
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        return None;
    }
    let home = dirs::home_dir()?;
    let path = match kind {
        AgentKind::Claude => crate::search::claude_transcript(&home, id)?,
        AgentKind::Codex => crate::search::codex_transcript(&home.join(".codex/sessions"), id)?,
        AgentKind::Pi | AgentKind::OpenCode => return None,
    };
    global().transcript(&path, kind)
}

/// Claude usage in `jsonl` added onto `acc`, deduping by message id and keeping
/// the last model seen.
fn claude_usage_into(jsonl: &str, seen: &mut HashSet<String>, mut acc: TokenUsage) -> TokenUsage {
    for d in jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        if d["type"] != "assistant" {
            continue;
        }
        if let Some(m) = d["message"]["model"].as_str() {
            acc.model = Some(m.to_string());
        }
        let Some(id) = d["message"]["id"].as_str() else {
            continue;
        };
        // The same message id repeats once per content block, with identical
        // usage; count it once.
        if !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(u) = claude_block_usage(&d["message"]["usage"]) {
            acc.input += u.input;
            acc.output += u.output;
            acc.cache_write += u.cache_write;
            acc.cache_read += u.cache_read;
        }
    }
    acc
}

/// A single message's usage object, or `None` when it isn't present.
fn claude_block_usage(u: &Value) -> Option<TokenUsage> {
    Some(TokenUsage {
        input: u["input_tokens"].as_u64()?,
        output: u["output_tokens"].as_u64()?,
        cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
        model: None,
    })
}

/// Codex usage in `jsonl`, keeping the last `token_count` whose `info` isn't
/// null; earlier values are replaced (they're cumulative, not additive).
fn codex_usage_into(jsonl: &str, mut acc: TokenUsage) -> TokenUsage {
    for d in jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        if d["type"] != "event_msg" || d["payload"]["type"] != "token_count" {
            continue;
        }
        let Some(info) = d["payload"]["info"].as_object() else {
            continue;
        };
        let Some(total) = info.get("total_token_usage").and_then(|v| v.as_object()) else {
            continue;
        };
        let input = total["input_tokens"].as_u64().unwrap_or(0);
        let cached = total["cached_input_tokens"].as_u64().unwrap_or(0);
        acc.input = input.saturating_sub(cached);
        acc.cache_read = cached;
        acc.output = total["output_tokens"].as_u64().unwrap_or(0);
        // Codex reports cache hits inside input; there is no separate write.
        acc.cache_write = 0;
    }
    acc
}

/// Claude usage summed over a whole transcript.
pub fn claude_usage(jsonl: &str) -> TokenUsage {
    claude_usage_into(jsonl, &mut HashSet::new(), TokenUsage::default())
}

/// Codex usage from the last non-null `token_count` of a whole transcript.
pub fn codex_usage(jsonl: &str) -> TokenUsage {
    codex_usage_into(jsonl, TokenUsage::default())
}

/// Dollar cost of `usage` at `pricing` (USD per million tokens), `None` when
/// the session's model has no price (costing is opt-in).
pub fn cost(usage: &TokenUsage, pricing: &HashMap<String, Pricing>) -> Option<f64> {
    let model = usage.model.as_deref()?;
    let p = pricing.get(model)?;
    let per_million = |t: u64, price: f64| t as f64 / 1_000_000.0 * price;
    Some(
        per_million(usage.input, p.input)
            + per_million(usage.output, p.output)
            + per_million(usage.cache_write, p.cache_write)
            + per_million(usage.cache_read, p.cache_read),
    )
}

/// A compact human token count: `1.2M`, `850k`, `430`.
pub fn human(tokens: u64) -> String {
    match tokens {
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1_000_000.0),
        t if t >= 1_000 => format!("{}k", t / 1_000),
        t => format!("{t}"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::Pricing;

    /// Unique temp directory under [`std::env::temp_dir`], removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "showrunner-usage-test-{}-{}-{tag}",
                std::process::id(),
                nanos
            ));
            fs::create_dir_all(&path).expect("failed to create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn claude_line(id: &str, model: &str, input: u64, out: u64, cw: u64, cr: u64) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{input},"cache_creation_input_tokens":{cw},"cache_read_input_tokens":{cr},"output_tokens":{out}}}}}}}"#
        )
    }

    #[test]
    fn claude_counts_each_message_id_once() {
        // The same id recurs once per content block; it must count once.
        let jsonl = format!(
            "{}\n{}\n{}\n",
            claude_line("msg_1", "claude-opus-5-5", 2, 532, 35223, 24306),
            claude_line("msg_1", "claude-opus-5-5", 2, 532, 35223, 24306),
            claude_line("msg_2", "claude-opus-5-5", 10, 20, 0, 5),
        );
        let u = claude_usage(&jsonl);
        assert_eq!(u.input, 12);
        assert_eq!(u.output, 552);
        assert_eq!(u.cache_write, 35223);
        assert_eq!(u.cache_read, 24311);
        assert_eq!(u.total(), u.input + u.output + u.cache_write + u.cache_read);
        assert_eq!(u.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn claude_ignores_malformed_and_non_assistant_lines() {
        let jsonl = format!(
            "not json\n{}\n{}\n",
            r#"{"type":"user","message":{"id":"u1","usage":{"input_tokens":9}}}"#,
            claude_line("msg_1", "m", 5, 0, 0, 0),
        );
        let u = claude_usage(&jsonl);
        assert_eq!(u.input, 5);
        assert_eq!(u.model.as_deref(), Some("m"));
    }

    #[test]
    fn codex_takes_last_non_null_token_count() {
        let jsonl = r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":200,"output_tokens":25,"total_tokens":325}}}}
{"type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        let u = codex_usage(jsonl);
        assert_eq!(u.input, 100);
        assert_eq!(u.cache_read, 200);
        assert_eq!(u.output, 25);
        assert_eq!(u.cache_write, 0);
        assert_eq!(u.total(), 325);
        assert_eq!(u.model, None);
    }

    #[test]
    fn incremental_parse_appends_and_resets() {
        let dir = TempDir::new("incremental");
        let path = dir.path().join("transcript.jsonl");
        let cache = UsageCache::new();
        let write = |s: &str| fs::write(&path, s).unwrap();

        write(&format!("{}\n", claude_line("msg_1", "m", 10, 5, 0, 0)));
        let first = cache.transcript(&path, AgentKind::Claude).unwrap();
        assert_eq!(first.input, 10);

        // Append another message; only the appended bytes are parsed.
        let appended = format!("{}\n", claude_line("msg_2", "m", 20, 7, 0, 0));
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str(&appended);
        write(&content);
        let second = cache.transcript(&path, AgentKind::Claude).unwrap();
        assert_eq!(second.input, 30);
        assert_eq!(second.output, 12);

        // A duplicate of an already-counted id in the appended tail is skipped.
        let mut content = fs::read_to_string(&path).unwrap();
        content.push_str(&format!("{}\n", claude_line("msg_1", "m", 10, 5, 0, 0)));
        write(&content);
        let third = cache.transcript(&path, AgentKind::Claude).unwrap();
        assert_eq!(third.input, 30);

        // Truncate: the cache resets and re-parses the whole file.
        write(&format!("{}\n", claude_line("msg_9", "m", 100, 1, 0, 0)));
        let reset = cache.transcript(&path, AgentKind::Claude).unwrap();
        assert_eq!(reset.input, 100);
        assert_eq!(reset.output, 1);
    }

    #[test]
    fn cost_is_opt_in() {
        let pricing: HashMap<String, Pricing> = HashMap::from([(
            "claude-opus-5-5".into(),
            Pricing {
                input: 15.0,
                output: 75.0,
                cache_write: 18.75,
                cache_read: 1.5,
            },
        )]);
        let mut usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_write: 1_000_000,
            cache_read: 1_000_000,
            model: Some("claude-opus-5-5".into()),
        };
        assert_eq!(cost(&usage, &pricing), Some(15.0 + 75.0 + 18.75 + 1.5));
        // Unknown model -> no price -> None.
        usage.model = Some("other".into());
        assert_eq!(cost(&usage, &pricing), None);
        // No model -> None.
        usage.model = None;
        assert_eq!(cost(&usage, &pricing), None);
        // Empty pricing -> None.
        assert_eq!(cost(&usage, &HashMap::new()), None);
    }

    #[test]
    fn human_formats_compact() {
        assert_eq!(human(430), "430");
        assert_eq!(human(850_000), "850k");
        assert_eq!(human(1_200_000), "1.2M");
        assert_eq!(human(0), "0");
    }
}
