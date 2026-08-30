# Code Review — SSE, resource monitoring, OpenCode harness, bootstrap bash, TUI resources

Scope: the six areas requested (SSE endpoint, `/proc` resource sampling, tmux helpers +
OpenCode agent command, OpenCode harness profile, bootstrap bash, TUI RES display).
Read-only review; no source files modified.

Verification done: `cargo test` (116 passed, 1 ignored), live `opencode --help` /
`opencode run --help` (v1.18.25, the version installed on this machine), and a
live `opencode serve` run to check the process `comm` name.

---

## Summary

| Severity | Count |
|----------|-------|
| Critical | 0 |
| High     | 4 |
| Medium   | 5 |
| Low      | 10 |
| Nit      | 5 |

**Top 3 issues**

1. `src/tmux.rs:1046-1056` — the OpenCode launch passes the initial prompt as a
   *positional* argument, but opencode's TUI positional is the **project
   directory** (`opencode [project]`, confirmed via `opencode --help`). Sessions
   created with a prompt — the primary task-creation flow — get a command like
   `opencode 'Fix the login bug'`, which opencode interprets as a path. The
   correct flag is `--prompt` (present in v1.18.25).
2. `src/tmux.rs:1046-1056` — the OpenCode branch silently drops the
   `system_prompt` (the Showrunner session-context briefing) entirely, and
   launches without the auto-approve flag (`--auto`) that the other three agents
   all get (`--dangerously-skip-permissions` / yolo / `--approve`). Result:
   opencode sessions run un-briefed and stall on every permission dialog.
3. `helpers/alert_status.sh:5-7` — the tmux status-bar badge reads
   `status.cache`, but **nothing in the current flow writes that file**: the
   comment claims the Rust daemon writes it (it doesn't), and the legacy
   `alerts.sh` daemon that does write it is never started by `sessionizer.tmux`.
   The badge permanently displays `0`.

---

## High

### H1. OpenCode initial prompt is passed as a positional = project directory
- **Where:** `src/tmux.rs:1051-1054` (`AgentKind::OpenCode` branch of `build_agent_command`)
- **What:** `cmd.push_str(&shell_escape(prompt))` appends the prompt as a bare
  positional. `opencode --help` (v1.18.25, verified on this machine) documents the
  default TUI command as `opencode [project]` with `Positionals: project — path to
  start opencode in`. There is no message positional for the TUI.
- **Why it matters:** `create_task_session`/`create_session` pass
  `build_initial_prompt(...)` (startup skills + user task, `src/tmux.rs:393`), so
  the common "new task with prompt" flow generates `opencode 'Run these startup
  skills... Task: ...'` — opencode treats that string as a directory and the
  session fails to start (or starts in the wrong project). Shell quoting is
  correct (`shell_escape` + tmux's own arg tokenizer), so this is purely a flag
  mistake.
- **Fix:** use the TUI's prompt flag:
  `opencode --auto [--continue] [--prompt <escaped>]` (`--prompt "prompt to use"`
  exists in the TUI options; `opencode run [message..]` is the non-interactive
  alternative but exits when done, so it is the wrong shape for a tmux pane).

### H2. OpenCode branch drops `system_prompt` (session briefing) entirely
- **Where:** `src/tmux.rs:1046-1056`
- **What:** The function contract (`src/tmux.rs:972-976`) is "agents without a
  system-prompt flag get it prepended to the first message instead" — Claude/Pi
  use `--append-system-prompt`, Codex merges it into the positional prompt when
  `!resume`. The OpenCode branch only looks at `initial_prompt`;
  `system_prompt` (the project/branch/PR/skills briefing built at
  `src/tmux.rs:1211-1247`) is never delivered to opencode.
- **Why it matters:** opencode sessions run with none of the Showrunner context
  (task branch rules, "merge into task branch, never push the worktree branch",
  the manage-sessions skill pointer) — silently, with no error.
- **Fix:** mirror the Codex branch: when `!resume`, combine
  `Some(sp)+Some(p) → "{sp}\n\n{p}"`, `Some(sp)+None → sp`, and pass it via
  `--prompt`.

### H3. OpenCode launches without auto-approve, unlike the other three agents
- **Where:** `src/tmux.rs:1047` (`let mut cmd = String::from("opencode");`)
- **What:** Claude gets `--dangerously-skip-permissions`, Codex
  `--dangerously-bypass-approvals-and-sandbox`, Pi `--approve`. OpenCode gets
  nothing. `opencode --help` shows the equivalent: `--auto — auto-approve
  permissions that are not explicitly denied (dangerous!)`.
- **Why it matters:** with default permissions, opencode prompts on bash/edit
  calls, so every session stalls in the tmux pane while the TUI shows
  "WaitingForPermission" — the exact failure mode the other agents were
  configured to avoid. The whole harness design (attention detection, `ask`
  flow) assumes yolo-style launch.
- **Fix:** `let mut cmd = String::from("opencode --auto");`

### H4. Status-bar alert badge has no writer in the current flow
- **Where:** `helpers/alert_status.sh:5-7` (comment), `:10-18` (read); writer
  missing repo-wide
- **What:** The script reads `$CACHE_DIR/status.cache` and its comment says it is
  "a single integer written by the showrunner Rust daemon". No Rust code writes
  `status.cache` (grep across `src/`: zero hits). The only writer is
  `helpers/alerts.sh:172-175` (`alert_update_indicators`), but the `alerts.sh`
  daemon (`alert_daemon_start`) is never invoked by anything: `sessionizer.tmux`
  only starts the Rust daemon (`scripts/daemon.sh`), and `scripts/main.sh`
  (which doesn't source `alerts.sh` either) is no longer wired into any binding.
- **Why it matters:** the `#(…/alert_status.sh)` badge in `status-right`
  permanently shows `⚠ 0`, and the comment actively misdirects future debugging
  toward the Rust daemon.
- **Fix:** pick one owner: (a) have the Rust daemon write the count of
  `WaitingForPermission`/`WaitingForInput` sessions to `status.cache` each tick
  (it already computes these), or (b) start the `alerts.sh` daemon from
  `sessionizer.tmux`, or (c) delete the badge until a writer exists. Update the
  comment to match whichever is chosen.

---

## Medium

### M1. SSE `/events` re-emits the full state every 500 ms per client regardless of changes
- **Where:** `src/server.rs:225-246` (clone), `:221` (interval), `:253` (KeepAlive)
- **What:** `api_events` does `state.worker.latest.lock().unwrap().clone()` — it
  never consumes the update (contrast `api_state:182` which uses `.take()`). The
  worker republishes `latest` once or twice per tick (~1–3 s), but the SSE loop
  wakes every 500 ms and, finding the same `Some(u)`, rebuilds and re-sends the
  identical full state JSON every 500 ms. Each emission also does a
  `Config::load()` disk read + `build_state` on a `spawn_blocking` thread.
  Additionally, `tokio::time::interval` defaults to `MissedTickBehavior::Burst`,
  so a client that stalls (slow network) gets a burst of duplicate state events
  on resume.
- **Why it matters:** 2–4× redundant bandwidth/CPU per client, scaling linearly
  with client count — this is the mobile-web path over tailnet. Latent today
  (the in-repo `src/web/app.js` only polls `/api/state`; no `EventSource` for
  `/events` exists in the repo), but the route is public and the next client
  will inherit the behavior.
- **Fix:** only emit on change — e.g. have the worker bump a `generation: u64`
  in the update (or store `(generation, WorkerUpdate)`), have the SSE loop track
  the last emitted generation, and skip when unchanged. Also
  `timer.set_missed_tick_behavior(MissedTickBehavior::Skip)` (stable in
  tokio 1.50, per Cargo.lock) and drop the custom 500 ms ping path or
  `KeepAlive::default()` (they overlap; see N3).

### M2. OpenCode attention markers: 4 of 6 never match, 1 is a generic substring
- **Where:** `src/agent.rs:95-102`; matching in `src/tmux.rs:2082-2096`
  (`detect_attention_dialog`, tail = last 12 non-empty lines, case-sensitive
  `contains`)
- **What:** opencode's real permission dialog (from
  `packages/tui/src/routes/session/permission.tsx` and issue reports) renders:
  header `△ Permission required`, tool line, then buttons
  `Allow once / Allow always / Reject` with `⇆ select  enter confirm`. So:
  - live: `"Permission required"` ✓, `"Allow "` ✓ (matches "Allow once/always")
  - dead: `"Approve "`, `"Do you want to run"` (Claude phrasing),
    `"❯ [Y/n]"`, `"[y/N]"` (opencode uses arrow selection, not y/N prompts)
  - false-positive risk: `"Allow "` is a common English word — a model answer or
    code line ending in "…Allow something" in the last 12 pane lines flips an
    idle session to WaitingForPermission. Compare the specificity of the other
    agents: Codex `"› 1."` and Pi `"↑↓ navigate"` are near-unique UI strings.
  - missed dialogs: opencode's `question` tool (AskUserQuestion) and the
    `doom_loop` "Continue after repeated failures" dialog are not covered, so
    those stall as WaitingForInput (green) instead of permission (magenta `!`).
- **Why it matters:** status is the core signal of the TUI/`ask` flow
  (`src/cli.rs:653` bails on `has_permission_prompt`); both misfires and misses
  corrupt it for opencode sessions.
- **Fix:** use the exact rendered strings: `["Permission required",
  "Allow once", "Allow always", "Reject permission"]` (drop the four dead
  markers); optionally add the question-dialog header once captured. Validate
  against a live pane the way the Pi/Codex markers were ("captured from
  pi 0.84.2 panes" per `src/agent.rs:182-183`).

### M3. OpenCode `is_prompt_chrome` is too loose — `│`/`>` prefix can chop transcript
- **Where:** `src/agent.rs:130-137`; consumed by `trim_pane` at `src/cli.rs:729`
  (`showrunner ask` reply extraction)
- **What:** The heuristic trims from the *last* line matching
  `line == "❯" || line == ">" || starts_with('╭') || starts_with('└') ||
  starts_with('│') || ≥10-char ─/━ rule`. `starts_with('│')` and bare `>` are
  generic: tool output that prints boxed content (tables, tree dumps, captured
  panes, some LSP output) ending in a `│`-prefixed line after the real
  transcript will be treated as chrome and truncated — corrupting the extracted
  reply. The code comment itself admits it is unvalidated ("may need validation
  against specific OpenCode TUI themes").
- **Why it matters:** `ask` is the cross-session primitive other agents use; a
  truncated reply is a silent data corruption in that flow.
- **Fix:** capture a real opencode pane (idle + dialog states), pin the exact
  input-box glyphs, and prefer the distinctive frame (e.g. the bottom border
  line pattern with its hint text) over any single box character.

### M4. `ui_get_session_alert_count` expects a 6th field (PID) that no in-repo writer produces
- **Where:** `scripts/lib/ui.sh:448-481` (field read at `:456`); conflicting
  consumer `helpers/alerts.sh:112-134`
- **What:** `ui.sh` reads `tail -1 opencode-hook.state | cut -d'|' -f6` as the
  alerting PID, then walks the ppid chain. The only in-repo consumer of that
  same file, `alerts.sh`, defines a **5-field** format
  (`tool|event_type|session_id|description|ts`) — no PID field. So either the
  out-of-repo opencode hook writes 6 fields (and `alerts.sh` parses it wrong) or
  5 fields (and `ui.sh`'s per-session badge can never light up). Additionally,
  even when the PID matches, the badge displays the *global* count on that one
  session, and only the last line (`tail -1`) is ever considered.
- **Why it matters:** per-session alert badges are either dead or misattributed;
  two consumers with contradictory formats for one state file is a maintenance
  trap. (Context: `main.sh`/`ui.sh` is legacy — no binding invokes it anymore —
  which makes the whole block dead weight rather than user-visible, but the task
  asked for it specifically.)
- **Fix:** document the canonical `opencode-hook.state` schema in one place
  (header comment in `alerts.sh`), have `ui.sh` conform to it, and compute a
  real per-session count; or delete the legacy block.

### M5. Daemon PID file: stale/recycled PIDs are trusted and force-killed
- **Where:** `scripts/daemon.sh:53-60` (`daemon_status`), `:33-50` (`daemon_stop`,
  `kill -9` at `:45`)
- **What:** Liveness is `kill -0 $pid` only. If the daemon dies and the OS
  recycles its PID to an unrelated process: `daemon_status` reports "running"
  (so `daemon_start` no-ops and the daemon stays dead), and `daemon_stop` sends
  `SIGKILL` to the unrelated process at line 45.
- **Why it matters:** a status toggle that lies, and a `kill -9` aimed at
  whatever happens to hold the old PID. Low probability, high impact if hit.
- **Fix:** before trusting or killing, verify identity:
  `ps -p "$pid" -o comm= | grep -qx showrunner` (or `pgrep -x showrunner`), and
  only then act; treat mismatch as stale PID and clean up.

---

## Low

### L1. First-pass `/proc` read failure inflates CPU% for that pid
- **Where:** `src/resources.rs:71-74` (`ticks1 ... unwrap_or(0)`) and `:79-83`
- **What:** If `read_cpu_ticks` fails transiently on the *first* read for an
  existing pid, `base = 0` and the second read's full lifetime tick count is
  counted as if it happened inside the ~200 ms window → a one-sample CPU% spike
  for that session. (The mirror case — pid vanishing *between* passes — is
  handled correctly by `saturating_sub`; pids created *during* the window are
  also correct, since their ticks start at 0 at creation. Both were verified
  against the code, not just asserted.)
- **Fix:** when `ticks1` is missing, set `base = ticks2` (delta 0) instead of 0.

### L2. One `tmux list-panes` spawn per session per sample
- **Where:** `src/resources.rs:54` inside `sample_sessions`
- **What:** The batched sampler still does N `tmux list-panes -t <name>` process
  spawns (one per session, every ~8th tick). A single `tmux list-panes -a -F
  "#{session_name}\t#{pane_pid}"` call would fetch all of them — the exact
  pattern already used by `list_run_sessions` (`src/tmux.rs:889-898`).
- **Fix:** batch the pane-pid lookup; keep `list_pane_pids` for the single-name
  callers.

### L3. Hard-coded `PAGE_SIZE = 4096`
- **Where:** `src/resources.rs:20`, used at `:172`
- **What:** RSS KiB = resident pages × 4096 / 1024. On 16K-page (arm64) or 64K
  kernels the number is off by 4×/16×. Fine on this x86_64 box, wrong elsewhere.
- **Fix:** read `VmRSS:` from `/proc/<pid>/status` (already in KiB, no page-size
  assumption) — also one fewer format edge case than `statm`.

### L4. `gpu_available()` is dead code; `gpu_processes()` runs unconditionally
- **Where:** `src/resources.rs:176-182` (unused — zero callers outside the
  module), `src/worker.rs:100`
- **What:** The worker spawns `nvidia-smi` every 8th tick on every machine,
  including GPU-less ones where it just fails. The helper that would gate it
  was written but never wired.
- **Fix:** `let has_gpu = gpu_available();` once at worker start; only call
  `gpu_processes()` when true — or delete `gpu_available()`.

### L5. `gpu_processes()` has no timeout
- **Where:** `src/resources.rs:186-200`
- **What:** `std::process::Command::output()` blocks indefinitely; a wedged
  nvidia driver (a known nvidia-smi failure mode) hangs the worker thread and
  freezes *all* status updates, not just GPU data.
- **Fix:** run it with a deadline (e.g. spawn, `try_wait` loop, kill on
  timeout), or at minimum move it behind the `gpu_available()` gate and accept
  the risk in a comment.

### L6. `#[ignore]` runtime test doesn't test what it claims
- **Where:** `src/resources.rs:250-279`
- **What:** The pane runs a *sleeping* Python loop (`time.sleep(0.001)` ×
  200000 ≈ 200 s of near-zero CPU), so the CPU-attribution path is never
  exercised — only `mem_kb > 0` is asserted. If a prior run crashed, the
  `_sr_runtime` session already exists and `new-session` fails silently
  (`let _ =`), the test then measures the stale session, and the final
  `kill-session` is best-effort (no cleanup on panic).
- **Fix:** use a real CPU burner (e.g. `yes > /dev/null` or a Python busy loop),
  assert `cpu_percent > 0`, kill any pre-existing session first, and clean up in
  a guard that also runs on failure.

### L7. `@tmux_sessionizer_loaded` is set but never read
- **Where:** `sessionizer.tmux:25-26`
- **What:** The comment says this marks "bindings + auto-start only run once
  per server (idempotent)", but nothing reads the option. The keybinds
  re-run on every source (harmless — `bind-key` overwrites), and the
  auto-start is actually guarded by the *different* option
  `@tmux_sessionizer_daemon_autostart` (`:38-40`).
- **Fix:** delete the dead option and fix the comment, or actually guard the
  bind block with it.

### L8. Popup command: PATH resolution at keypress time + paths with spaces
- **Where:** `sessionizer.tmux:13-21` (discovery), `:29, :32` (bindings)
- **What:** Discovery runs in the shell that sources the plugin, but when
  discovery falls back to bare `showrunner` (`:20`), the *tmux server's*
  environment resolves PATH at keypress time — the server's PATH (from when it
  was started) may lack e.g. linuxbrew dirs, giving a "command not found"
  error pane. Separately, the resolved path is interpolated into the binding
  string, so a repo path containing spaces word-splits inside
  `display-popup … -E /path with space/showrunner`.
- **Fix:** in the fallback branch, require `command -v` to return an absolute
  path and warn if not; quote the binary in the binding
  (`display-popup … -E '"$SESSIONIZER_BIN"'` style — or better, invoke via
  `run-shell -b` with a properly quoted command).

### L9. `daemon_start` double-start race
- **Where:** `scripts/daemon.sh:16-30`
- **What:** Two near-simultaneous starts (fast double M-a, or auto-start racing
  a manual start) both pass `daemon_status`, both `nohup` — the second loses
  the bind on `127.0.0.1:7878` and exits, leaving a dead PID in the file. The
  next status call cleans it up, but in the meantime the toggle reports
  "started" while the *first* instance (unknown to the PID file) is what's
  actually serving.
- **Fix:** make the PID file the lock: `set -C; echo $! > "$PID_FILE"` (O_EXCL
  via `>`) and fail the start if the file already exists, or check
  `fuser 7878/tcp` before starting.

### L10. `process_name() == id()` breaks for node-script installs of opencode
- **Where:** `src/agent.rs:70-72`; consumer `src/tmux.rs:2040-2059`
- **What:** Verified on this machine: the current npm binary (`opencode.exe`,
  ELF) sets its own `comm` to `opencode`, so `pane_comm == "opencode"` and
  `pgrep -x opencode` both work. But opencode installed as a plain node script
  (older releases) runs with `comm = node`, in which case every opencode
  session reads as `Finished` immediately (same class of failure the Codex
  comment at `src/agent.rs:67-69` describes for codex).
- **Fix:** at minimum, extend `process_name`/the probe for OpenCode to also
  accept `node` with an argv check (`pgrep -f "opencode"`), or document the
  native-binary requirement.

---

## Nit

### N1. `mem_human` constant names and boundary rendering
- **Where:** `src/ui.rs:543-555`
- **What:** `MB`/`GB` actually hold KiB thresholds of 1 GiB / 1 TiB (misleading
  names); `kb = 1048575` renders as `1024.0M` instead of `1.0G`; suffixes are
  binary units labelled with decimal letters (consistent with the `CPU x% · 1.2G`
  style, just worth knowing).
- **Fix:** rename to `KIB_PER_GIB`-style or restructure with
  `(value, unit)` pairs; optionally use `%.0f` below 10 to get `1024M → 1.0G`
  via unit promotion.

### N2. `CPU {:.0}%` rounding
- **Where:** `src/ui.rs:539`
- **What:** Rust float formatting rounds half-to-even (`12.5 → "12"`); negative
  and zero are safe (cpu_percent can't go negative — `saturating_sub` upstream).
  Cosmetic only.

### N3. Double ping mechanism in the SSE stream
- **Where:** `src/server.rs:249` (custom 500 ms `comment("ping")`) and `:253`
  (`KeepAlive::default()`, 10 s ping)
- **What:** Both emit SSE comments; the custom path only fires when idle, so it's
  redundant belt-and-suspenders with KeepAlive. Harmless, but pick one so the
  keepalive interval has a single owner.

### N4. `futures-util` dep is wider than needed
- **Where:** `Cargo.toml:23`, used only at `src/server.rs:12`
  (`use futures_util::stream::Stream`)
- **What:** The `Stream` trait is re-exported from `futures-core`; `futures-util`
  is already in the dep graph via axum so this costs nothing at build time, but
  the minimal honest dependency is `futures-core = "0.3"`.

### N5. Stale doc comments
- **Where:** `src/worker.rs:86-87` ("refresh every 8th tick (~4s)" — a tick is
  500 ms sleep *plus* per-session tmux/git probes, so the real cadence is
  ~8–24 s); `src/resources.rs:1-5` ("for ALL tmux sessions" — the worker only
  samples showrunner `cm*` sessions via `list_sessions`;
  `all_sessions_resources()` is only used by the ignored test)
- **Fix:** reword both to match actual behavior.

---

## Positive Observations

- `/proc/<pid>/stat` parsing is done right where it's easy to get wrong:
  `rfind(')')` handles comm values containing spaces/parens, and the field
  indices are correct (ppid = token 1 after comm; utime/stime = tokens 11/12 —
  verified against the man-page field layout) (`src/resources.rs:106-162`).
- The batched sampler is a genuinely good design: one ppid-map build and one
  shared 200 ms CPU window for all sessions instead of N sequential samples
  (`src/resources.rs:42-103`), with the worker carrying the cache forward so the
  UI never flickers between samples (`src/worker.rs:86-101, 161, 240`).
- Client-disconnect handling in the SSE stream is correct by construction: axum
  drops the response body, the `async_stream` generator is dropped, and the
  `Infallible` error type plus the ping fallback mean a failed
  `Config::load()` degrades to a keepalive comment instead of killing the
  stream (`src/server.rs:217-254`).
- `shell_escape` uses the correct single-quote idiom (`'\''`), and tmux's own
  argument tokenizer honors those quotes, so prompt injection into the pane
  command is not possible (`src/tmux.rs:123-125`).
- Good security defaults in the API: loopback-only bind (`127.0.0.1:7878`,
  `src/cli.rs:113-122`), `validate_session_name` gating all tmux-targeting
  routes, and a strict allow-list for injected keys (`src/server.rs:282-288,
  395-402`).
- The RES column integrates cleanly with the existing autosizing metadata layout
  (column dropped when empty, header `RES` maxed into the width, adhoc rows
  included in sizing) (`src/ui.rs:518-526, 590, 1123-1148`).
- Empirical verification was possible and encouraging: the installed opencode
  npm binary self-reports `comm=opencode`, so the agent-liveness probe works for
  current versions (see L10 for the older-install caveat).

---

## Resolution (post-review)

Fixed across commits `44b9571` (feat) + `a78f29b` (fix): **all 4 High (H1–H4),
M1, M2, M3, L1–L10, N3, N4, N5.**

Not addressed (intentional):
- **M4** — **resolved by deletion.** Commit `461732b` ("…+ drop legacy bash")
  removed the superseded legacy bash TUI (`scripts/main.sh`, `scripts/lib/ui.sh`,
  `helpers/alerts.sh`), so the contradictory `opencode-hook.state` 6-field reader
  no longer exists. Only `scripts/daemon.sh` + `helpers/alert_status.sh` remain.
- **N1 / N2** — cosmetic (`mem_human` unit-constant naming; `CPU {:.0}%`
  half-to-even rounding). Still intentionally open.

Verification (at the time): `cargo test` 116 pass; SSE idle emits ~1 state/6s
(was ~12); daemon `status.cache` written; recycled-PID rejected; runtime resource
test exercises a real CPU burner.

---

### Second, broader review (G1–G6) — 2026-08

A follow-up whole-codebase review grouped its findings into six areas. All were
fixed in `91e87f4..f018422` (six commits), followed by a hardening pass
(`94cf3ca` leaks, `0d52a31` hung-daemon detection, `2edad7b` safety tests):
- **G1 hooks** — authoritative agent-hook model (`AgentEvent::status_hint`,
  `PermissionRequest` wired), `post-event.sh` as a thin forwarder, plugin
  `hooks.json` installed into the worktree; `set_session_env` propagates the
  daemon port.
- **G2 consumption** — `POST /api/hook` → worker `hook_inbox` keyed by cwd →
  cwd→session correlation → one-shot status override (zero pane-scrape latency).
- **G3 auto-close** — fail-safe dirty check on the record's work dir (error ⇒
  dirty), 3-tick finished-stability guard, `auto_closed` record marker (restore
  skips it, restart/unarchive clears it), bounded per-session bookkeeping.
- **G4 notifications** — spawn-and-detach with a reaper thread, so a wedged
  notifier can't stall the single worker polling thread.
- **G5 TUI** — approve gated on `WaitingForPermission`, unique paste buffer per
  send, content-sized resource panel, worker-hint dedup.
- **G6 scope** — daemon-death local fallback + "daemon down" banner (with
  hung-daemon detection), `/api/hook` loopback-only, docs.

Verification: `cargo test` 176 pass; hook → status e2e confirmed over a live SSE
stream; the failsafe-dirty property and the hook loopback policy are now
unit-tested.
