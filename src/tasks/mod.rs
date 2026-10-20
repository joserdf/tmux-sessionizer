//! Kanban board model for dispatching AI coding agents.
//!
//! Phase 5 core: provider-neutral types (`Card`, [`Column`], [`TaskProvider`])
//! plus the board configuration and the factory that turns a [`BoardConfig`]
//! into a concrete provider. A later dispatcher loop (not here) will pull cards
//! off these boards and hand them to agent sessions; Plane / GitHub / Jira
//! providers will be built on top of the same trait.

pub mod native;

use std::collections::HashMap;
use std::fmt;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A kanban column a card can sit in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Backlog,
    Ready,
    Running,
    NeedsYou,
    Review,
    Done,
}

/// Uniquely identifies a card within its provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CardId(pub String);

/// A single board card, provider-neutral. `id` is the stable provider-local
/// identity a dispatcher acts on; `key` is the human-facing short reference
/// (e.g. "SR-42").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    pub id: CardId,
    pub key: String,
    pub title: String,
    pub body: String,
    pub column: Column,
    pub labels: Vec<String>,
    pub priority: Option<u8>,
    pub url: Option<String>,
}

/// The kind of thing a card can be linked to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Branch,
    PullRequest,
    Session,
}

/// A single provider-side link recorded on a card (kind + target url).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLink {
    pub kind: Link,
    pub url: String,
}

/// Backend that reads and mutates one board.
///
/// Synchronous on purpose: a future dispatcher will call it from
/// `spawn_blocking` so it must not itself block on async machinery. `list`
/// takes `&self` (a pure read); the mutators take `&mut self` because every
/// provider holds mutable state it changes in place.
pub trait TaskProvider {
    /// All cards currently on the board, in board order.
    fn list(&self) -> Result<Vec<Card>>;
    /// Move a card to another column.
    fn move_to(&mut self, card: &CardId, col: Column) -> Result<()>;
    /// Leave a comment on a card.
    fn comment(&mut self, card: &CardId, body: &str) -> Result<()>;
    /// Record a link on a card.
    fn link(&mut self, card: &CardId, kind: Link, url: &str) -> Result<()>;
}

fn default_max_running() -> usize {
    2
}
fn default_poll_secs() -> u64 {
    60
}

/// Global dispatch loop settings, under `[dispatch]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchConfig {
    /// Global cap on concurrently running agents across all boards.
    #[serde(default = "default_max_running")]
    pub max_running: usize,
    /// Seconds between dispatcher polling passes.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        DispatchConfig {
            max_running: default_max_running(),
            poll_secs: default_poll_secs(),
        }
    }
}

/// Which backend a board reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// showrunner's own tasks.
    Native,
    Plane,
    Github,
    Jira,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ProviderKind::Native => "native",
            ProviderKind::Plane => "plane",
            ProviderKind::Github => "github",
            ProviderKind::Jira => "jira",
        };
        f.write_str(name)
    }
}

fn default_board_max_running() -> usize {
    1
}

/// A single board, under `[[boards]]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardConfig {
    /// Display name for the board.
    pub name: String,
    /// Backend provider.
    pub provider: ProviderKind,
    /// Showrunner project the cards' agent sessions run in.
    pub project: String,
    /// Agent harness to dispatch cards to (defaults to the global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Per-board cap on concurrently running agents.
    #[serde(default = "default_board_max_running")]
    pub max_running: usize,
    /// Optional path to the prompt template used for dispatched tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
    /// Plane backend settings.
    #[serde(default)]
    pub plane: Option<PlaneConfig>,
    /// GitHub backend settings.
    #[serde(default)]
    pub github: Option<GithubConfig>,
    /// Jira backend settings.
    #[serde(default)]
    pub jira: Option<JiraConfig>,
    /// Provider state name -> canonical [`Column`] mapping.
    #[serde(default)]
    pub columns: HashMap<String, Column>,
}

impl BoardConfig {
    /// Reject configurations that cannot work: non-native providers must have
    /// their backend block, the project must be named, and the running cap
    /// must be at least 1.
    pub fn validate(&self) -> Result<()> {
        if self.project.is_empty() {
            bail!("board '{}': project must not be empty", self.name);
        }
        if self.max_running < 1 {
            bail!("board '{}': max_running must be >= 1", self.name);
        }
        let requires = |block: &str, present: bool| -> Result<()> {
            if !present {
                bail!(
                    "board '{}': provider '{}' requires a [boards.{}] block",
                    self.name,
                    self.provider,
                    block
                );
            }
            Ok(())
        };
        match self.provider {
            ProviderKind::Native => Ok(()),
            ProviderKind::Plane => requires("plane", self.plane.is_some()),
            ProviderKind::Github => requires("github", self.github.is_some()),
            ProviderKind::Jira => requires("jira", self.jira.is_some()),
        }
    }
}

/// Plane backend config. Secrets are only ever env-var *names*, never values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaneConfig {
    pub url: String,
    pub workspace: String,
    pub project_id: String,
    pub api_key_env: String,
}

/// GitHub Projects backend config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GithubConfig {
    pub owner: String,
    pub project_number: u32,
    #[serde(default = "default_status_field")]
    pub status_field: String,
}

fn default_status_field() -> String {
    "Status".to_string()
}

/// Jira backend config. Credentials are env-var names, never values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JiraConfig {
    pub url: String,
    pub project_key: String,
    pub email_env: String,
    pub api_token_env: String,
}

/// Build the concrete [`TaskProvider`] for a board. The native provider borrows
/// the (in-memory) config so its mutations land straight in showrunner's tasks;
/// a separate `Config::save()` persists them. Non-native providers are not
/// implemented yet and fail loudly.
pub fn provider_for<'a>(
    config: &'a mut Config,
    board: &BoardConfig,
) -> Result<Box<dyn TaskProvider + 'a>> {
    board.validate()?;
    match board.provider {
        ProviderKind::Native => {
            let project = config
                .projects
                .iter_mut()
                .find(|p| p.name == board.project)
                .ok_or_else(|| {
                    anyhow!(
                        "board '{}': project '{}' not found",
                        board.name,
                        board.project
                    )
                })?;
            Ok(Box::new(native::NativeProvider { project }))
        }
        other => bail!("provider '{other}' is not implemented yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Project, Task};

    fn task(name: &str, branch: &str) -> Task {
        Task {
            name: name.to_string(),
            branch: branch.to_string(),
            base_branch: None,
            archived: false,
            column: None,
            body: None,
            notes: vec![],
            links: vec![],
        }
    }

    fn project() -> Project {
        Project {
            name: "tmux-sessionizer".to_string(),
            path: "/tmp/showrunner-test".to_string(),
            tasks: vec![task("Fix bug", "fix/bug"), task("Ship feature", "feat/x")],
            copy_patterns: vec![],
            setup_commands: vec![],
            run_command: None,
        }
    }

    #[test]
    fn parses_full_board_config() {
        let toml_str = r#"
[dispatch]
max_running = 2
poll_secs = 60

[[boards]]
name = "sessionizer"
provider = "plane"
project = "tmux-sessionizer"
agent = "claude"
max_running = 2
prompt_template = "~/.showrunner/prompts/task.md"

[boards.plane]
url = "http://plane.lan"
workspace = "joserdf"
project_id = "abc"
api_key_env = "PLANE_API_KEY"

[boards.columns]
"Backlog" = "backlog"
"Ready for agent" = "ready"
"In Progress" = "running"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.dispatch.max_running, 2);
        assert_eq!(config.dispatch.poll_secs, 60);
        assert_eq!(config.boards.len(), 1);
        let board = &config.boards[0];
        assert_eq!(board.name, "sessionizer");
        assert_eq!(board.provider, ProviderKind::Plane);
        assert_eq!(board.project, "tmux-sessionizer");
        assert_eq!(board.agent.as_deref(), Some("claude"));
        assert_eq!(board.max_running, 2);
        assert_eq!(
            board.prompt_template.as_deref(),
            Some("~/.showrunner/prompts/task.md")
        );
        let plane = board.plane.as_ref().unwrap();
        assert_eq!(plane.url, "http://plane.lan");
        assert_eq!(plane.workspace, "joserdf");
        assert_eq!(plane.project_id, "abc");
        assert_eq!(plane.api_key_env, "PLANE_API_KEY");
        assert_eq!(board.columns.get("Backlog"), Some(&Column::Backlog));
        assert_eq!(board.columns.get("Ready for agent"), Some(&Column::Ready));
        assert_eq!(board.columns.get("In Progress"), Some(&Column::Running));
        board.validate().unwrap();
    }

    #[test]
    fn config_without_boards_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.dispatch, DispatchConfig::default());
        assert_eq!(config.dispatch.max_running, 2);
        assert_eq!(config.dispatch.poll_secs, 60);
        assert!(config.boards.is_empty());
    }

    #[test]
    fn task_without_board_fields_serializes_unchanged() {
        let task = task("Fix bug", "fix/bug");
        let s = toml::to_string(&task).unwrap();
        assert_eq!(s, "name = \"Fix bug\"\nbranch = \"fix/bug\"\n");
    }

    #[test]
    fn task_board_fields_round_trip() {
        let task = Task {
            column: Some(Column::NeedsYou),
            body: Some("body text".to_string()),
            notes: vec!["note".to_string()],
            links: vec![TaskLink {
                kind: Link::PullRequest,
                url: "http://pr".to_string(),
            }],
            ..task("Fix bug", "fix/bug")
        };
        let s = toml::to_string(&task).unwrap();
        let back: Task = toml::from_str(&s).unwrap();
        assert_eq!(back.column, Some(Column::NeedsYou));
        assert_eq!(back.body.as_deref(), Some("body text"));
        assert_eq!(back.notes, vec!["note".to_string()]);
        assert_eq!(back.links.len(), 1);
        assert_eq!(back.links[0].kind, Link::PullRequest);
    }

    #[test]
    fn validate_rejects_bad_boards() {
        let base = BoardConfig {
            name: "b".to_string(),
            provider: ProviderKind::Plane,
            project: "p".to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: None,
            jira: None,
            columns: HashMap::new(),
        };
        // Plane without its block fails.
        assert!(base.validate().is_err());
        // With the block it passes.
        let mut ok = base.clone();
        ok.plane = Some(PlaneConfig {
            url: "http://plane.lan".to_string(),
            workspace: "w".to_string(),
            project_id: "abc".to_string(),
            api_key_env: "PLANE_API_KEY".to_string(),
        });
        ok.validate().unwrap();
        // Empty project fails even for native.
        let mut native = BoardConfig {
            provider: ProviderKind::Native,
            ..ok.clone()
        };
        native.project = String::new();
        assert!(native.validate().is_err());
        // Zero cap fails.
        let mut capped = BoardConfig {
            provider: ProviderKind::Native,
            ..ok.clone()
        };
        capped.max_running = 0;
        assert!(capped.validate().is_err());
    }

    #[test]
    fn provider_factory_rejects_unimplemented() {
        let mut config = Config::default();
        config.projects.push(project());
        let board = BoardConfig {
            name: "b".to_string(),
            provider: ProviderKind::Jira,
            project: "tmux-sessionizer".to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: None,
            jira: Some(JiraConfig {
                url: "http://jira".to_string(),
                project_key: "SR".to_string(),
                email_env: "JIRA_EMAIL".to_string(),
                api_token_env: "JIRA_TOKEN".to_string(),
            }),
            columns: HashMap::new(),
        };
        let err = provider_for(&mut config, &board)
            .err()
            .expect("jira provider should fail")
            .to_string();
        assert!(err.contains("'jira' is not implemented yet"), "got: {err}");
    }

    #[test]
    fn native_provider_lists_only_cards() {
        let mut config = Config::default();
        let mut p = project();
        // Make only one task a card.
        p.tasks[0].column = Some(Column::Backlog);
        config.projects.push(p);
        let board = BoardConfig {
            name: "b".to_string(),
            provider: ProviderKind::Native,
            project: "tmux-sessionizer".to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: None,
            jira: None,
            columns: HashMap::new(),
        };
        let provider = provider_for(&mut config, &board).unwrap();
        let cards = provider.list().unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id.0, "fix/bug");
        assert_eq!(cards[0].key, "Fix bug");
        assert_eq!(cards[0].title, "Fix bug");
        assert_eq!(cards[0].column, Column::Backlog);
    }

    #[test]
    fn native_provider_mutators_round_trip() {
        let mut config = Config::default();
        let mut p = project();
        p.tasks[0].column = Some(Column::Backlog);
        config.projects.push(p);
        let board = BoardConfig {
            name: "b".to_string(),
            provider: ProviderKind::Native,
            project: "tmux-sessionizer".to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: None,
            jira: None,
            columns: HashMap::new(),
        };
        let mut provider = provider_for(&mut config, &board).unwrap();
        let id = provider.list().unwrap()[0].id.clone();
        provider.move_to(&id, Column::Running).unwrap();
        provider.comment(&id, "first comment").unwrap();
        provider
            .link(&id, Link::PullRequest, "http://pr/1")
            .unwrap();

        // Mutations are visible through a fresh provider over the same config.
        let cards = provider.list().unwrap();
        assert_eq!(cards[0].column, Column::Running);

        drop(provider);
        let task = &config.projects[0].tasks[0];
        assert_eq!(task.column, Some(Column::Running));
        assert_eq!(task.notes, vec!["first comment".to_string()]);
        assert_eq!(task.links.len(), 1);
        assert_eq!(task.links[0].kind, Link::PullRequest);
        assert_eq!(task.links[0].url, "http://pr/1");
    }

    #[test]
    fn native_provider_missing_card_fails() {
        let mut config = Config::default();
        config.projects.push(project());
        let board = BoardConfig {
            name: "b".to_string(),
            provider: ProviderKind::Native,
            project: "tmux-sessionizer".to_string(),
            agent: None,
            max_running: 1,
            prompt_template: None,
            plane: None,
            github: None,
            jira: None,
            columns: HashMap::new(),
        };
        let mut provider = provider_for(&mut config, &board).unwrap();
        let err = provider
            .move_to(&CardId("nope".to_string()), Column::Done)
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "got: {err}");
    }
}
