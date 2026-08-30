#!/usr/bin/env bash
# Forward a Claude Code hook event to the local showrunner daemon.
# Best-effort: every failure is swallowed so the agent is never blocked or
# altered by a missing daemon. The daemon normalizes the event via /api/hook.
#
# The event JSON arrives on stdin; it is captured into a variable first and
# passed to python as an argument (the heredoc owns python's stdin).
set +e
PORT="${SESSIONIZER_PORT:-7878}"
EV="$(cat || true)"
python3 - "$PORT" "$EV" <<'PY' 2>/dev/null || true
import sys, json
import urllib.request

port = sys.argv[1]
try:
    ev = json.loads(sys.argv[2]) if sys.argv[2] else {}
except Exception:
    sys.exit(0)
if not isinstance(ev, dict):
    sys.exit(0)

ev["agent"] = "claude"
# The raw payload (including notification_type when present) is forwarded as-is;
# the daemon normalizes it at the boundary (see hooks::parse_claude_hook).

try:
    req = urllib.request.Request(
        "http://127.0.0.1:%s/api/hook" % port,
        data=json.dumps(ev).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    urllib.request.urlopen(req, timeout=1)
except Exception:
    pass
PY
exit 0
