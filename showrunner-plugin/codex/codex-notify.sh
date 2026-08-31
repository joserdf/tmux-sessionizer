#!/usr/bin/env bash
# Forward a Codex `notify` event to the local showrunner daemon.
#
# Codex's legacy `notify` spawns this program after each completed turn and
# passes the event JSON as the LAST command-line argument (NOT stdin); its
# stdin/stdout/stderr are nulled. We only ever get `agent-turn-complete`, which
# we map to the daemon's canonical codex "Stop" event (a finished turn -> the
# agent is idle). The `cwd` from the payload is the correlation key the daemon
# uses to find the tmux session.
#
# Best-effort: every failure is swallowed so a missing daemon never affects
# codex. The daemon normalizes the event at the boundary
# (see hooks::parse_codex_hook).
set +e
PORT="${SESSIONIZER_PORT:-7878}"
EV="${1:-}"
python3 - "$PORT" "$EV" <<'PY' 2>/dev/null || true
import sys, json
import urllib.request

port, ev_raw = sys.argv[1], sys.argv[2]
try:
    ev = json.loads(ev_raw) if ev_raw else {}
except Exception:
    sys.exit(0)
if not isinstance(ev, dict):
    sys.exit(0)

body = {"agent": "codex", "hook": "Stop", "cwd": ev.get("cwd") or ""}

try:
    req = urllib.request.Request(
        "http://127.0.0.1:%s/api/hook" % port,
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    urllib.request.urlopen(req, timeout=1)
except Exception:
    pass
PY
exit 0
