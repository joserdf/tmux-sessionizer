//! Plane (plane.so, self-hosted) [`TaskProvider`]: a board backed by the
//! work items of one Plane project. All HTTP is blocking on purpose because
//! the provider is called from a std::thread dispatcher, the TUI, and axum
//! handlers inside `tokio::task::spawn_blocking`. Mapping from Plane states
//! and work items to showrunner [`Card`]s is kept in pure functions so it can
//! be unit-tested without a network.

use std::collections::HashMap;
use std::env;

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::tasks::{Card, CardId, Column, Link, PlaneConfig, TaskProvider};

/// A single Plane state (kanban lane) returned by the `states/` endpoint.
#[derive(Debug, Clone, Deserialize)]
struct State {
    id: String,
    name: String,
    group: String,
}

/// A paginated list envelope shared by `states/` and `work-items/`.
#[derive(Debug, Deserialize)]
struct Page<T> {
    results: Vec<T>,
    #[serde(default)]
    next_cursor: Option<String>,
    #[serde(default)]
    next_page_results: bool,
}

/// A single work item (issue) from `work-items/`.
#[derive(Debug, Deserialize)]
struct WorkItem {
    id: String,
    name: String,
    #[serde(default)]
    description_html: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    sequence_id: Option<i64>,
    #[serde(default)]
    labels: Vec<String>,
}

/// Project metadata used for the card key prefix (its `identifier`).
#[derive(Debug, Deserialize)]
struct Project {
    #[serde(default)]
    identifier: Option<String>,
}

/// A single work-item label from the `labels/` endpoint.
#[derive(Debug, Deserialize)]
struct Label {
    id: String,
    name: String,
}

/// A column a state falls into by name alone, before group fallback. Only
/// columns with no meaningful group mapping live here so agents get the same
/// lanes the dispatcher moves cards between (e.g. Review).
fn default_column_for_name(name: &str) -> Option<Column> {
    if name.eq_ignore_ascii_case("review") {
        Some(Column::Review)
    } else {
        None
    }
}

/// Map a Plane state group to the fallback column. `unstarted` is treated as
/// Backlog so agents only ever receive explicitly-Ready work.
fn column_for_group(group: &str) -> Option<Column> {
    match group {
        "backlog" | "unstarted" => Some(Column::Backlog),
        "started" => Some(Column::Running),
        "completed" => Some(Column::Done),
        // Cancelled work never becomes a card.
        _ => None,
    }
}

/// Resolve a state's column: first by its configured name, then by a known
/// default name, else by its group.
fn state_to_column(columns: &HashMap<String, Column>, state: &State) -> Option<Column> {
    let by_name = columns
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(&state.name))
        .map(|(_, col)| *col);
    by_name
        .or_else(|| default_column_for_name(&state.name))
        .or_else(|| column_for_group(&state.group))
}

/// The state to move a card into for a column: a state whose configured name
/// maps to the column, else one matching by its default name, else the first
/// state of the matching group.
fn state_for_column<'a>(
    states: &'a [State],
    columns: &HashMap<String, Column>,
    col: Column,
) -> Option<&'a State> {
    let mapped = states.iter().find(|s| {
        columns
            .iter()
            .any(|(name, c)| *c == col && name.eq_ignore_ascii_case(&s.name))
    });
    if mapped.is_some() {
        return mapped;
    }
    let by_default = states
        .iter()
        .find(|s| default_column_for_name(&s.name) == Some(col));
    if by_default.is_some() {
        return by_default;
    }
    let group = match col {
        Column::Backlog => "backlog",
        Column::Running => "started",
        Column::Done => "completed",
        _ => return None,
    };
    states.iter().find(|s| s.group == group)
}

/// Plane priority string -> card priority (4 = urgent .. 1 = low).
fn parse_priority(priority: &str) -> Option<u8> {
    match priority {
        "urgent" => Some(4),
        "high" => Some(3),
        "medium" => Some(2),
        "low" => Some(1),
        _ => None,
    }
}

/// Escape free text for embedding in an HTML comment body, preserving newlines.
fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            '\n' => escaped.push_str("<br>"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// The cursor for the next page of a paginated response, if any.
fn next_page<T>(page: &Page<T>) -> Option<String> {
    if page.next_page_results {
        page.next_cursor.clone()
    } else {
        None
    }
}

/// The live Plane provider. Project identifier and states are fetched once at
/// construction and cached for the provider's lifetime.
pub struct PlaneProvider {
    client: reqwest::blocking::Client,
    base: String,
    /// Web root for building card URLs (`{url}/{workspace}/projects/{project}/issues`).
    web_base: String,
    columns: HashMap<String, Column>,
    identifier: String,
    /// States in API order, so "first state of the group" is deterministic.
    states: Vec<State>,
    label_names: HashMap<String, String>,
    api_key: String,
}

impl PlaneProvider {
    /// Build the provider, fetching and caching the project identifier and
    /// states. Fails fast when the API key env var is missing.
    pub fn connect(plane: &PlaneConfig, columns: HashMap<String, Column>) -> Result<Self> {
        let api_key = env::var(&plane.api_key_env).with_context(|| {
            format!(
                "plane: env var '{}' (plane.api_key_env) is not set",
                plane.api_key_env
            )
        })?;
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let root = plane.url.trim_end_matches('/');
        let base = format!(
            "{root}/api/v1/workspaces/{}/projects/{}",
            plane.workspace, plane.project_id
        );
        let web_base = format!("{root}/{}/projects/{}/issues", plane.workspace, plane.project_id);
        let mut provider = PlaneProvider {
            client,
            base,
            web_base,
            columns,
            identifier: String::new(),
            states: Vec::new(),
            label_names: HashMap::new(),
            api_key,
        };
        provider.load_project()?;
        provider.load_states()?;
        provider.load_labels()?;
        Ok(provider)
    }

    /// GET a single JSON document with the API key header.
    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(format!("{}{}", self.base, path))
            .header("X-API-Key", &self.api_key)
            .send()
            .context("plane: request failed")?
            .error_for_status()
            .context("plane: request returned an error status")?;
        response.json().context("plane: bad JSON in response")
    }

    /// Cache the project `identifier` for building card keys.
    fn load_project(&mut self) -> Result<()> {
        let project: Project = self.get("")?;
        self.identifier = project.identifier.unwrap_or_default();
        Ok(())
    }

    /// Fetch and cache every state, in API order, following pagination.
    fn load_states(&mut self) -> Result<()> {
        let mut cursor: Option<String> = None;
        loop {
            let path = match &cursor {
                Some(c) => format!("/states/?per_page=100&cursor={c}"),
                None => "/states/?per_page=100".to_string(),
            };
            let page: Page<State> = self.get(&path)?;
            let more = next_page(&page);
            self.states.extend(page.results);
            cursor = more;
            if cursor.is_none() {
                return Ok(());
            }
        }
    }

    /// Fetch every label and cache id -> name, following pagination.
    fn load_labels(&mut self) -> Result<()> {
        let mut cursor: Option<String> = None;
        loop {
            let path = match &cursor {
                Some(c) => format!("/labels/?per_page=100&cursor={c}"),
                None => "/labels/?per_page=100".to_string(),
            };
            let page: Page<Label> = self.get(&path)?;
            let more = next_page(&page);
            for label in page.results {
                self.label_names.insert(label.id, label.name);
            }
            cursor = more;
            if cursor.is_none() {
                return Ok(());
            }
        }
    }

    /// Fetch every work item across pages and turn it into a [`Card`].
    fn list_items(&self) -> Result<Vec<Card>> {
        let mut cards = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let path = match &cursor {
                Some(c) => format!("/work-items/?per_page=100&cursor={c}"),
                None => "/work-items/?per_page=100".to_string(),
            };
            let page: Page<WorkItem> = self.get(&path)?;
            for item in &page.results {
                if let Some(card) = self.item_to_card(item) {
                    cards.push(card);
                }
            }
            cursor = next_page(&page);
            if cursor.is_none() {
                return Ok(cards);
            }
        }
    }

    /// Map one work item to a card, skipping cancelled/unmapped states.
    fn item_to_card(&self, item: &WorkItem) -> Option<Card> {
        let state = item
            .state
            .as_deref()
            .and_then(|id| self.states.iter().find(|s| s.id == id))?;
        let column = state_to_column(&self.columns, state)?;
        let key = match &self.identifier {
            prefix if !prefix.is_empty() => format!("{prefix}-{}", item.sequence_id.unwrap_or_default()),
            _ => format!("#{}", item.sequence_id.unwrap_or_default()),
        };
        let body = item
            .description_html
            .as_deref()
            .map(strip_html)
            .unwrap_or_default();
        let labels = item
            .labels
            .iter()
            .filter_map(|id| self.label_names.get(id).cloned())
            .collect();
        let url = format!("{}/{}", self.web_base, item.id);
        Some(Card {
            id: CardId(item.id.clone()),
            key,
            title: item.name.clone(),
            body,
            column,
            labels,
            priority: item.priority.as_deref().and_then(parse_priority),
            url: Some(url),
        })
    }

    /// POST a JSON body and decode the JSON response.
    fn post<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<T> {
        let response = self
            .client
            .post(format!("{}{}", self.base, path))
            .header("X-API-Key", &self.api_key)
            .json(body)
            .send()
            .context("plane: request failed")?
            .error_for_status()
            .context("plane: request returned an error status")?;
        response.json().context("plane: bad JSON in response")
    }

    /// PATCH a JSON body and decode the JSON response.
    fn patch<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<T> {
        let response = self
            .client
            .patch(format!("{}{}", self.base, path))
            .header("X-API-Key", &self.api_key)
            .json(body)
            .send()
            .context("plane: request failed")?
            .error_for_status()
            .context("plane: request returned an error status")?;
        response.json().context("plane: bad JSON in response")
    }
}

impl TaskProvider for PlaneProvider {
    fn list(&self) -> Result<Vec<Card>> {
        self.list_items()
    }

    fn move_to(&mut self, card: &CardId, col: Column) -> Result<()> {
        let target = state_for_column(&self.states, &self.columns, col).ok_or_else(|| {
            anyhow!(
                "no Plane state mapped to column {col:?}; map a Plane state to it in [boards.columns]"
            )
        })?;
        let body = serde_json::json!({ "state": target.id });
        let _: serde_json::Value = self.patch(&format!("/work-items/{}/", card.0), &body)?;
        Ok(())
    }

    fn comment(&mut self, card: &CardId, body: &str) -> Result<()> {
        let comment_html = format!("<p>{}</p>", escape_html(body));
        let payload = serde_json::json!({ "comment_html": comment_html });
        let _: serde_json::Value = self.post(
            &format!("/work-items/{}/comments/", card.0),
            &payload,
        )?;
        Ok(())
    }

    fn link(&mut self, card: &CardId, kind: Link, url: &str) -> Result<()> {
        // Branch references are `branch:name`, not URLs; Plane links need real
        // URLs, so record those as a comment instead.
        if kind == Link::Branch && !is_url(url) {
            return self.comment(card, url);
        }
        let title = match kind {
            Link::Branch => "branch",
            Link::PullRequest => "pull request",
            Link::Session => "session",
        };
        let payload = serde_json::json!({ "url": url, "title": title });
        let _: serde_json::Value =
            self.post(&format!("/work-items/{}/links/", card.0), &payload)?;
        Ok(())
    }
}

/// Whether a string looks like an absolute URL (scheme://...).
fn is_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

/// Crude HTML-to-text: strip tags so descriptions render as plain body text.
fn strip_html(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str, group: &str) -> State {
        State {
            id: format!("state-{name}"),
            name: name.to_string(),
            group: group.to_string(),
        }
    }

    #[test]
    fn state_maps_by_name_before_group() {
        let mut columns = HashMap::new();
        columns.insert("Ready for agent".to_string(), Column::Ready);
        columns.insert("In Progress".to_string(), Column::Running);
        let s = state("Ready for agent", "unstarted");
        assert_eq!(
            state_to_column(&columns, &s),
            Some(Column::Ready),
            "configured name wins over group"
        );
    }

    #[test]
    fn unmapped_state_falls_back_to_group() {
        let columns = HashMap::new();
        assert_eq!(
            state_to_column(&columns, &state("Todo", "unstarted")),
            Some(Column::Backlog),
            "unstarted yields Backlog so agents only get Ready work"
        );
        assert_eq!(
            state_to_column(&columns, &state("In Progress", "started")),
            Some(Column::Running)
        );
        assert_eq!(
            state_to_column(&columns, &state("Done", "completed")),
            Some(Column::Done)
        );
        assert_eq!(
            state_to_column(&columns, &state("Cancelled", "cancelled")),
            None,
            "cancelled work is skipped"
        );
    }

    #[test]
    fn move_target_prefers_mapped_state_then_group() {
        let states = vec![
            state("In Progress", "started"),
            state("Review", "started"),
            state("Todo", "unstarted"),
        ];
        let columns = HashMap::from([("Review".to_string(), Column::Review)]);
        assert_eq!(
            state_for_column(&states, &columns, Column::Review).map(|s| s.name.as_str()),
            Some("Review")
        );
        let empty = HashMap::new();
        assert_eq!(
            state_for_column(&states, &empty, Column::Running).map(|s| s.name.as_str()),
            Some("In Progress"),
            "first state of the started group"
        );
    }

    #[test]
    fn move_target_missing_group_errors() {
        let states = vec![state("Todo", "unstarted")];
        let columns = HashMap::new();
        assert!(state_for_column(&states, &columns, Column::NeedsYou).is_none());
    }

    #[test]
    fn review_state_reads_as_review_by_default() {
        let columns = HashMap::new();
        assert_eq!(
            state_to_column(&columns, &state("Review", "started")),
            Some(Column::Review),
            "a Review state maps to Review before the started-group fallback"
        );
        assert_eq!(
            state_to_column(&columns, &state("review", "completed")),
            Some(Column::Review),
            "case-insensitive"
        );
    }

    #[test]
    fn move_target_finds_review_by_default() {
        let states = vec![
            state("In Progress", "started"),
            state("Review", "started"),
        ];
        let columns = HashMap::new();
        assert_eq!(
            state_for_column(&states, &columns, Column::Review).map(|s| s.name.as_str()),
            Some("Review"),
            "Review is the move target without any [boards.columns] entry"
        );
    }

    #[test]
    fn pagination_cursor_advances_only_while_more_pages() {
        let more: Page<serde_json::Value> = serde_json::from_value(serde_json::json!({
            "results": [],
            "next_cursor": "abc",
            "next_page_results": true,
        }))
        .unwrap();
        assert_eq!(next_page(&more), Some("abc".to_string()));
        let done: Page<serde_json::Value> = serde_json::from_value(serde_json::json!({
            "results": [],
            "next_cursor": "abc",
            "next_page_results": false,
        }))
        .unwrap();
        assert_eq!(next_page(&done), None);
    }

    #[test]
    fn comment_escapes_html_and_keeps_newlines() {
        assert_eq!(
            escape_html("a <b> & \"q\" 'x'\nnext"),
            "a &lt;b&gt; &amp; &quot;q&quot; &#39;x&#39;<br>next"
        );
    }

    #[test]
    fn priority_maps_urgent_to_four_and_none_to_none() {
        assert_eq!(parse_priority("urgent"), Some(4));
        assert_eq!(parse_priority("high"), Some(3));
        assert_eq!(parse_priority("medium"), Some(2));
        assert_eq!(parse_priority("low"), Some(1));
        assert_eq!(parse_priority("none"), None);
    }

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(
            strip_html("<p>Hello <b>world</b></p>"),
            "Hello world"
        );
    }

    /// A provider with cached states, labels, and identifier, no network.
    fn test_provider() -> PlaneProvider {
        PlaneProvider {
            client: reqwest::blocking::Client::new(),
            base: "https://plane.example/api/v1/workspaces/w/projects/p".to_string(),
            web_base: "https://plane.example/w/projects/p/issues".to_string(),
            columns: HashMap::new(),
            identifier: "SR".to_string(),
            states: vec![
                State {
                    id: "s-backlog".into(),
                    name: "Backlog".into(),
                    group: "backlog".into(),
                },
                State {
                    id: "s-inprog".into(),
                    name: "In Progress".into(),
                    group: "started".into(),
                },
            ],
            label_names: HashMap::from([
                ("bug".to_string(), "Bug".to_string()),
                ("frontend".to_string(), "Frontend".to_string()),
            ]),
            api_key: "test-key".to_string(),
        }
    }

    #[test]
    fn work_item_maps_to_card() {
        let provider = test_provider();
        let item: WorkItem = serde_json::from_value(serde_json::json!({
            "id": "uuid-1",
            "name": "Fix the widget",
            "description_html": "<p>It <b>breaks</b>.</p>",
            "state": "s-inprog",
            "priority": "high",
            "sequence_id": 42,
            "labels": ["bug", "frontend"],
        }))
        .unwrap();
        let card = provider.item_to_card(&item).unwrap();
        assert_eq!(card.id.0, "uuid-1");
        assert_eq!(card.key, "SR-42");
        assert_eq!(card.title, "Fix the widget");
        assert_eq!(card.body, "It breaks.");
        assert_eq!(card.column, Column::Running);
        assert_eq!(card.priority, Some(3));
        assert_eq!(card.labels, vec!["Bug".to_string(), "Frontend".to_string()]);
        assert_eq!(
            card.url.as_deref(),
            Some("https://plane.example/w/projects/p/issues/uuid-1"),
            "card url is the web URL, not the API endpoint"
        );
    }

    #[test]
    fn work_item_labels_drop_unknown_ids() {
        let provider = test_provider();
        let item: WorkItem = serde_json::from_value(serde_json::json!({
            "id": "uuid-3",
            "name": "Labels",
            "state": "s-backlog",
            "sequence_id": 1,
            "labels": ["bug", "unknown-id"],
        }))
        .unwrap();
        let card = provider.item_to_card(&item).unwrap();
        assert_eq!(card.labels, vec!["Bug".to_string()]);
    }

    #[test]
    fn work_item_without_identifier_uses_sequence_number() {
        let mut provider = test_provider();
        provider.identifier.clear();
        let item: WorkItem = serde_json::from_value(serde_json::json!({
            "id": "uuid-2",
            "name": "No prefix",
            "state": "s-backlog",
            "priority": "none",
            "sequence_id": 7,
        }))
        .unwrap();
        let card = provider.item_to_card(&item).unwrap();
        assert_eq!(card.key, "#7");
        assert_eq!(card.priority, None);
    }

    #[test]
    fn connect_errors_when_api_key_env_missing() {
        let plane = PlaneConfig {
            url: "https://plane.example".to_string(),
            workspace: "w".to_string(),
            project_id: "p".to_string(),
            api_key_env: "SHOWRUNNER_TEST_PLANE_UNSET_KEY".to_string(),
        };
        // Ensure the sentinel is absent so the provider reliably fails fast.
        // SAFETY: single-threaded test on a uniquely-named var, no other test
        // reads or writes it.
        unsafe {
            std::env::remove_var(&plane.api_key_env);
        }
        let err = PlaneProvider::connect(&plane, HashMap::new())
            .err()
            .expect("connect should fail without the API key env var")
            .to_string();
        assert!(
            err.contains(&plane.api_key_env),
            "error should name the env var, got: {err}"
        );
    }

    /// Live round-trip against a real Plane instance. Runs only when
    /// PLANE_TEST_URL, PLANE_TEST_WORKSPACE, PLANE_TEST_PROJECT and
    /// PLANE_API_KEY are all set; otherwise it no-ops.
    #[test]
    #[ignore]
    fn live_plane_roundtrip() {
        let (Some(url), Some(workspace), Some(project), Some(_api_key)) = (
            std::env::var("PLANE_TEST_URL").ok(),
            std::env::var("PLANE_TEST_WORKSPACE").ok(),
            std::env::var("PLANE_TEST_PROJECT").ok(),
            std::env::var("PLANE_API_KEY").ok(),
        ) else {
            return;
        };
        let plane = PlaneConfig {
            url,
            workspace,
            project_id: project,
            api_key_env: "PLANE_API_KEY".to_string(),
        };
        let mut provider = PlaneProvider::connect(&plane, HashMap::new()).expect("connect");
        let cards = provider.list().expect("list");
        assert!(!cards.is_empty(), "expected at least one card");
        let first_backlog = cards
            .iter()
            .find(|c| c.column == Column::Backlog)
            .expect("a Backlog card");
        let id = first_backlog.id.clone();
        provider
            .move_to(&id, Column::Running)
            .expect("move to Running");
        provider
            .move_to(&id, Column::Backlog)
            .expect("move back to Backlog");
        provider
            .comment(&id, "showrunner live test")
            .expect("comment");
    }
}
