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

/// Parse a Claude Code `settings.json` hook payload.
///
/// Canonical shape:
///
/// ```json
/// {"hook_event_name":"Notification","message":"...","title":"...","matcher":"permission_prompt","session_id":"..."}
/// ```
///
/// `hook_event_name` selects the event; for `Notification` the `matcher`
/// field disambiguates between permission, completion, and input-needed
/// states.
pub fn parse_claude_hook(json: &str) -> Option<AgentEvent> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let event_name = value.get("hook_event_name").and_then(|v| v.as_str())?;
    match event_name {
        "SessionStart" => Some(AgentEvent::SessionStarted),
        "SessionEnd" => Some(AgentEvent::SessionStopped),
        "Stop" => Some(AgentEvent::Finished),
        "Notification" => {
            let matcher = value.get("matcher").and_then(|v| v.as_str())?;
            match matcher {
                "permission_prompt" => Some(AgentEvent::PermissionRequest),
                "agent_completed" => Some(AgentEvent::Finished),
                "idle_prompt" | "agent_needs_input" => Some(AgentEvent::NeedsInput),
                _ => None,
            }
        }
        _ => None,
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
