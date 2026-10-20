//! Token usage of an agent session, parsed from its transcript, with an
//! incremental per-path cache so multi-MB transcripts are only ever read once.
//!
//! - **Claude** (JSONL under `~/.claude/projects/*/<id>.jsonl`) carries one
//!   `usage` object per content block, so the same message id recurs on
//!   consecutive lines; each id is counted once and summed over the file.
//! - **Codex** (JSONL under `~/.codex/sessions`, `rollout-<id>.jsonl`) reports
//!   cumulative `token_count` events; the last one with a non-null `info` wins.
//! - **OpenCode** (SQLite at `$XDG_DATA_HOME/opencode/opencode.db`) keeps
//!   aggregated token columns on its `session` table, read via the `sqlite3`
//!   CLI; the model is the session's, or the latest assistant message's. This
//!   also covers Carcará (`carcara-coder`, an OpenCode pointed at the Carcará
//!   endpoint).
//! - **Pi** (JSONL under `<agent-dir>/sessions/--<encoded-cwd>--/*.jsonl`)
//!   records assistant `message.usage`; summed over the file.
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
                AgentKind::Pi => usage = pi_usage_into(&text, &mut seen, usage),
                AgentKind::OpenCode | AgentKind::Custom(_) => return None,
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

/// The OpenCode cache: keyed by (db path, session id, time_updated), since
/// usage only changes when the session row is updated.
type OpcodeCache = Mutex<HashMap<(PathBuf, String, i64), TokenUsage>>;

/// The process-wide OpenCode cache.
fn opencode_global() -> &'static OpcodeCache {
    static CACHE: OnceLock<OpcodeCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Token usage of an agent session by its kind, conversation id (when known)
/// and pane cwd (the fallback key for OpenCode and Pi, whose hooks carry no
/// id), when its transcript can be located and read.
pub fn token_usage(kind: AgentKind, id: Option<&str>, cwd: Option<&str>) -> Option<TokenUsage> {
    match kind {
        // Claude and Codex transcripts are keyed by conversation id.
        AgentKind::Claude | AgentKind::Codex => {
            let id = valid_id(id?)?;
            let home = dirs::home_dir()?;
            let path = match kind {
                AgentKind::Claude => crate::search::claude_transcript(&home, id)?,
                _ => crate::search::codex_transcript(&home.join(".codex/sessions"), id)?,
            };
            global().transcript(&path, kind)
        }
        // OpenCode is keyed by session id, else the latest session in the cwd.
        AgentKind::OpenCode => opencode_usage_at(&opencode_db()?, id, cwd),
        // Pi is keyed by cwd (its session file lives in an encoded-cwd dir).
        AgentKind::Pi => {
            let home = dirs::home_dir()?;
            let path = pi_transcript(&home, cwd?)?;
            global().transcript(&path, kind)
        }
        // TOML-defined agents have no known transcript format.
        AgentKind::Custom(_) => None,
    }
}

/// A conversation id is safe to interpolate into a transcript path or query
/// only when it stays within a strict character whitelist.
fn valid_id(id: &str) -> Option<&str> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        None
    } else {
        Some(id)
    }
}

// ---- OpenCode (SQLite via the `sqlite3` CLI) ------------------------------

/// The OpenCode database path: `$XDG_DATA_HOME/opencode/opencode.db`, else
/// `~/.local/share/opencode/opencode.db`. `None` when the db is missing.
fn opencode_db() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME")
        && !dir.is_empty()
    {
        let p = PathBuf::from(dir).join("opencode/opencode.db");
        if p.exists() {
            return Some(p);
        }
    }
    let p = dirs::home_dir()?.join(".local/share/opencode/opencode.db");
    p.exists().then_some(p)
}

/// Usage for an OpenCode session read from `db`, cached by (db, id,
/// time_updated). `id` wins when present; otherwise the most recently updated,
/// non-archived, top-level session whose `directory` equals `cwd`.
fn opencode_usage_at(db: &Path, id: Option<&str>, cwd: Option<&str>) -> Option<TokenUsage> {
    let session = opencode_session(db, id, cwd)?;
    let key = (db.to_path_buf(), session.id.clone(), session.time_updated);
    let mut cache = opencode_global().lock().ok()?;
    if let Some(u) = cache.get(&key) {
        return Some(u.clone());
    }
    // Reasoning tokens count as output (they're billed that way).
    let model = if session.model.is_empty() {
        opencode_model_from_message(db, &session.id)
    } else {
        Some(session.model)
    };
    let usage = TokenUsage {
        input: session.input,
        output: session.output,
        cache_write: session.cache_write,
        cache_read: session.cache_read,
        model,
    };
    cache.insert(key, usage.clone());
    Some(usage)
}

/// A session row: its aggregated token columns plus the model id.
struct OpcodeSession {
    id: String,
    time_updated: i64,
    model: String,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
}

/// The OpenCode `session` row for `id`, or for the latest matching `cwd`.
fn opencode_session(db: &Path, id: Option<&str>, cwd: Option<&str>) -> Option<OpcodeSession> {
    let cols = "id, time_updated, model, tokens_input, tokens_output, \
                tokens_reasoning, tokens_cache_read, tokens_cache_write";
    let query = match id {
        Some(id) if valid_id(id).is_some() => format!("SELECT {cols} FROM session WHERE id = '{id}'"),
        _ => {
            let dir = sql_escape(cwd?);
            format!(
                "SELECT {cols} FROM session WHERE directory = '{dir}' \
                 AND parent_id IS NULL AND (time_archived IS NULL OR time_archived = 0) \
                 ORDER BY time_updated DESC LIMIT 1"
            )
        }
    };
    let rows = sqlite_json(db, &query)?.as_array()?.clone();
    let row = rows.first()?;
    Some(OpcodeSession {
        id: row["id"].as_str()?.to_string(),
        time_updated: row["time_updated"].as_i64().unwrap_or(0),
        model: row["model"].as_str().unwrap_or("").to_string(),
        input: row["tokens_input"].as_u64().unwrap_or(0),
        output: row["tokens_output"].as_u64().unwrap_or(0)
            + row["tokens_reasoning"].as_u64().unwrap_or(0),
        cache_write: row["tokens_cache_write"].as_u64().unwrap_or(0),
        cache_read: row["tokens_cache_read"].as_u64().unwrap_or(0),
    })
}

/// The model id of the latest assistant message of a session, when the session
/// row's `model` is empty.
fn opencode_model_from_message(db: &Path, session_id: &str) -> Option<String> {
    let query = format!(
        "SELECT data FROM message WHERE session_id = '{session_id}' \
         AND json_valid(data) AND json_extract(data, '$.role') = 'assistant' \
         ORDER BY time_updated DESC LIMIT 1"
    );
    let row = sqlite_json(db, &query)?.as_array()?.first()?.clone();
    let data: Value = serde_json::from_str(row["data"].as_str()?).ok()?;
    data["modelID"].as_str().map(str::to_string)
}

/// Run `sqlite3 -readonly -json` and parse its output; `None` if the CLI is
/// missing or the query fails.
fn sqlite_json(db: &Path, query: &str) -> Option<Value> {
    let out = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg("-json")
        .arg(db)
        .arg(query)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Double single quotes so a path can be interpolated into a SQL literal.
fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

// ---- Pi (JSONL under an encoded-cwd session dir) ---------------------------

/// The Pi agent dir: `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
fn pi_dir(home: &Path) -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/agent"))
}

/// The current Pi session file for `cwd`: the newest `.jsonl` (by mtime) in
/// the encoded-cwd session directory.
fn pi_transcript(home: &Path, cwd: &str) -> Option<PathBuf> {
    let dir = pi_dir(home).join("sessions").join(encode_pi_cwd(cwd));
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_file() && e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| e.path())
        .collect();
    files.sort_by_key(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()));
    files.into_iter().last()
}

/// Encode a cwd into Pi's safe session-dir name: strip one leading slash,
/// replace `/`, `\` and `:` with `-`, wrap in `--…--`.
fn encode_pi_cwd(cwd: &str) -> String {
    let stripped = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
    format!("--{}--", stripped.replace(['/', '\\', ':'], "-"))
}

/// Pi usage in `jsonl` added onto `acc`: assistant messages' `message.usage`,
/// deduped by entry id, keeping the last model seen.
fn pi_usage_into(jsonl: &str, seen: &mut HashSet<String>, mut acc: TokenUsage) -> TokenUsage {
    for d in jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        if d["type"] != "message" || d["message"]["role"] != "assistant" {
            continue;
        }
        let Some(id) = d["id"].as_str() else {
            continue;
        };
        if !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(m) = d["message"]["model"].as_str() {
            acc.model = Some(m.to_string());
        }
        let u = &d["message"]["usage"];
        acc.input += u["input"].as_u64().unwrap_or(0);
        acc.output += u["output"].as_u64().unwrap_or(0);
        acc.cache_write += u["cacheWrite"].as_u64().unwrap_or(0);
        acc.cache_read += u["cacheRead"].as_u64().unwrap_or(0);
    }
    acc
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

/// Pi usage summed over a whole transcript.
pub fn pi_usage(jsonl: &str) -> TokenUsage {
    pi_usage_into(jsonl, &mut HashSet::new(), TokenUsage::default())
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

    #[test]
    fn pi_sums_assistant_message_usage() {
        // Format per @mariozechner/pi-coding-agent 0.73.1
        // (dist/core/session-manager.js:580-588, dist/core/agent-session.js:2347-2356).
        let jsonl = r#"{"type":"session","id":"s1","message":{"role":"system"}}
{"type":"message","id":"m1","parentId":"s1","message":{"role":"user","content":"hi"}}
{"type":"message","id":"m2","parentId":"m1","message":{"role":"assistant","model":"grok-4","provider":"xai","usage":{"input":100,"output":50,"cacheRead":200,"cacheWrite":10,"cost":{"total":0.1}}}}
{"type":"message","id":"m3","parentId":"m2","message":{"role":"assistant","model":"grok-4","provider":"xai","usage":{"input":30,"output":20,"cacheRead":0,"cacheWrite":0}}}
not json"#;
        let u = pi_usage(jsonl);
        assert_eq!(u.input, 130);
        assert_eq!(u.output, 70);
        assert_eq!(u.cache_read, 200);
        assert_eq!(u.cache_write, 10);
        assert_eq!(u.total(), 130 + 70 + 200 + 10);
        assert_eq!(u.model.as_deref(), Some("grok-4"));
    }

    #[test]
    fn pi_cwd_encodes_into_safe_dir_name() {
        // session-manager.js:210-214 strips one leading slash and replaces
        // `/`, `\`, `:` with `-`, wrapped in `--…--`.
        assert_eq!(encode_pi_cwd("/home/user/proj"), "--home-user-proj--");
        assert_eq!(encode_pi_cwd("C:/Users/me"), "--C--Users-me--");
        assert_eq!(encode_pi_cwd("/a/b"), "--a-b--");
    }

    /// Create a temp OpenCode-style db via the `sqlite3` CLI; returns None and
    /// skips the test when sqlite3 isn't on PATH.
    fn make_opencode_db(dir: &Path) -> Option<PathBuf> {
        let db = dir.join("opencode.db");
        let sql = r#"
CREATE TABLE session (
  id TEXT PRIMARY KEY, directory TEXT, time_updated INTEGER, time_archived INTEGER,
  parent_id TEXT, model TEXT, tokens_input INTEGER, tokens_output INTEGER,
  tokens_reasoning INTEGER, tokens_cache_read INTEGER, tokens_cache_write INTEGER
);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_updated INTEGER, data TEXT);
INSERT INTO session VALUES ('s1','/work/proj',1000,0,NULL,'deepseek-x',380,530,0,69088,0);
INSERT INTO session VALUES ('s2','/work/proj',2000,0,NULL,NULL,10,5,2,3,1);
INSERT INTO session VALUES ('s3','/work/proj',3000,999,NULL,'archived-model',1,1,0,0,0);
INSERT INTO session VALUES ('s4','/other',100,0,'s1','child',5,5,0,0,0);
INSERT INTO message VALUES ('m1','s2',2000,'{"role":"assistant","modelID":"fallback-model","tokens":{}}');
"#;
        let out = std::process::Command::new("sqlite3").arg(&db).arg(sql).output().ok()?;
        out.status.success().then_some(db)
    }

    #[test]
    fn opencode_reads_session_token_columns() {
        let dir = TempDir::new("opencode");
        let Some(db) = make_opencode_db(dir.path()) else {
            eprintln!("sqlite3 unavailable; skipping");
            return;
        };
        // By id: aggregated columns, model from the session row.
        let u = opencode_usage_at(&db, Some("s1"), None).unwrap();
        assert_eq!(u.input, 380);
        assert_eq!(u.output, 530);
        assert_eq!(u.cache_read, 69088);
        assert_eq!(u.cache_write, 0);
        assert_eq!(u.model.as_deref(), Some("deepseek-x"));
        // Reasoning counts as output.
        let u2 = opencode_usage_at(&db, Some("s2"), None).unwrap();
        assert_eq!(u2.output, 5 + 2);
        // Empty session model falls back to the latest assistant message's.
        assert_eq!(u2.model.as_deref(), Some("fallback-model"));
    }

    #[test]
    fn opencode_cwd_fallback_picks_latest_non_archived_top_level() {
        let dir = TempDir::new("opencode");
        let Some(db) = make_opencode_db(dir.path()) else {
            eprintln!("sqlite3 unavailable; skipping");
            return;
        };
        // Most recently updated matching directory, skipping archived (s3) and
        // child (s4) sessions.
        let u = opencode_usage_at(&db, None, Some("/work/proj")).unwrap();
        assert_eq!(u.input, 10);
        assert_eq!(u.output, 7);
        // No match in an unrelated directory.
        assert!(opencode_usage_at(&db, None, Some("/nowhere")).is_none());
    }

    #[test]
    fn opencode_rejects_non_whitelisted_session_ids() {
        let dir = TempDir::new("opencode");
        let Some(db) = make_opencode_db(dir.path()) else {
            eprintln!("sqlite3 unavailable; skipping");
            return;
        };
        // A crafted id must not reach the query; falls back to cwd lookup.
        assert!(opencode_usage_at(&db, Some("1; DROP TABLE session"), None).is_none());
    }
}
