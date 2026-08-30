use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use futures_core::stream::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::interval;

use crate::config::{self, Config};
use crate::ops;
use crate::tmux::{self, DiffStats, TmuxSession};
use crate::worker::{TaskInfo, Worker, WorkerUpdate};

struct ServerState {
    worker: Worker,
    hostname: String,
    /// Last snapshot, served while the worker has no fresh update
    /// (e.g. two requests within one worker tick).
    last_state: Mutex<Option<Value>>,
    /// Recent agent hook events (session id, normalized event, time), ingested
    /// via POST /api/hook. Bounded to the most recent ~200.
    hook_events: Mutex<VecDeque<(String, crate::hooks::AgentEvent, std::time::Instant)>>,
}

type ApiError = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

pub fn run(bind: &str) -> Result<()> {
    let state = Arc::new(ServerState {
        worker: Worker::spawn(),
        hostname: crate::app::detect_hostname(),
        last_state: Mutex::new(None),
        hook_events: Mutex::new(VecDeque::new()),
    });

    if let Ok(cfg) = Config::load() {
        sync_hints(&state.worker, &cfg);
    }

    // Capture a handle for the status-cache task before `state` moves into the router.
    let status_state = state.clone();

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/manifest.json", get(manifest))
        .route("/icon.png", get(icon))
        .route("/api/state", get(api_state))
        .route("/events", get(api_events))
        .route("/events/worker", get(api_events_worker))
        .route("/api/sessions/{name}/output", get(api_output))
        .route("/api/diff", get(api_diff))
        .route("/api/sessions/{name}/send", post(api_send))
        .route("/api/sessions/{name}/keys", post(api_keys))
        .route("/api/sessions/{name}/kill", post(api_kill))
        .route("/api/projects", post(api_create_project))
        .route("/api/tasks", post(api_create_task))
        .route("/api/tasks/delete", post(api_delete_task))
        .route("/api/sessions", post(api_create_session))
        .route("/api/adhoc", post(api_create_adhoc))
        .route("/api/hook", post(api_hook))
        .route("/api/hook-events", get(api_hook_events))
        .with_state(state);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let port = listener.local_addr()?.port();
        println!("showrunner serving on http://{bind}");
        println!("Expose over your tailnet with: tailscale serve --bg {port}");

        // Write the tmux status-bar alert count (sessions waiting for input or
        // a permission) to $SESSIONIZER_CACHE_DIR/status.cache every 2s. This
        // is the single owner of that file (consumed by helpers/alert_status.sh).
        let status_state = status_state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(2000));
            loop {
                ticker.tick().await;
                let Some(u) = status_state.worker.latest.lock().unwrap().clone() else {
                    continue;
                };
                let attention = u
                    .statuses
                    .values()
                    .filter(|s| {
                        matches!(
                            s,
                            crate::tmux::SessionStatus::WaitingForInput
                                | crate::tmux::SessionStatus::WaitingForPermission
                        )
                    })
                    .count();
                write_status_cache(attention);
            }
        });

        // `into_make_service_with_connect_info` makes the TCP peer address
        // available to handlers as `ConnectInfo`; `api_hook` uses it to reject
        // non-loopback sources (hooks are only ever posted locally).
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await?;
        Ok(())
    })
}

/// Write the attention count to the tmux status-bar cache file (best effort).
fn write_status_cache(count: usize) {
    let dir = std::env::var_os("SESSIONIZER_CACHE_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache").join("tmux-sessionizer")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("status.cache"), count.to_string());
}

/// Mirror of `App::sync_worker_hints` for the headless server.
fn sync_hints(worker: &Worker, cfg: &Config) {
    let tasks: Vec<TaskInfo> = cfg
        .projects
        .iter()
        .flat_map(|p| {
            p.tasks.iter().map(|t| TaskInfo {
                project_name: p.name.clone(),
                project_path: p.path.clone(),
                branch: t.branch.clone(),
                base_branch: t.base_branch().to_string(),
            })
        })
        .collect();

    let project_paths: Vec<(String, String)> = cfg
        .projects
        .iter()
        .map(|p| (p.name.clone(), p.path.clone()))
        .collect();

    if let Ok(mut hints) = worker.hints.lock() {
        hints.tasks = tasks;
        hints.project_paths = project_paths;
    }
}

fn diff_json(d: &DiffStats) -> Value {
    json!({ "added": d.added, "removed": d.removed })
}

fn session_json(s: &TmuxSession, u: &WorkerUpdate) -> Value {
    let r = u.resources.get(&s.name);
    json!({
        "tmux_name": s.name,
        "name": s.session_name,
        "status": u.statuses.get(&s.name).map(|st| st.as_str()),
        "agent": tmux::session_agent(&s.name).id(),
        "branch": u.session_branches.get(&s.name),
        "diff": u.diff_stats.get(&s.name).map(diff_json),
        "cpu": r.map(|r| r.cpu_percent).unwrap_or(0.0),
        "mem_kb": r.map(|r| r.mem_kb).unwrap_or(0u64),
    })
}

fn build_state(cfg: &Config, u: &WorkerUpdate, hostname: &str) -> Value {
    let projects: Vec<Value> = cfg
        .projects
        .iter()
        .map(|p| {
            let tasks: Vec<Value> = p
                .tasks
                .iter()
                .map(|t| {
                    let sessions: Vec<Value> =
                        tmux::sessions_for_task(&p.name, &t.name, &u.sessions)
                            .iter()
                            .map(|s| session_json(s, u))
                            .collect();
                    json!({
                        "name": t.name,
                        "branch": t.branch,
                        "base_branch": t.base_branch(),
                        "archived": t.archived,
                        "pr_url": u.pr_urls.get(&t.branch),
                        "diff": u.task_diff_stats.get(&t.branch).map(diff_json),
                        "sessions": sessions,
                    })
                })
                .collect();
            let adhoc: Vec<Value> = tmux::adhoc_sessions_for_project(&p.name, &u.sessions)
                .iter()
                .map(|s| session_json(s, u))
                .collect();
            json!({
                "name": p.name,
                "path": p.path,
                "branch": u.project_branches.get(&p.name),
                "tasks": tasks,
                "adhoc_sessions": adhoc,
            })
        })
        .collect();

    let gpu: Vec<Value> = u
        .gpu
        .iter()
        .map(|(pid, mem_mib)| json!({ "pid": pid, "mem_mib": mem_mib }))
        .collect();

    json!({ "host": hostname, "projects": projects, "gpu": gpu })
}

async fn api_state(State(state): State<Arc<ServerState>>) -> Result<Response, ApiError> {
    let snapshot = tokio::task::spawn_blocking(move || {
        let cfg = Config::load().map_err(internal)?;
        sync_hints(&state.worker, &cfg);

        let update = state.worker.latest.lock().unwrap().take();
        let value = match update {
            Some(u) => build_state(&cfg, &u, &state.hostname),
            None => match state.last_state.lock().unwrap().clone() {
                Some(v) => v,
                // First request racing the worker's first tick: list sessions
                // inline so the tree renders immediately (statuses fill in on
                // the next poll).
                None => {
                    let u = WorkerUpdate {
                        sessions: tmux::list_sessions().unwrap_or_default(),
                        statuses: Default::default(),
                        diff_stats: Default::default(),
                        task_diff_stats: Default::default(),
                        session_branches: Default::default(),
                        session_agents: Default::default(),
                        pr_urls: Default::default(),
                        project_branches: Default::default(),
                        run_sessions: Default::default(),
                        resources: Default::default(),
                        gpu: Default::default(),
                        generation: 0,
                    };
                    build_state(&cfg, &u, &state.hostname)
                }
            },
        };
        *state.last_state.lock().unwrap() = Some(value.clone());
        Ok::<_, ApiError>(value)
    })
    .await
    .map_err(internal)??;

    Ok(axum::Json(snapshot).into_response())
}

async fn api_events(
    State(state): State<Arc<ServerState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
      let stream = async_stream::stream! {
        let mut timer = interval(Duration::from_millis(500));
        // A stalled client shouldn't burst-receive every missed tick.
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_gen: u64 = 0;
        let mut last_json: String = String::new();
        loop {
            timer.tick().await;

            let update_opt = {
                let latest = state.worker.latest.lock().unwrap();
                latest.clone()
            };

            // Skip the (relatively expensive) rebuild when the worker hasn't
            // published a new update since this client last looked.
            if let Some(u) = update_opt {
                if u.generation == last_gen {
                    continue;
                }
                let update_gen = u.generation;
                let state_clone = Arc::clone(&state);
                let res = tokio::task::spawn_blocking(move || {
                    let cfg = Config::load().ok()?;
                    sync_hints(&state_clone.worker, &cfg);
                    let val = build_state(&cfg, &u, &state_clone.hostname);
                    *state_clone.last_state.lock().unwrap() = Some(val.clone());
                    Some(val)
                })
                .await;

                if let Ok(Some(snapshot)) = res {
                    let json = snapshot.to_string();
                    // Only emit when the serialized state actually changed. The
                    // worker republishes (bumping generation) even when nothing
                    // changed, so without this the identical full payload would
                    // be re-sent to every client on each publish.
                    if json != last_json {
                        let event = Event::default().event("state").data(json.clone());
                        last_json = json;
                        last_gen = update_gen;
                        yield Ok(event);
                    } else {
                        last_gen = update_gen;
                    }
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// SSE stream of the raw `WorkerUpdate` — the TUI's native format. The web UI
/// consumes `/events` (the `build_state` shape); the TUI consumes this so the
/// daemon's worker is its single source of truth (no duplicate local worker).
/// Emits only when the serialized update changes.
async fn api_events_worker(
    State(state): State<Arc<ServerState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let mut timer = interval(Duration::from_millis(500));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_gen: u64 = 0;
        let mut last_json: String = String::new();
        loop {
            timer.tick().await;
            let update_opt = {
                let latest = state.worker.latest.lock().unwrap();
                latest.clone()
            };
            if let Some(u) = update_opt {
                if u.generation == last_gen {
                    continue;
                }
                let update_gen = u.generation;
                let state_clone = Arc::clone(&state);
                let res = tokio::task::spawn_blocking(move || {
                    // Keep the daemon's worker hints fresh (task/project info) so
                    // the raw WorkerUpdate is fully populated even when the only
                    // client is the TUI on /events/worker (mirrors /events).
                    let cfg = Config::load().ok()?;
                    sync_hints(&state_clone.worker, &cfg);
                    serde_json::to_string(&u).ok()
                })
                .await;
                if let Ok(Some(json)) = res {
                    if json != last_json {
                        let event = Event::default().event("worker").data(json.clone());
                        last_json = json;
                        last_gen = update_gen;
                        yield Ok(event);
                    } else {
                        last_gen = update_gen;
                    }
                }
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod sse_tests {
    use super::*;

    #[test]
    fn test_worker_update_clonable() {
        let u = WorkerUpdate {
            sessions: vec![],
            statuses: Default::default(),
            diff_stats: Default::default(),
            task_diff_stats: Default::default(),
            session_branches: Default::default(),
            session_agents: Default::default(),
            pr_urls: Default::default(),
            project_branches: Default::default(),
            run_sessions: Default::default(),
            resources: Default::default(),
            gpu: Default::default(),
            generation: 0,
        };
        let u2 = u.clone();
        assert_eq!(u2.sessions.len(), 0);
        assert_eq!(u2.generation, 0);
    }
}

/// Reject session names that don't look like showrunner tmux sessions so
/// the API can't be used to poke at arbitrary tmux targets.
fn validate_session_name(name: &str) -> Result<(), ApiError> {
    if name.starts_with("cm") && !name.contains(':') {
        Ok(())
    } else {
        Err(bad_request("not a showrunner session"))
    }
}

#[derive(Deserialize)]
struct OutputParams {
    lines: Option<usize>,
}

async fn api_output(
    Path(name): Path<String>,
    Query(params): Query<OutputParams>,
) -> Result<Response, ApiError> {
    validate_session_name(&name)?;
    let lines = params.lines.unwrap_or(300).clamp(50, 5000);

    let (text, width) = tokio::task::spawn_blocking(move || tmux::capture_output(&name, lines))
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "session not found".to_string()))?;

    Ok(axum::Json(json!({ "text": text, "width": width })).into_response())
}

#[derive(Deserialize)]
struct DiffParams {
    session: Option<String>,
    project: Option<String>,
    branch: Option<String>,
}

/// Unified diff, either for a session's worktree (vs its task branch) or for
/// a task branch (vs its base branch).
async fn api_diff(Query(params): Query<DiffParams>) -> Result<Response, ApiError> {
    if let Some(name) = &params.session {
        validate_session_name(name)?;
    } else if params.project.is_none() || params.branch.is_none() {
        return Err(bad_request("expected ?session= or ?project=&branch="));
    }

    let text = tokio::task::spawn_blocking(move || {
        if let Some(name) = params.session {
            tmux::get_session_diff_text(&name)
        } else {
            let (project_name, branch) = (params.project.unwrap(), params.branch.unwrap());
            let cfg = Config::load().ok()?;
            let project = cfg.projects.iter().find(|p| p.name == project_name)?;
            let base = project
                .tasks
                .iter()
                .find(|t| t.branch == branch)
                .map(|t| t.base_branch().to_string())
                .unwrap_or_else(|| "main".to_string());
            tmux::get_branch_diff_text(&project.path, &branch, &base)
        }
    })
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "diff unavailable".to_string()))?;

    Ok(axum::Json(json!({ "text": text })).into_response())
}

fn default_submit() -> bool {
    true
}

#[derive(Deserialize)]
struct SendBody {
    text: String,
    #[serde(default = "default_submit")]
    submit: bool,
}

async fn api_send(
    Path(name): Path<String>,
    axum::Json(body): axum::Json<SendBody>,
) -> Result<StatusCode, ApiError> {
    validate_session_name(&name)?;

    tokio::task::spawn_blocking(move || {
        if body.text.is_empty() {
            if body.submit {
                tmux::send_key(&name, "Enter")
            } else {
                Ok(())
            }
        } else {
            tmux::send_text(&name, &body.text, body.submit)
        }
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

async fn api_keys(
    Path(name): Path<String>,
    axum::Json(body): axum::Json<KeyBody>,
) -> Result<StatusCode, ApiError> {
    validate_session_name(&name)?;

    const NAMED_KEYS: &[&str] = &[
        "Enter", "Escape", "Up", "Down", "Left", "Right", "Tab", "BSpace",
    ];
    let allowed = NAMED_KEYS.contains(&body.key.as_str())
        || (body.key.chars().count() == 1 && body.key.chars().all(|c| c.is_ascii_alphanumeric()));
    if !allowed {
        return Err(bad_request(format!("key '{}' not allowed", body.key)));
    }

    tokio::task::spawn_blocking(move || tmux::send_key(&name, &body.key))
        .await
        .map_err(internal)?
        .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

async fn api_kill(Path(name): Path<String>) -> Result<StatusCode, ApiError> {
    validate_session_name(&name)?;
    if name.ends_with(&format!("__{}", tmux::MAIN_SESSION)) {
        return Err(bad_request(
            "the main session can't be killed — delete the task instead",
        ));
    }

    tokio::task::spawn_blocking(move || ops::kill_session(&name))
        .await
        .map_err(internal)?
        .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

/// Ingest a normalized agent hook event, POSTed by an agent's hook script.
/// Body: `{ "agent": "claude"|"opencode"|"codex", ...agent-specific payload }`.
/// The event is normalized and kept (bounded) for `GET /api/hook-events`.
async fn api_hook(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    axum::Json(body): axum::Json<Value>,
) -> Result<StatusCode, ApiError> {
    // Hook events are only ever posted by local agent hooks (post-event.sh curls
    // 127.0.0.1). Reject non-loopback sources so an exposed daemon (e.g. bound to
    // a tailnet IP) can't be fed forged hook events that manipulate session
    // status or trigger auto-close.
    if !peer.ip().is_loopback() {
        return Ok(StatusCode::FORBIDDEN);
    }
    let agent = body.get("agent").and_then(Value::as_str).unwrap_or("");
    let raw = body.to_string();
    let event = match agent {
        "claude" => crate::hooks::parse_claude_hook(&raw)
            .ok_or_else(|| bad_request("could not parse claude hook payload"))?,
        "opencode" => crate::hooks::parse_opencode_hook(&raw)
            .ok_or_else(|| bad_request("could not parse opencode hook payload"))?,
        "codex" => crate::hooks::parse_codex_hook(&raw)
            .ok_or_else(|| bad_request("could not parse codex hook payload"))?,
        other => return Err(bad_request(format!("unknown agent '{other}'"))),
    };
    let session_id = body
        .get("session_id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_default();
    // The cwd is the correlation key the worker uses to map the event back to a
    // tmux session (the agent's session_id is not the tmux name).
    let cwd = body
        .get("cwd")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_default();

    {
        let mut evs = state.hook_events.lock().unwrap();
        evs.push_back((session_id.clone(), event.clone(), std::time::Instant::now()));
        while evs.len() > 200 {
            evs.pop_front();
        }
    }
    // Feed the authoritative inbox so the worker refines status with zero
    // pane-scrape latency (permission prompt, turn-completed, session-ended).
    if !cwd.is_empty() {
        state.worker.hook_inbox.lock().unwrap().push_back((cwd, event));
    }
    Ok(StatusCode::OK)
}

/// Recent ingested hook events (oldest first), for debugging/observability.
async fn api_hook_events(State(state): State<Arc<ServerState>>) -> Result<axum::Json<Value>, ApiError> {
    let evs = state.hook_events.lock().unwrap();
    let arr: Vec<Value> = evs
        .iter()
        .map(|(sid, ev, _ts)| json!({ "session_id": sid, "event": ev }))
        .collect();
    Ok(axum::Json(json!({ "events": arr })))
}

#[derive(Deserialize)]
struct CreateProjectBody {
    path: String,
    name: Option<String>,
}

/// Register a new project from a directory on the server's filesystem. Mirrors
/// the TUI's add-project flow: the path must be an existing git repository, and
/// the name defaults to the directory's basename.
async fn api_create_project(
    State(state): State<Arc<ServerState>>,
    axum::Json(body): axum::Json<CreateProjectBody>,
) -> Result<Response, ApiError> {
    let name = tokio::task::spawn_blocking(move || {
        let raw = body.path.trim();
        if raw.is_empty() {
            anyhow::bail!("project path is required");
        }
        let path = std::path::PathBuf::from(crate::app::expand_tilde(raw));
        if !path.is_dir() {
            anyhow::bail!("not a directory: {raw}");
        }
        let path = path.canonicalize().unwrap_or(path);
        let path_str = path.to_string_lossy().to_string();
        if !path.join(".git").is_dir() {
            anyhow::bail!("not a git repository");
        }
        if Config::load()?.has_project_at(&path_str) {
            anyhow::bail!("project already registered");
        }

        let name = body
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".into())
            });

        let name_for_add = name.clone();
        let cfg = Config::modify(move |c| c.add_project(name_for_add, path_str))?;
        sync_hints(&state.worker, &cfg);
        Ok(name)
    })
    .await
    .map_err(internal)?
    .map_err(|e: anyhow::Error| bad_request(e.to_string()))?;

    Ok(axum::Json(json!({ "name": name })).into_response())
}

#[derive(Deserialize)]
struct CreateTaskBody {
    project: String,
    name: String,
    prompt: Option<String>,
    agent: Option<String>,
}

async fn api_create_task(
    axum::Json(body): axum::Json<CreateTaskBody>,
) -> Result<Response, ApiError> {
    let tmux_name = tokio::task::spawn_blocking(move || {
        let cfg = Config::load()?;
        let project = ops::find_project(&cfg, &body.project)?.clone();

        let agent = ops::resolve_agent(&cfg, body.agent.as_deref())?;
        // A new task starts with its main session, like the TUI.
        let (_, tmux_name) = ops::create_task(
            &cfg,
            &project,
            body.name.trim(),
            None,
            None,
            body.prompt.as_deref().filter(|p| !p.trim().is_empty()),
            agent,
        )?;
        Ok::<_, anyhow::Error>(tmux_name)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;

    Ok(axum::Json(json!({ "tmux_name": tmux_name })).into_response())
}

#[derive(Deserialize)]
struct CreateSessionBody {
    project: String,
    task: String,
    prompt: Option<String>,
    agent: Option<String>,
}

async fn api_create_session(
    axum::Json(body): axum::Json<CreateSessionBody>,
) -> Result<Response, ApiError> {
    let tmux_name = tokio::task::spawn_blocking(move || {
        let cfg = Config::load()?;
        let project = ops::find_project(&cfg, &body.project)?.clone();
        let task = project
            .tasks
            .iter()
            .find(|t| t.name == body.task)
            .ok_or_else(|| anyhow::anyhow!("task '{}' not found", body.task))?
            .clone();

        let agent = ops::resolve_agent(&cfg, body.agent.as_deref())?;
        ops::create_session(
            &cfg,
            &project,
            &task.name,
            &task.branch,
            true,
            body.prompt.as_deref().filter(|p| !p.trim().is_empty()),
            agent,
        )
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;

    Ok(axum::Json(json!({ "tmux_name": tmux_name })).into_response())
}

#[derive(Deserialize)]
struct DeleteTaskBody {
    project: String,
    task: String,
}

/// Delete a task: kill its sessions, remove their worktrees/branches, drop the
/// session records, and remove the task from the config. Mirrors the TUI's
/// task delete.
async fn api_delete_task(
    axum::Json(body): axum::Json<DeleteTaskBody>,
) -> Result<StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || {
        let cfg = Config::load()?;
        ops::delete_task(ops::find_project(&cfg, &body.project)?, &body.task)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CreateAdhocBody {
    project: String,
    name: String,
    agent: Option<String>,
}

/// Create an ad-hoc session (no task, no worktree) on a project. Mirrors the
/// TUI's "new adhoc session" flow.
async fn api_create_adhoc(
    axum::Json(body): axum::Json<CreateAdhocBody>,
) -> Result<Response, ApiError> {
    let tmux_name = tokio::task::spawn_blocking(move || {
        let cfg = Config::load()?;
        let project = cfg
            .projects
            .iter()
            .find(|p| p.name == body.project)
            .ok_or_else(|| anyhow::anyhow!("project '{}' not found", body.project))?
            .clone();

        let name = body.name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!("adhoc session name is required");
        }

        let tmux_name = format!(
            "cm__{}__{}__{}",
            tmux::sanitize(&project.name),
            tmux::ADHOC_MARKER,
            tmux::sanitize(&name),
        );
        let sessions = tmux::list_sessions().unwrap_or_default();
        if sessions.iter().any(|s| s.name == tmux_name) {
            anyhow::bail!("adhoc session '{name}' already exists");
        }

        let agent = ops::resolve_agent(&cfg, body.agent.as_deref())?;
        let tmux_name = tmux::create_adhoc_session(
            &project.name,
            &project.path,
            &name,
            &cfg.startup_skills,
            agent,
        )?;
        config::add_session_record(
            &tmux_name,
            config::SessionRecord {
                project_name: project.name.clone(),
                project_path: project.path.clone(),
                task_name: tmux::ADHOC_MARKER.to_string(),
                task_branch: String::new(),
                session_name: name,
                use_worktree: false,
                archived: false,
                auto_closed: false,
                agent: agent.id().to_string(),
            },
        );
        Ok(tmux_name)
    })
    .await
    .map_err(internal)?
    .map_err(|e: anyhow::Error| bad_request(e.to_string()))?;

    Ok(axum::Json(json!({ "tmux_name": tmux_name })).into_response())
}

async fn index() -> Response {
    (
        [(header::CACHE_CONTROL, "no-cache")],
        Html(include_str!("web/index.html")),
    )
        .into_response()
}

async fn app_js() -> Response {
    static_file("application/javascript", include_str!("web/app.js"))
}

async fn style_css() -> Response {
    static_file("text/css", include_str!("web/style.css"))
}

async fn manifest() -> Response {
    static_file("application/json", include_str!("web/manifest.json"))
}

async fn icon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_bytes!("web/icon.png").as_slice(),
    )
        .into_response()
}

fn static_file(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}
