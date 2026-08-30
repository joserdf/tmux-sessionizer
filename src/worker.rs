use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::autoclose::{self, AutoCloseConfig, CloseAction};
use crate::hooks::AgentEvent;
use crate::resources::SessionResources;
use crate::tmux::{self, DiffStats, SessionStatus, TmuxSession};

/// Task info for computing branch diffs.
#[derive(Clone)]
pub struct TaskInfo {
    pub project_name: String,
    pub project_path: String,
    pub branch: String,
    pub base_branch: String,
}

/// Shared state the UI thread writes to, the worker thread reads from.
pub struct WorkerHints {
    pub tasks: Vec<TaskInfo>,
    /// Project name → project path, so the worker can query the current branch.
    pub project_paths: Vec<(String, String)>,
}

/// Data produced by the background worker for the UI to consume.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerUpdate {
    pub sessions: Vec<TmuxSession>,
    pub statuses: HashMap<String, SessionStatus>,
    pub diff_stats: HashMap<String, DiffStats>,
    /// Keyed by branch name.
    pub task_diff_stats: HashMap<String, DiffStats>,
    /// Branch checked out in each session's worktree, keyed by session tmux name.
    pub session_branches: HashMap<String, String>,
    /// Agent harness id per session, keyed by session tmux name.
    pub session_agents: HashMap<String, String>,
    /// PR URLs keyed by branch name.
    pub pr_urls: HashMap<String, String>,
    /// Current git branch for each project, keyed by project name.
    pub project_branches: HashMap<String, String>,
    /// Live "Run" sessions, keyed by tmux name (`cmrun-*`); value is true while
    /// the command is still executing, false once it dropped to a shell.
    pub run_sessions: HashMap<String, bool>,
    /// Per-session CPU/mem, keyed by session tmux name. Sampled every 8th tick
    /// (the 200ms CPU sample is too slow for every tick); the last value is
    /// carried forward on non-sampling ticks.
    pub resources: HashMap<String, SessionResources>,
    /// (pid, used GPU memory MiB) for every process using a GPU; sampled on the
    /// same cadence as `resources`.
    pub gpu: Vec<(u32, u64)>,
    /// Monotonically increasing; bumped on every publish. Lets SSE clients emit
    /// only on change instead of re-sending the identical state each poll.
    pub generation: u64,
}

pub struct Worker {
    pub hints: Arc<Mutex<WorkerHints>>,
    /// Single-slot handoff: the worker overwrites this each tick; the UI takes
    /// it. Using a mutex instead of an unbounded channel prevents updates from
    /// piling up while the main thread is blocked (e.g. during `tmux attach`).
    pub latest: Arc<Mutex<Option<WorkerUpdate>>>,
    /// Authoritative agent-hook events, keyed by the hook's `cwd`, pushed by
    /// `POST /api/hook` and drained by the worker loop to refine status with
    /// zero pane-scrape latency. (cwd, not the agent's session_id, is the
    /// correlation key: it maps back to a tmux session via the records.)
    pub hook_inbox: Arc<Mutex<VecDeque<(String, AgentEvent)>>>,
    /// Live connection to the daemon (remote mode). True while the SSE stream
    /// is actively delivering; false when it has dropped. In local mode this is
    /// always true (the worker IS the source). The UI shows a "daemon
    /// disconnected" banner when `remote && !connected`.
    pub connected: Arc<AtomicBool>,
    /// True when this worker is a client of a remote daemon (vs. a local worker
    /// probing tmux directly).
    pub remote: bool,
}

impl Worker {
    pub fn spawn() -> Self {
        let hints = Arc::new(Mutex::new(WorkerHints {
            tasks: Vec::new(),
            project_paths: Vec::new(),
        }));
        let latest = Arc::new(Mutex::new(None));
        let hook_inbox = Arc::new(Mutex::new(VecDeque::new()));
        // Local mode: the worker is its own source, so it's always "connected"
        // and never remote (no daemon banner).
        let connected = Arc::new(AtomicBool::new(true));

        let hints_clone = hints.clone();
        let latest_clone = latest.clone();
        let inbox_clone = hook_inbox.clone();
        let connected_clone = connected.clone();
        thread::spawn(move || {
            worker_loop(hints_clone, latest_clone, inbox_clone, false, connected_clone)
        });

        Worker {
            hints,
            latest,
            hook_inbox,
            connected,
            remote: false,
        }
    }

    /// The daemon's raw-`WorkerUpdate` SSE URL, from `SESSIONIZER_PORT`
    /// (default 7878).
    pub fn daemon_url() -> String {
        let port = std::env::var("SESSIONIZER_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(7878);
        format!("http://127.0.0.1:{port}/events/worker")
    }

    /// Cheap reachability probe for the daemon (a TCP connect with a short
    /// timeout). Used to pick remote (daemon) vs. local worker without an HTTP
    /// round-trip.
    pub fn daemon_reachable() -> bool {
        let port = std::env::var("SESSIONIZER_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(7878);
        std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(700),
        )
        .is_ok()
    }

    /// Run this worker as a CLIENT of the daemon's SSE stream instead of
    /// spawning a local worker. A background thread (its own tokio runtime)
    /// reads `/events/worker` and writes each `WorkerUpdate` into `latest`,
    /// reconnecting with backoff if the connection drops. A second background
    /// thread runs the local probe loop in *fallback* mode: it stays idle while
    /// the daemon is reachable and takes over publishing (and notifications) the
    /// moment the stream drops, so the dashboard keeps updating when the daemon
    /// dies. `hints` is unused in remote mode but kept for a uniform `Worker`
    /// shape.
    pub fn connect_remote(url: &str) -> Self {
        let hints = Arc::new(Mutex::new(WorkerHints {
            tasks: Vec::new(),
            project_paths: Vec::new(),
        }));
        let latest = Arc::new(Mutex::new(None));
        // Unused in remote mode (the daemon's worker owns the hook inbox); kept
        // for a uniform Worker shape.
        let hook_inbox = Arc::new(Mutex::new(VecDeque::new()));
        // Starts false; the SSE client flips it true once the stream is
        // delivering and back to false on any drop.
        let connected = Arc::new(AtomicBool::new(false));

        let url = url.to_string();
        let latest_sse = latest.clone();
        let connected_sse = connected.clone();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(rt) = rt {
                rt.block_on(sse_client(url, latest_sse, connected_sse));
            }
        });

        // Local fallback prober: idle while the daemon is up, takes over when
        // the stream drops.
        let latest_fb = latest.clone();
        let connected_fb = connected.clone();
        let hints_fb = hints.clone();
        let inbox_fb = hook_inbox.clone();
        thread::spawn(move || {
            worker_loop(hints_fb, latest_fb, inbox_fb, true, connected_fb)
        });

        Worker {
            hints,
            latest,
            hook_inbox,
            connected,
            remote: true,
        }
    }
}

/// Read the daemon's raw-`WorkerUpdate` SSE stream into `latest`, reconnecting
/// with backoff on any drop so a daemon restart recovers transparently. Runs
/// forever (the TUI's process lifetime bounds it).
async fn sse_client(
    url: String,
    latest: Arc<Mutex<Option<WorkerUpdate>>>,
    connected: Arc<AtomicBool>,
) {
    use futures_util::StreamExt;
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut backoff = Duration::from_millis(500);
    loop {
        let ok = match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                backoff = Duration::from_millis(500);
                // Stream is live: the daemon is the authoritative source again.
                connected.store(true, Ordering::Relaxed);
                let mut stream = resp.bytes_stream();
                let mut buf: Vec<u8> = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(b) => b,
                        Err(_) => break,
                    };
                    buf.extend_from_slice(&chunk);
                    // Pull out each complete SSE frame (delimited by a blank line).
                    while let Some(end) = find_frame_end(&buf) {
                        let frame: Vec<u8> = buf.drain(..end).collect();
                        if let Some(update) = parse_sse_frame(&String::from_utf8_lossy(&frame)) {
                            *latest.lock().unwrap() = Some(update);
                        }
                    }
                }
                // Stream dropped (daemon closing / restarting / read error): the
                // local fallback prober takes over from here.
                connected.store(false, Ordering::Relaxed);
                true
            }
            _ => {
                // Couldn't (re)connect the daemon.
                connected.store(false, Ordering::Relaxed);
                false
            }
        };
        if !ok {
            // Couldn't (re)connect: back off and retry.
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}

/// Index (exclusive end) of the first SSE frame in `buf` — i.e., just past the
/// first `\n\n`. Returns None while an incomplete frame is buffered.
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

/// Extract the `WorkerUpdate` from a single SSE frame's `data:` lines.
fn parse_sse_frame(frame: &str) -> Option<WorkerUpdate> {
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(d) = line.strip_prefix("data:") {
            data.push_str(d.trim_start());
        }
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<WorkerUpdate>(&data).ok()
}

/// `fallback` marks a remote-mode worker running as the local fallback prober:
/// it stays idle (no tmux probing) while `connected` is true and takes over
/// publishing once the daemon stream drops. Fallback runs the dashboard probes
/// and notifications but NOT auto-close (a degraded client should not kill
/// sessions; the daemon was the auto-close authority).
fn worker_loop(
    hints: Arc<Mutex<WorkerHints>>,
    latest: Arc<Mutex<Option<WorkerUpdate>>>,
    hook_inbox: Arc<Mutex<VecDeque<(String, AgentEvent)>>>,
    fallback: bool,
    connected: Arc<AtomicBool>,
) {
    let mut content_hashes: HashMap<String, u64> = HashMap::new();
    let mut stable_ticks: HashMap<String, u32> = HashMap::new();
    let mut diff_stats: HashMap<String, DiffStats> = HashMap::new();
    let mut session_branches: HashMap<String, String> = HashMap::new();
    let mut session_agents: HashMap<String, String> = HashMap::new();
    let mut pr_urls: HashMap<String, String> = HashMap::new();
    let mut task_diff_stats: HashMap<String, DiffStats> = HashMap::new();
    let mut project_branches: HashMap<String, String> = HashMap::new();
    // Resource caches: the sampler takes a ~200ms CPU window, so refresh every
    // 8th tick (each tick is a 500ms sleep plus per-session probes, so this is
    // roughly every several seconds) and carry the last values forward in between.
    let mut resources: HashMap<String, SessionResources> = HashMap::new();
    let mut gpu: Vec<(u32, u64)> = Vec::new();
    // Probe once: don't spawn `nvidia-smi` on every tick on GPU-less machines.
    let has_gpu = crate::resources::gpu_available();
    let mut tick: u64 = 0;
    let mut generation: u64 = 0;
    // Auto-close policy (opt-in; disabled by default). Read once at start.
    let auto_close = match crate::config::Config::load() {
        Ok(c) => c.auto_close,
        Err(_) => AutoCloseConfig::default(),
    };
    let mut idle_since: HashMap<String, Instant> = HashMap::new();
    let mut ac_acted: HashMap<String, CloseAction> = HashMap::new();
    let mut prev_statuses: HashMap<String, SessionStatus> = HashMap::new();
    // How many consecutive ticks a session has looked finished. `Finished` is
    // only reported (and thus becomes auto-close eligible) after it persists, so
    // a single transient probe failure can't kill a clean session.
    let mut finished_ticks: HashMap<String, u32> = HashMap::new();
    // Cache of (last_check, dirty) per close candidate, so a blocked (dirty)
    // session isn't re-scanned by `git status` on every auto-close pass.
    let mut dirty_cache: HashMap<String, (Instant, bool)> = HashMap::new();

    loop {
        // Fallback mode: while the daemon is reachable it owns the data, so the
        // local prober stays idle (no duplicate tmux probing). The instant the
        // stream drops, it wakes and takes over.
        if fallback && connected.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
            continue;
        }

        let sessions = tmux::list_sessions().unwrap_or_default();

        // Refresh the resource sample before publishing, so both publishes
        // below carry the latest computed values.
        if tick % 8 == 0 {
            let names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
            resources = crate::resources::sample_sessions(&names);
            gpu = if has_gpu {
                crate::resources::gpu_processes()
            } else {
                Vec::new()
            };
        }

        // Compute statuses
        let mut statuses = HashMap::new();
        const STABLE_THRESHOLD: u32 = 3;
        // A session must look finished this many consecutive ticks before it is
        // reported Finished (and thus becomes auto-close eligible), so a single
        // transient probe failure can't kill a clean session.
        const FINISHED_THRESHOLD: u32 = 3;

        for session in &sessions {
            let probe = tmux::probe_session(&session.name);

            let status = match probe {
                None => {
                    content_hashes.remove(&session.name);
                    stable_ticks.remove(&session.name);
                    finished_status(&mut finished_ticks, &session.name, FINISHED_THRESHOLD)
                }
                Some(probe) if !probe.agent_alive => {
                    content_hashes.remove(&session.name);
                    stable_ticks.remove(&session.name);
                    finished_status(&mut finished_ticks, &session.name, FINISHED_THRESHOLD)
                }
                Some(probe) => {
                    finished_ticks.remove(&session.name);
                    let prev_hash = content_hashes.get(&session.name).copied();
                    let content_changed = prev_hash.is_some_and(|h| h != probe.content_hash);

                    content_hashes.insert(session.name.clone(), probe.content_hash);

                    if content_changed {
                        stable_ticks.insert(session.name.clone(), 0);
                        SessionStatus::Running
                    } else {
                        let ticks = stable_ticks.entry(session.name.clone()).or_insert(0);
                        *ticks = ticks.saturating_add(1);

                        if *ticks < STABLE_THRESHOLD {
                            SessionStatus::Running
                        } else if probe.has_permission_prompt {
                            SessionStatus::WaitingForPermission
                        } else {
                            SessionStatus::WaitingForInput
                        }
                    }
                }
            };

            statuses.insert(session.name.clone(), status);
        }

        // Apply one-shot authoritative hook overrides. A hook event (e.g. a
        // permission prompt) removes the pane-scrape detection latency so the
        // state shows on the very next tick. The hook's `cwd` is correlated back
        // to a tmux session via the records. Consumed here; the pane scrape is
        // authoritative again on the following tick (it keeps or corrects the
        // state).
        {
            let pending: Vec<(String, AgentEvent)> = {
                let mut inbox = hook_inbox.lock().unwrap();
                inbox.drain(..).collect()
            };
            if !pending.is_empty() {
                let records = crate::config::load_sessions();
                let mut cwd_to_session: HashMap<String, String> = HashMap::new();
                for (name, rec) in &records {
                    let cwd = if rec.use_worktree {
                        tmux::worktree_dir(&rec.project_name, &rec.task_name, &rec.session_name)
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        rec.project_path.clone()
                    };
                    cwd_to_session.insert(cwd, name.clone());
                }
                for (cwd, ev) in pending {
                    let Some(status) = ev.status_hint() else { continue };
                    let Some(name) = cwd_to_session.get(&cwd).cloned() else { continue };
                    if sessions.iter().any(|s| s.name == name) {
                        statuses.insert(name, status);
                    }
                }
            }
        }

        // Surface a permission prompt to the OS (once per transition), so the
        // user is alerted even when the TUI is in another pane. The tmux status
        // bar badge is driven separately by the daemon's status.cache.
        for (name, status) in statuses.iter() {
            if *status == SessionStatus::WaitingForPermission
                && prev_statuses.get(name) != Some(&SessionStatus::WaitingForPermission)
            {
                crate::notify::send(&crate::notify::Notification {
                    title: "showrunner: approval needed".to_string(),
                    body: format!("Session '{name}' is waiting for a permission decision."),
                    urgent: true,
                });
            }
        }
        prev_statuses = statuses.clone();

        // Publish statuses right away with the previously computed maps — the
        // git work below can take many seconds on a cold start, and statuses
        // are the most time-sensitive signal.
        generation += 1;
        *latest.lock().unwrap() = Some(WorkerUpdate {
            sessions: sessions.clone(),
            statuses: statuses.clone(),
            diff_stats: diff_stats.clone(),
            session_branches: session_branches.clone(),
            session_agents: session_agents.clone(),
            task_diff_stats: task_diff_stats.clone(),
            pr_urls: pr_urls.clone(),
            project_branches: project_branches.clone(),
            run_sessions: tmux::list_run_sessions(),
            resources: resources.clone(),
            gpu: gpu.clone(),
            generation,
        });

        // Refresh diff stats and terminal counts less frequently (~every 2 seconds)
        if tick % 4 == 0 {
            let session_names: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
            diff_stats.retain(|k, _| session_names.contains(k));
            session_branches.retain(|k, _| session_names.contains(k));
            session_agents.retain(|k, _| session_names.contains(k));

            for session in &sessions {
                if let Some(stats) = tmux::get_diff_stats(&session.name) {
                    diff_stats.insert(session.name.clone(), stats);
                }
                if let Some(branch) = tmux::get_session_branch(&session.name) {
                    session_branches.insert(session.name.clone(), branch);
                }
                session_agents
                    .entry(session.name.clone())
                    .or_insert_with(|| tmux::session_agent(&session.name).id().to_string());
            }
        }

        let (tasks, project_paths) = {
            let h = hints.lock().unwrap();
            (h.tasks.clone(), h.project_paths.clone())
        };

        // Compute task branch diffs (less frequently). Values persist across
        // ticks so consumers always see the latest computed stats.
        if tick % 4 == 0 {
            task_diff_stats.retain(|k, _| tasks.iter().any(|t| t.branch == *k));
            for task in &tasks {
                if let Some(stats) =
                    tmux::get_branch_diff(&task.project_path, &task.branch, &task.base_branch)
                {
                    task_diff_stats.insert(task.branch.clone(), stats);
                }
            }
        }

        // Check for PRs (infrequently, ~every 10 seconds)
        if tick % 20 == 0 {
            for task in tasks.iter() {
                if !pr_urls.contains_key(&task.branch) {
                    if let Some(url) = tmux::get_pr_url(&task.project_path, &task.branch) {
                        // Write PR URL to file so hooks can read it without network calls
                        let pr_path = crate::config::pr_url_path(&task.project_name, &task.branch);
                        if let Some(parent) = pr_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(&pr_path, &url);
                        pr_urls.insert(task.branch.clone(), url);
                    }
                }
            }
        }

        // Compute current branch for each project (less frequently, ~every 2 seconds)
        if tick % 4 == 0 {
            project_branches.retain(|k, _| project_paths.iter().any(|(n, _)| n == k));
            for (name, path) in &project_paths {
                if let Some(branch) = get_current_branch(path) {
                    project_branches.insert(name.clone(), branch);
                }
            }
        }

        // Evaluate the auto-close policy on a reduced cadence (~every 2s),
        // before publishing so a killed session drops out on the next tick.
        // Skipped in fallback mode: a degraded client must not kill sessions
        // (the daemon was the auto-close authority).
        if !fallback && tick % 4 == 0 {
            auto_close_step(
                &auto_close,
                &statuses,
                &session_agents,
                &mut idle_since,
                &mut ac_acted,
                &mut dirty_cache,
            );
        }

        // Prune per-session bookkeeping for sessions that no longer exist so the
        // maps stay bounded as sessions come and go.
        let alive: HashSet<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        prev_statuses.retain(|n, _| alive.contains(n.as_str()));
        ac_acted.retain(|n, _| alive.contains(n.as_str()));
        idle_since.retain(|n, _| alive.contains(n.as_str()));
        finished_ticks.retain(|n, _| alive.contains(n.as_str()));
        dirty_cache.retain(|n, _| alive.contains(n.as_str()));

        generation += 1;
        let update = WorkerUpdate {
            sessions,
            statuses,
            diff_stats: diff_stats.clone(),
            session_branches: session_branches.clone(),
            session_agents: session_agents.clone(),
            task_diff_stats: task_diff_stats.clone(),
            pr_urls: pr_urls.clone(),
            project_branches: project_branches.clone(),
            run_sessions: tmux::list_run_sessions(),
            resources: resources.clone(),
            gpu: gpu.clone(),
            generation,
        };

        *latest.lock().unwrap() = Some(update);

        tick += 1;
        thread::sleep(Duration::from_millis(500));
    }
}

fn get_current_branch(project_path: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", project_path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}



/// Advance a session's finished-stability counter and report its status. A
/// session is only reported `Finished` once it has looked finished (dead pane /
/// missing agent window) for `threshold` consecutive ticks; until then it
/// reports `Running` so a single transient probe failure can't make it an
/// instant auto-close target.
fn finished_status(
    finished_ticks: &mut HashMap<String, u32>,
    name: &str,
    threshold: u32,
) -> SessionStatus {
    let ticks = finished_ticks.entry(name.to_string()).or_insert(0);
    *ticks = ticks.saturating_add(1);
    if *ticks >= threshold {
        SessionStatus::Finished
    } else {
        SessionStatus::Running
    }
}

/// Apply the auto-close policy to agent sessions (never plain terminals or run
/// sessions). A clean finished/idle session is closed; a dirty one is left
/// alone but surfaced once via a notification. Uses `kill_session_only` so the
/// worktree/branch/records are preserved.
fn auto_close_step(
    cfg: &AutoCloseConfig,
    statuses: &HashMap<String, SessionStatus>,
    session_agents: &HashMap<String, String>,
    idle_since: &mut HashMap<String, Instant>,
    acted: &mut HashMap<String, CloseAction>,
    dirty_cache: &mut HashMap<String, (Instant, bool)>,
) {
    if !cfg.enabled {
        return;
    }
    // Records are the authoritative set of showrunner sessions and give each
    // session's work dir. A session with no record (e.g. a hand-made `cm__*`
    // session) is not ours to auto-close.
    let records = crate::config::load_sessions();
    // Re-check `git status` at most every DIRTY_RECHECK so a blocked (dirty)
    // candidate doesn't pay a full scan of its (possibly large) repo every 2 s,
    // while still catching the user committing within a bounded window.
    const DIRTY_RECHECK: Duration = Duration::from_secs(10);

    for (name, _agent) in session_agents.iter() {
        let Some(status) = statuses.get(name) else {
            continue;
        };
        let finished = *status == SessionStatus::Finished;

        // Track how long the session has been idle (agent finished a turn and
        // is waiting for input). WaitingForPermission is NOT idle (it needs
        // action), and Running is actively working.
        if *status == SessionStatus::WaitingForInput {
            idle_since.entry(name.clone()).or_insert(Instant::now());
        } else {
            idle_since.remove(name);
        }
        let idle = idle_since.get(name).is_some_and(|since| {
            cfg.idle_secs
                .is_some_and(|secs| since.elapsed() >= Duration::from_secs(secs))
        });

        if !finished && !idle {
            acted.remove(name);
            dirty_cache.remove(name);
            continue;
        }

        let Some(record) = records.get(name) else {
            continue;
        };

        // Fail-safe dirty check on the authoritative work dir (worktree or
        // project path), not the user-mutable pane cwd. Any read error is
        // treated as dirty, so a session we can't verify is never closed.
        let dirty = match dirty_cache.get(name) {
            Some((when, d)) if when.elapsed() < DIRTY_RECHECK => *d,
            _ => {
                let work_dir = if record.use_worktree {
                    tmux::worktree_dir(&record.project_name, &record.task_name, &record.session_name)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    record.project_path.clone()
                };
                let d = tmux::worktree_dirty_failsafe(&work_dir);
                dirty_cache.insert(name.clone(), (Instant::now(), d));
                d
            }
        };
        let reason = if finished { "finished" } else { "idle" };

        let action = autoclose::evaluate(
            cfg,
            &autoclose::SessionCloseState {
                finished,
                idle,
                has_uncommitted_changes: dirty,
            },
        );

        match action {
            CloseAction::Close => {
                let _ = tmux::kill_session_only(name);
                // Mark the record so the TUI's startup restore doesn't bring an
                // auto-closed session back (restart / unarchive clears it).
                crate::config::set_session_auto_closed(name, true);
                idle_since.remove(name);
                acted.remove(name);
                dirty_cache.remove(name);
                crate::notify::send(&crate::notify::Notification {
                    title: "showrunner: auto-closed".to_string(),
                    body: format!("Session '{name}' was auto-closed ({reason}, clean)."),
                    urgent: false,
                });
            }
            CloseAction::Confirm => {
                if acted.get(name) != Some(&CloseAction::Confirm) {
                    acted.insert(name.clone(), CloseAction::Confirm);
                    crate::notify::send(&crate::notify::Notification {
                        title: "showrunner: auto-close blocked".to_string(),
                        body: format!("Session '{name}' has uncommitted changes; it was not auto-closed."),
                        urgent: true,
                    });
                }
            }
            CloseAction::None => {
                acted.remove(name);
            }
        }
    }
}

#[cfg(test)]
mod sse_client_tests {
    use super::*;
    use crate::tmux::TmuxSession;

    fn sample_update() -> WorkerUpdate {
        WorkerUpdate {
            sessions: vec![TmuxSession {
                name: "cm__proj__task__s1".to_string(),
                project_name: "proj".to_string(),
                task_name: "task".to_string(),
                session_name: "s1".to_string(),
            }],
            statuses: Default::default(),
            diff_stats: Default::default(),
            task_diff_stats: Default::default(),
            session_branches: Default::default(),
            session_agents: Default::default(),
            pr_urls: Default::default(),
            project_branches: Default::default(),
            run_sessions: Default::default(),
            resources: Default::default(),
            gpu: vec![(4242, 512)],
            generation: 7,
        }
    }

    #[test]
    fn workerupdate_round_trips() {
        let u = sample_update();
        let json = serde_json::to_string(&u).unwrap();
        let back: WorkerUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.generation, 7);
        assert_eq!(back.sessions.len(), 1);
        assert_eq!(back.sessions[0].name, "cm__proj__task__s1");
        assert_eq!(back.gpu, vec![(4242, 512)]);
    }

    #[test]
    fn parse_sse_frame_extracts_worker_update() {
        let json = serde_json::to_string(&sample_update()).unwrap();
        let frame = format!("event: worker\ndata: {json}\n\n");
        let parsed = parse_sse_frame(&frame).expect("should parse");
        assert_eq!(parsed.generation, 7);
        assert_eq!(parsed.sessions[0].session_name, "s1");
    }

    #[test]
    fn parse_sse_frame_ignores_non_data_frames() {
        assert!(parse_sse_frame(": keep-alive\n\n").is_none());
        assert!(parse_sse_frame("\n").is_none());
    }

    #[test]
    fn find_frame_end_locates_blank_line() {
        assert_eq!(find_frame_end(b"data: x"), None);
        assert_eq!(find_frame_end(b"data: x\n\n"), Some(9));
        assert_eq!(find_frame_end(b"data: a\n\ndata: b"), Some(9));
    }
}
