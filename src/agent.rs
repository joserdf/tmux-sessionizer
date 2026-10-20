//! Agent harness profiles: which coding-agent CLI a session runs and the
//! agent-specific bits of launching, resuming, and observing it. Everything
//! else (tmux, worktrees, status polling) is agent-agnostic.

use std::fmt;
use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
    Pi,
    OpenCode,
    /// A TOML-defined agent: an index into [`defs`].
    Custom(u8),
}

/// Ids of the agents built into the code.
const BUILTIN_IDS: [&str; 4] = ["claude", "codex", "pi", "opencode"];

/// An agent defined in TOML (see `src/agents.toml` for the fields). It has no
/// hooks or transcript parsing: its status comes from the pane alone.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDef {
    pub id: String,
    pub label: String,
    #[serde(default = "default_icon")]
    pub icon: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub prompt_args: Vec<String>,
    #[serde(default)]
    pub resume: Vec<String>,
    #[serde(default)]
    pub process_name: Option<String>,
    #[serde(default)]
    pub attention_markers: Vec<String>,
    #[serde(default)]
    pub prompt_chrome: Vec<String>,
}

fn default_icon() -> String {
    "\u{25c6}".to_string() // ◆
}

#[derive(Deserialize)]
struct AgentsFile {
    #[serde(default)]
    agent: Vec<AgentDef>,
}

fn parse_defs(text: &str) -> anyhow::Result<Vec<AgentDef>> {
    Ok(toml::from_str::<AgentsFile>(text)?.agent)
}

/// `extra` over `base` by id. Entries reusing a built-in id or without a
/// command are skipped.
fn merge_defs(mut base: Vec<AgentDef>, extra: Vec<AgentDef>) -> Vec<AgentDef> {
    for def in extra {
        if BUILTIN_IDS.contains(&def.id.as_str()) || def.command.is_empty() {
            eprintln!("agents.toml: skipping agent '{}' (built-in id or empty command)", def.id);
            continue;
        }
        match base.iter_mut().find(|d| d.id == def.id) {
            Some(slot) => *slot = def,
            None => base.push(def),
        }
    }
    base
}

/// The TOML-defined agents: the bundled `agents.toml`, overridden and extended
/// by `~/.showrunner/agents.toml` (not read under test). Loaded once.
pub fn defs() -> &'static [AgentDef] {
    static DEFS: OnceLock<Vec<AgentDef>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let bundled = parse_defs(include_str!("agents.toml")).expect("bundled agents.toml");
        let user = if cfg!(test) { Vec::new() } else { user_defs() };
        let mut defs = merge_defs(merge_defs(Vec::new(), bundled), user);
        defs.truncate(usize::from(u8::MAX));
        defs
    })
}

fn user_defs() -> Vec<AgentDef> {
    let path = crate::config::Config::config_path().with_file_name("agents.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_defs(&text).unwrap_or_else(|e| {
        eprintln!("{}: {e:#}", path.display());
        Vec::new()
    })
}

impl AgentKind {
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "pi" => Some(AgentKind::Pi),
            "opencode" => Some(AgentKind::OpenCode),
            _ => defs()
                .iter()
                .position(|d| d.id == id)
                .map(|i| AgentKind::Custom(i as u8)),
        }
    }

    /// The TOML definition of a custom agent; `None` for built-ins.
    pub fn def(self) -> Option<&'static AgentDef> {
        match self {
            AgentKind::Custom(i) => defs().get(usize::from(i)),
            _ => None,
        }
    }

    /// Every agent: the built-ins, then the TOML-defined ones.
    pub fn all() -> Vec<AgentKind> {
        let custom = (0..defs().len()).map(|i| AgentKind::Custom(i as u8));
        Self::ALL.iter().copied().chain(custom).collect()
    }

    /// Ids accepted in config (`default_agent`) and on `--agent`.
    pub fn ids() -> Vec<&'static str> {
        Self::all().into_iter().map(Self::id).collect()
    }

    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Pi => "pi",
            AgentKind::OpenCode => "opencode",
            AgentKind::Custom(_) => self.def().map_or("?", |d| d.id.as_str()),
        }
    }

    /// Single-cell glyph identifying the harness in session lists.
    pub fn icon(self) -> &'static str {
        match self {
            AgentKind::Claude => "\u{273b}", // ✻ — Claude's asterisk motif
            AgentKind::Codex => "\u{2b21}",  // ⬡ — hexagon, OpenAI-ish
            AgentKind::Pi => "\u{3c0}",      // π
            AgentKind::OpenCode => "\u{2b22}", // ⬢ — black hexagon
            AgentKind::Custom(_) => self.def().map_or("?", |d| d.icon.as_str()),
        }
    }

    /// Human-readable name, e.g. for pickers.
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex CLI",
            AgentKind::Pi => "Pi",
            AgentKind::OpenCode => "OpenCode",
            AgentKind::Custom(_) => self.def().map_or("?", |d| d.label.as_str()),
        }
    }

    /// Whether the agent's hooks report the start of a turn (so the
    /// inventory need not guess "working" from pane output). Codex `notify`
    /// only reports completed turns; Pi has no hooks.
    pub fn reports_turn_start(self) -> bool {
        matches!(self, AgentKind::Claude | AgentKind::OpenCode)
    }

    /// The built-in agents, in picker order ([`AgentKind::all`] adds the
    /// TOML-defined ones).
    pub const ALL: &[AgentKind] = &[
        AgentKind::Claude,
        AgentKind::Codex,
        AgentKind::Pi,
        AgentKind::OpenCode,
    ];

    /// Process name to look for when checking whether the agent is still
    /// alive in a pane (either as the pane process or one of its children —
    /// codex installed via npm runs as a `node` wrapper with a `codex` child).
    pub fn process_name(self) -> &'static str {
        match self.def() {
            Some(d) => d.process_name.as_deref().unwrap_or_else(|| {
                let bin = d.command[0].as_str();
                bin.rsplit('/').next().unwrap_or(bin)
            }),
            None => self.id(),
        }
    }

    /// For agents that can run as a plain node script (comm `node` rather than
    /// their own name), the command-line term to search for when the exact
    /// `pgrep -x` liveness check fails. `None` for agents whose process name is
    /// reliable (the npm/opencode binary self-reports `comm=opencode`).
    pub fn node_argv_term(self) -> Option<&'static str> {
        match self {
            // Older node-script opencode installs run as `node …/opencode …`,
            // so `comm` is `node` and `pgrep -x opencode` misses them.
            AgentKind::OpenCode => Some(self.id()),
            // TOML agents are often node scripts (gemini, copilot).
            AgentKind::Custom(_) => Some(self.process_name()),
            _ => None,
        }
    }

    /// Markers for dialogs that need the user's attention (permission prompts
    /// and question selectors), matched against the bottom of the pane.
    pub fn attention_markers(self) -> Vec<&'static str> {
        let markers: &[&str] = match self {
            AgentKind::Claude => &[
                "Do you want to",
                "Yes, allow all",
                "No, and tell Claude what to do differently",
                "❯ 1.",
            ],
            // Codex renders selectors (trust dialog etc.) as `› 1.`; with the
            // yolo flag there are no per-command approvals.
            AgentKind::Codex => &["› 1."],
            // Pi has no per-command approvals; its only blocking dialogs are
            // modal selectors (project trust, /model picker, …), which all
            // render the same navigation hint line. Sessions launch with
            // `--approve`, so the trust dialog shouldn't appear — the markers
            // are a safety net.
            AgentKind::Pi => &["Trust project folder?", "↑↓ navigate"],
            // OpenCode permission dialog (opencode v1.18.25): header
            // `△ Permission required` with `Allow once / Allow always / Reject`
            // buttons. Matched against the last pane lines, case-sensitive.
            AgentKind::OpenCode => &[
                "Permission required",
                "Allow once",
                "Allow always",
            ],
            AgentKind::Custom(_) => {
                let def = self.def();
                return def
                    .map(|d| d.attention_markers.iter().map(String::as_str).collect())
                    .unwrap_or_default();
            }
        };
        markers.to_vec()
    }

    /// Whether `line` is part of the agent's input box / footer chrome, used
    /// to trim transcripts and to guard attention detection.
    pub fn is_prompt_chrome(self, line: &str) -> bool {
        let line = line.trim();
        match self {
            AgentKind::Claude => {
                line == "❯"
                    || line == ">"
                    || line.starts_with('╭')
                    || line.starts_with('┌')
                    || (line.chars().count() >= 10 && line.chars().all(|c| c == '─'))
            }
            // Codex draws long ─ rules *between* turns, so those are content;
            // its input line always starts with `›` (placeholder or typed text)
            // and everything below it is footer.
            AgentKind::Codex => line == "›" || line.starts_with("› "),
            // Pi frames its input box with full-width ─ rules; the cwd line
            // and stats footer render below the bottom rule and are cut with
            // it. It draws no rules inside the transcript.
            AgentKind::Pi => line.chars().count() >= 10 && line.chars().all(|c| c == '─'),
            // OpenCode's input box sits at the bottom of the pane, framed by full-width
            // rules with a `❯` prompt line. Match only those distinctive
            // markers: a bare `>`, `│` side-rails, and box corners appear in
            // ordinary tool output (tables, trees, captured panes) and would
            // wrongly truncate the transcript. (Conservative pending a captured
            // opencode pane to pin the exact glyphs per theme.)
            AgentKind::OpenCode => {
                line == "❯"
                    || (line.chars().count() >= 10 && line.chars().all(|c| c == '─' || c == '━'))
            }
            AgentKind::Custom(_) => self
                .def()
                .is_some_and(|d| d.prompt_chrome.iter().any(|p| line.starts_with(p.as_str()))),
        }
    }

    /// Whether this agent loads the showrunner skills as a Claude Code plugin
    /// (`--plugin-dir`). Other agents get the same SKILL.md files installed
    /// under `.agents/skills/` instead.
    pub fn supports_plugin_dir(self) -> bool {
        matches!(self, AgentKind::Claude)
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Parse an agent id, with a helpful error listing the valid ids.
pub fn parse_agent_id(id: &str) -> anyhow::Result<AgentKind> {
    AgentKind::from_id(id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown agent '{id}' (expected one of: {})",
            AgentKind::ids().join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        for id in AgentKind::ids() {
            assert_eq!(AgentKind::from_id(id).unwrap().id(), id);
        }
        assert!(AgentKind::from_id("gpt").is_none());
    }

    #[test]
    fn bundled_agents_load_as_custom_kinds() {
        let gemini = AgentKind::from_id("gemini").unwrap();
        assert!(matches!(gemini, AgentKind::Custom(_)));
        assert_eq!(gemini.label(), "Gemini CLI");
        assert_eq!(gemini.process_name(), "gemini");
        assert_eq!(AgentKind::from_id("cursor").unwrap().process_name(), "cursor-agent");
        for id in ["gemini", "copilot", "cursor", "amp"] {
            assert!(AgentKind::ids().contains(&id), "{id}");
        }
        assert!(!gemini.reports_turn_start());
        assert!(gemini.attention_markers().contains(&"Allow execution"));
        assert!(gemini.is_prompt_chrome("│ > type here"));
    }

    #[test]
    fn user_agents_override_by_id_and_cannot_shadow_builtins() {
        let base = parse_defs(include_str!("agents.toml")).unwrap();
        let n = base.len();
        let extra = parse_defs(
            r#"
            [[agent]]
            id = "gemini"
            label = "Gemini (mine)"
            command = ["gemini"]
            [[agent]]
            id = "claude"
            label = "fake"
            command = ["x"]
            [[agent]]
            id = "aider"
            label = "Aider"
            command = ["/usr/bin/aider"]
            "#,
        )
        .unwrap();
        let merged = merge_defs(merge_defs(Vec::new(), base), extra);
        assert_eq!(merged.len(), n + 1);
        assert_eq!(merged.iter().find(|d| d.id == "gemini").unwrap().label, "Gemini (mine)");
        assert!(merged.iter().all(|d| d.id != "claude"));
        assert!(parse_defs("[[agent]]\nid = \"x\"\nlabel = \"x\"\ncommand = []\nbogus = 1").is_err());
    }

    #[test]
    fn default_is_claude() {
        assert_eq!(AgentKind::default(), AgentKind::Claude);
    }

    // Pi chrome captured from pi 0.84.2 panes: input box framed by
    // full-width ─ rules, cwd + stats footer below.
    #[test]
    fn pi_chrome_matches_rules_not_transcript_or_footer() {
        let pi = AgentKind::Pi;
        assert!(pi.is_prompt_chrome("────────────────────────────"));
        assert!(!pi.is_prompt_chrome("⠏ Working..."));
        assert!(!pi.is_prompt_chrome("0.0%/128k (auto)"));
        assert!(!pi.is_prompt_chrome("/some/work/dir (main)"));
    }

    #[test]
    fn codex_chrome_matches_input_line_not_rules() {
        let codex = AgentKind::Codex;
        assert!(codex.is_prompt_chrome("› Ask Codex to do anything"));
        assert!(codex.is_prompt_chrome("› typed text"));
        // Turn-separator rules are transcript content for codex.
        assert!(!codex.is_prompt_chrome("────────────────────────────"));
        assert!(!codex.is_prompt_chrome("• Working (2s • esc to interrupt)"));
    }
}
