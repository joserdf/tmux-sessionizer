//! Agent harness profiles: which coding-agent CLI a session runs and the
//! agent-specific bits of launching, resuming, and observing it. Everything
//! else (tmux, worktrees, status polling) is agent-agnostic.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
    Pi,
    OpenCode,
}

/// Ids accepted in config (`default_agent`) and on `--agent`.
pub const AGENT_IDS: &[&str] = &["claude", "codex", "pi", "opencode"];

impl AgentKind {
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "claude" => Some(AgentKind::Claude),
            "codex" => Some(AgentKind::Codex),
            "pi" => Some(AgentKind::Pi),
            "opencode" => Some(AgentKind::OpenCode),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Pi => "pi",
            AgentKind::OpenCode => "opencode",
        }
    }

    /// Single-cell glyph identifying the harness in session lists.
    pub fn icon(self) -> &'static str {
        match self {
            AgentKind::Claude => "\u{273b}", // ✻ — Claude's asterisk motif
            AgentKind::Codex => "\u{2b21}",  // ⬡ — hexagon, OpenAI-ish
            AgentKind::Pi => "\u{3c0}",      // π
            AgentKind::OpenCode => "\u{2b22}", // ⬢ — black hexagon
        }
    }

    /// Human-readable name, e.g. for pickers.
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex CLI",
            AgentKind::Pi => "Pi",
            AgentKind::OpenCode => "OpenCode",
        }
    }

    /// All known agents, in picker order.
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
        self.id()
    }

    /// Markers for dialogs that need the user's attention (permission prompts
    /// and question selectors), matched against the bottom of the pane.
    pub fn attention_markers(self) -> &'static [&'static str] {
        match self {
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
        }
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
            // OpenCode uses standard interactive prompt frames or indicators.
            // Heuristic for OpenCode prompt input box line & border rules:
            // Input prompt indicators like `>` or `❯`, or border box characters (`│`, `╭`, `└`, `─`),
            // or typical prompt/status bar lines. Note: may need validation against specific OpenCode TUI themes.
            AgentKind::OpenCode => {
                line == "❯"
                    || line == ">"
                    || line.starts_with('╭')
                    || line.starts_with('└')
                    || line.starts_with('│')
                    || (line.chars().count() >= 10 && line.chars().all(|c| c == '─' || c == '━'))
            }
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
            AGENT_IDS.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        for id in AGENT_IDS {
            assert_eq!(AgentKind::from_id(id).unwrap().id(), *id);
        }
        assert!(AgentKind::from_id("gpt").is_none());
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
