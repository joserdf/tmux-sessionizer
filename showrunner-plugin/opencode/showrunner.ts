// showrunner lifecycle hook for OpenCode.
//
// OpenCode auto-loads any file in <project>/.opencode/plugins/ (no config edit
// needed), so showrunner drops this file there when it creates a session. It
// forwards session lifecycle events to the local showrunner daemon so the TUI
// shows status without pane-scrape latency.
//
// Best-effort: every failure is swallowed, so a missing daemon (or a wrong
// port) never affects opencode. The daemon normalizes the payload at the
// boundary (see hooks::parse_opencode_hook).
//
// A `default` export that is an async function is the legacy plugin shape
// OpenCode accepts: it is called as `server(input, options)` and must return a
// hooks object. `input.directory` is the cwd of the opencode instance, which is
// the correlation key the daemon uses to map the event back to a tmux session.
export default async ({ directory }) => {
  const env = (globalThis.process && globalThis.process.env) || {}
  const port = env.SESSIONIZER_PORT || "7878"

  const post = (type, extra) => {
    const body = Object.assign({ agent: "opencode", type: type, cwd: directory }, extra || {})
    try {
      fetch("http://127.0.0.1:" + port + "/api/hook", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      }).catch(function () {})
    } catch (e) {
      // Never let a hook failure touch opencode.
    }
  }

  return {
    event: async ({ event }) => {
      const props = event.properties || {}
      switch (event.type) {
        // Session began.
        case "session.created":
          post("session.start")
          break
        // The agent finished a turn and is idle (canonical signal).
        case "session.status":
          if (props.status && props.status.type === "idle") post("session.finish")
          break
        // Legacy idle event (still emitted alongside session.status).
        case "session.idle":
          post("session.finish")
          break
        case "session.error":
          post("session.error", { message: (props.error && props.error.message) || "error" })
          break
        // Only fires when a permission policy is set to ask; showrunner launches
        // opencode with --auto (allow), so this is normally silent.
        case "permission.asked":
          post("permission.request")
          break
      }
    },
    // There is no "session end" event; the instance shutting down (or being
    // disposed) is our end signal -> the daemon marks the session finished.
    dispose: async () => {
      post("session.stop")
    },
  }
}
