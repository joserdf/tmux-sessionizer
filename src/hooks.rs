//! Normalized agent hook events.
//!
//! Each coding agent reports lifecycle transitions through its own hook
//! system — Claude Code `settings.json` hooks, the OpenCode plugin event
//! bus, and Codex `hooks.json`. The payloads share no common shape, so
//! each gets a dedicated parser mapping raw JSON onto the single
//! [`AgentEvent`] enum; the rest of the app consumes only that enum.
//!
//! Parsers never fail loudly: unparseable JSON or an unrecognized event
//! yields `None`, since hook payloads are best-effort side channels.
//! Fields are read individually (not by struct deserialization), so the
//! parsers are robust to field order and extra fields.

/// A normalized lifecycle event reported by an agent hook.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A new agent session began.
    SessionStarted,
    /// A session ended.
    SessionStopped,
    /// The agent is blocked on a permission prompt.
    PermissionRequest,
    /// The agent is blocked waiting for user input.
    NeedsInput,
    /// The agent finished its current turn or task.
    Finished,
    /// The agent session is idle.
    Idle,
    /// The agent reported an error; carries the payload's message.
    Error(String),
}

impl AgentEvent {
    /// The session status this event authoritatively implies, used to remove
    /// the pane-scrape detection latency (e.g. a permission prompt shows on the
    /// next tick instead of after the 3-tick stability window). Returns `None`
    /// when the event carries no status signal.
    ///
    /// Note the turn-completed vs. session-ended distinction: a `Stop`/
    /// `Finished` event means the agent is alive but idle (WaitingForInput),
    /// while `SessionEnd`/`SessionStopped` means the agent has exited.
    pub fn status_hint(&self) -> Option<crate::tmux::SessionStatus> {
        use crate::tmux::SessionStatus;
        match self {
            AgentEvent::PermissionRequest => Some(SessionStatus::WaitingForPermission),
            AgentEvent::NeedsInput | AgentEvent::Idle => Some(SessionStatus::WaitingForInput),
            AgentEvent::Finished => Some(SessionStatus::WaitingForInput),
            AgentEvent::SessionStopped => Some(SessionStatus::Finished),
            AgentEvent::SessionStarted => Some(SessionStatus::Running),
            AgentEvent::Error(_) => None,
        }
    }
}

/// Parse a Claude Code hook payload.
///
/// Claude sends a JSON object on stdin with `hook_event_name` selecting the
/// event. `Notification` events carry their type in `notification_type`
/// (`permission_prompt`, `idle_prompt`, `agent_needs_input`,
/// `agent_completed`, …); some builds omit that field, so we fall back to a
/// `matcher` field and finally to the message text. `PermissionRequest` fires
/// immediately when Claude asks to use a tool — the `Notification`
/// permission variant only fires after ~6 s of inactivity, so wiring
/// `PermissionRequest` is what makes the permission alert timely.
pub fn parse_claude_hook(json: &str) -> Option<AgentEvent> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let event_name = value.get("hook_event_name").and_then(|v| v.as_str())?;
    match event_name {
        "SessionStart" => Some(AgentEvent::SessionStarted),
        "SessionEnd" => Some(AgentEvent::SessionStopped),
        "Stop" => Some(AgentEvent::Finished),
        "PermissionRequest" => Some(AgentEvent::PermissionRequest),
        "Notification" => classify_notification(&value),
        _ => None,
    }
}

/// Map a Claude `Notification` payload to an [`AgentEvent`], trying the
/// authoritative `notification_type`, then a `matcher` field, then a
/// best-effort read of the message text. Permission is checked first so a
/// "stopped"/"done" word in an idle message can't be misread as completion.
fn classify_notification(value: &serde_json::Value) -> Option<AgentEvent> {
    if let Some(t) = value
        .get("notification_type")
        .or_else(|| value.get("matcher"))
        .and_then(|v| v.as_str())
    {
        return match t {
            "permission_prompt" => Some(AgentEvent::PermissionRequest),
            "agent_completed" => Some(AgentEvent::Finished),
            "idle_prompt" | "agent_needs_input" => Some(AgentEvent::NeedsInput),
            _ => None,
        };
    }
    let msg = value
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if msg.is_empty() {
        return None;
    }
    if msg.contains("permission") {
        Some(AgentEvent::PermissionRequest)
    } else if msg.contains("idle")
        || msg.contains("waiting")
        || msg.contains("input")
        || msg.contains("need")
    {
        Some(AgentEvent::NeedsInput)
    } else if msg.contains("complet")
        || msg.contains("done")
        || msg.contains("finished")
        || msg.contains("stopped")
    {
        Some(AgentEvent::Finished)
    } else {
        None
    }
}

/// Parse an OpenCode plugin event-bus payload.
///
/// Canonical shape:
///
/// ```json
/// {"type":"session.error","message":"..."}
/// ```
pub fn parse_opencode_hook(json: &str) -> Option<AgentEvent> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let event_type = value.get("type").and_then(|v| v.as_str())?;
    match event_type {
        "session.start" => Some(AgentEvent::SessionStarted),
        "session.finish" => Some(AgentEvent::Finished),
        "session.idle" => Some(AgentEvent::Idle),
        // Emitted by the plugin's `dispose` hook when the opencode instance
        // shuts down (there is no dedicated "session end" bus event).
        "session.stop" => Some(AgentEvent::SessionStopped),
        "session.error" => {
            let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("error");
            Some(AgentEvent::Error(message.to_owned()))
        }
        "permission.request" => Some(AgentEvent::PermissionRequest),
        _ => None,
    }
}

/// Parse a Codex `hooks.json` payload.
///
/// Canonical shape:
///
/// ```json
/// {"hook":"PermissionRequest","message":"..."}
/// ```
pub fn parse_codex_hook(json: &str) -> Option<AgentEvent> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let hook = value.get("hook").and_then(|v| v.as_str())?;
    match hook {
        "SessionStart" => Some(AgentEvent::SessionStarted),
        "SessionEnd" => Some(AgentEvent::SessionStopped),
        "PermissionRequest" => Some(AgentEvent::PermissionRequest),
        "Stop" => Some(AgentEvent::Finished),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Claude Code ---

    #[test]
    fn claude_session_start() {
        let json = r#"{"hook_event_name":"SessionStart","message":"session started","title":"Session Start","matcher":"startup","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::SessionStarted));
    }

    #[test]
    fn claude_session_end() {
        let json = r#"{"hook_event_name":"SessionEnd","message":"session ended","title":"Session End","matcher":"shutdown","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::SessionStopped));
    }

    #[test]
    fn claude_stop() {
        let json = r#"{"hook_event_name":"Stop","message":"turn complete","title":"Stop","matcher":"turn_complete","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::Finished));
    }

    #[test]
    fn claude_notification_permission_prompt() {
        let json = r#"{"hook_event_name":"Notification","message":"Waiting for permission to run: rm -rf target","title":"Permission Request","matcher":"permission_prompt","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::PermissionRequest));
    }

    #[test]
    fn claude_notification_agent_completed() {
        let json = r#"{"hook_event_name":"Notification","message":"Agent completed","title":"Agent Completed","matcher":"agent_completed","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::Finished));
    }

    #[test]
    fn claude_notification_idle_prompt() {
        let json = r#"{"hook_event_name":"Notification","message":"Agent is idle","title":"Idle","matcher":"idle_prompt","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::NeedsInput));
    }

    #[test]
    fn claude_notification_agent_needs_input() {
        let json = r#"{"hook_event_name":"Notification","message":"Agent needs input","title":"Input Needed","matcher":"agent_needs_input","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::NeedsInput));
    }

    #[test]
    fn claude_notification_unknown_matcher_is_none() {
        let json = r#"{"hook_event_name":"Notification","message":"custom","title":"Custom","matcher":"something_else","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), None);
    }

    #[test]
    fn claude_notification_uses_notification_type() {
        // The authoritative field Claude documents; no matcher present.
        let json = r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Write","notification_type":"permission_prompt","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::PermissionRequest));
        let json = r#"{"hook_event_name":"Notification","message":"idle","notification_type":"idle_prompt","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::NeedsInput));
    }

    #[test]
    fn claude_notification_type_wins_over_stale_matcher() {
        let json = r#"{"hook_event_name":"Notification","message":"x","notification_type":"permission_prompt","matcher":"agent_completed","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::PermissionRequest));
    }

    #[test]
    fn claude_permission_request_event() {
        // Immediate permission signal (fires before the ~6s Notification).
        let json = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::PermissionRequest));
    }

    #[test]
    fn claude_notification_message_inference_permission_first() {
        // No notification_type / matcher (the reported Claude bug): infer from
        // text. "waiting for your permission" has both idle and permission
        // words — permission must win.
        let json = r#"{"hook_event_name":"Notification","message":"Waiting for your permission to proceed","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::PermissionRequest));
    }

    #[test]
    fn claude_notification_message_inference_idle_and_completed() {
        let json = r#"{"hook_event_name":"Notification","message":"Claude is waiting for your input","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::NeedsInput));
        let json = r#"{"hook_event_name":"Notification","message":"Agent finished its task","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::Finished));
        // Unclassifiable message -> dropped.
        let json = r#"{"hook_event_name":"Notification","message":"hello there","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), None);
    }

    #[test]
    fn claude_unknown_event_name_is_none() {
        let json = r#"{"hook_event_name":"UserPromptSubmit","message":"prompt","title":"Prompt","matcher":"","session_id":"abc123"}"#;
        assert_eq!(parse_claude_hook(json), None);
    }

    #[test]
    fn claude_malformed_json_is_none() {
        assert_eq!(parse_claude_hook(r#"{"hook_event_name": "#), None);
    }

    #[test]
    fn claude_robust_to_field_order_and_extra_fields() {
        let json = r#"{"session_id":"abc123","extra":42,"hook_event_name":"SessionEnd","nested":{"a":true}}"#;
        assert_eq!(parse_claude_hook(json), Some(AgentEvent::SessionStopped));
    }

    // --- Status hints (authoritative consumption) ---

    #[test]
    fn status_hint_mapping() {
        use crate::tmux::SessionStatus;
        assert_eq!(
            AgentEvent::PermissionRequest.status_hint(),
            Some(SessionStatus::WaitingForPermission)
        );
        assert_eq!(AgentEvent::NeedsInput.status_hint(), Some(SessionStatus::WaitingForInput));
        // A completed turn leaves the agent alive but idle.
        assert_eq!(AgentEvent::Finished.status_hint(), Some(SessionStatus::WaitingForInput));
        // The session/agent has actually exited.
        assert_eq!(
            AgentEvent::SessionStopped.status_hint(),
            Some(SessionStatus::Finished)
        );
        assert_eq!(AgentEvent::SessionStarted.status_hint(), Some(SessionStatus::Running));
        assert_eq!(AgentEvent::Error("x".into()).status_hint(), None);
    }

    // --- OpenCode ---

    #[test]
    fn opencode_session_start() {
        let json = r#"{"type":"session.start","message":"session started"}"#;
        assert_eq!(parse_opencode_hook(json), Some(AgentEvent::SessionStarted));
    }

    #[test]
    fn opencode_session_finish() {
        let json = r#"{"type":"session.finish","message":"session finished"}"#;
        assert_eq!(parse_opencode_hook(json), Some(AgentEvent::Finished));
    }

    #[test]
    fn opencode_session_idle() {
        let json = r#"{"type":"session.idle","message":"session idle"}"#;
        assert_eq!(parse_opencode_hook(json), Some(AgentEvent::Idle));
    }

    #[test]
    fn opencode_session_error_with_message() {
        let json = r#"{"type":"session.error","message":"model provider failed"}"#;
        assert_eq!(
            parse_opencode_hook(json),
            Some(AgentEvent::Error("model provider failed".to_owned()))
        );
    }

    #[test]
    fn opencode_session_error_without_message() {
        let json = r#"{"type":"session.error"}"#;
        assert_eq!(
            parse_opencode_hook(json),
            Some(AgentEvent::Error("error".to_owned()))
        );
    }

    #[test]
    fn opencode_permission_request() {
        let json = r#"{"type":"permission.request","message":"Allow bash command?"}"#;
        assert_eq!(parse_opencode_hook(json), Some(AgentEvent::PermissionRequest));
    }

    #[test]
    fn opencode_session_stop() {
        // The plugin's dispose hook emits this when the instance shuts down.
        let json = r#"{"type":"session.stop","cwd":"/w"}"#;
        assert_eq!(parse_opencode_hook(json), Some(AgentEvent::SessionStopped));
    }

    #[test]
    fn opencode_unknown_type_is_none() {
        let json = r#"{"type":"message.part.updated","message":"part updated"}"#;
        assert_eq!(parse_opencode_hook(json), None);
    }

    #[test]
    fn opencode_malformed_json_is_none() {
        assert_eq!(parse_opencode_hook(r#"{"type": "session."#), None);
    }

    // --- Codex ---

    #[test]
    fn codex_session_start() {
        let json = r#"{"hook":"SessionStart","message":"session started"}"#;
        assert_eq!(parse_codex_hook(json), Some(AgentEvent::SessionStarted));
    }

    #[test]
    fn codex_session_end() {
        let json = r#"{"hook":"SessionEnd","message":"session ended"}"#;
        assert_eq!(parse_codex_hook(json), Some(AgentEvent::SessionStopped));
    }

    #[test]
    fn codex_permission_request() {
        let json = r#"{"hook":"PermissionRequest","message":"Allow running npm install?"}"#;
        assert_eq!(parse_codex_hook(json), Some(AgentEvent::PermissionRequest));
    }

    #[test]
    fn codex_stop() {
        let json = r#"{"hook":"Stop","message":"turn complete"}"#;
        assert_eq!(parse_codex_hook(json), Some(AgentEvent::Finished));
    }

    #[test]
    fn codex_unknown_hook_is_none() {
        let json = r#"{"hook":"TurnComplete","message":"turn complete"}"#;
        assert_eq!(parse_codex_hook(json), None);
    }

    #[test]
    fn codex_malformed_json_is_none() {
        assert_eq!(parse_codex_hook(r#"this is not json"#), None);
    }
}
