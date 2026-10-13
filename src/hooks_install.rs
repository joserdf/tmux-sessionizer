//! `showrunner hooks install|uninstall|status`: wire each agent's lifecycle
//! hooks globally, so agents in any tmux pane (not only showrunner sessions)
//! report to the daemon.
//!
//! Install and uninstall compute a list of file edits from the current file
//! contents (pure, tested), print it, and apply it only after confirmation.
//! An empty plan means there is nothing to do. Our entries are identified by
//! the installed script path, so the user's own hooks are never touched.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::tmux::{
    CODEX_NOTIFY, OPENCODE_PLUGIN, PLUGIN_HOOKS_JSON, PLUGIN_HOOKS_POST_EVENT,
    codex_notify_config_line, config_has_top_level_notify,
};

/// Where each piece lives. Built from `$HOME` in production, a temp dir in tests.
struct Paths {
    scripts: PathBuf,
    claude_dir: PathBuf,
    opencode_dir: PathBuf,
    codex_dir: PathBuf,
}

impl Paths {
    fn from_home() -> Result<Self> {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(Self {
            scripts: crate::config::base_dir().join("hooks"),
            claude_dir: home.join(".claude"),
            opencode_dir: home.join(".config").join("opencode"),
            codex_dir: home.join(".codex"),
        })
    }

    fn post_event(&self) -> PathBuf {
        self.scripts.join("post-event.sh")
    }
    fn codex_notify(&self) -> PathBuf {
        self.scripts.join("codex-notify.sh")
    }
    fn claude_settings(&self) -> PathBuf {
        self.claude_dir.join("settings.json")
    }
    fn opencode_plugin(&self) -> PathBuf {
        self.opencode_dir.join("plugins").join("showrunner.ts")
    }
    fn codex_config(&self) -> PathBuf {
        self.codex_dir.join("config.toml")
    }
    /// The command our Claude hook entries run.
    fn claude_command(&self) -> String {
        format!("bash \"{}\"", self.post_event().display())
    }
}

/// One planned file change; `after: None` removes the file.
#[derive(Debug)]
struct Edit {
    path: PathBuf,
    after: Option<String>,
    executable: bool,
    /// A user-owned file: back it up before writing.
    backup: bool,
    what: String,
}

// ---- Claude: ~/.claude/settings.json -------------------------------------

fn parse_settings(settings: &str) -> Result<Value> {
    if settings.trim().is_empty() {
        return Ok(json!({}));
    }
    let v: Value = serde_json::from_str(settings).context("settings.json is not valid JSON")?;
    if !v.is_object() {
        bail!("settings.json is not a JSON object");
    }
    Ok(v)
}

fn to_pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("a Value always serializes");
    s.push('\n');
    s
}

/// A hook group (`{matcher, hooks: [...]}`) that runs `command`.
fn group_runs(group: &Value, command: &str) -> bool {
    group["hooks"]
        .as_array()
        .is_some_and(|hs| hs.iter().any(|h| h["command"] == command))
}

/// The events the plugin's `hooks.json` subscribes to, with its group shape.
fn hook_template() -> Vec<(String, Value)> {
    let t: Value = serde_json::from_str(PLUGIN_HOOKS_JSON).expect("embedded hooks.json is valid");
    t["hooks"]
        .as_object()
        .expect("hooks.json has a hooks object")
        .iter()
        .map(|(event, groups)| (event.clone(), groups[0].clone()))
        .collect()
}

/// `settings` with a group running `command` on every template event that
/// lacks one. Everything else is kept as-is (key order included).
fn claude_install(settings: &str, command: &str) -> Result<String> {
    let mut root = parse_settings(settings)?;
    let hooks = root
        .as_object_mut()
        .expect("checked by parse_settings")
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("settings.json: `hooks` is not an object")?;
    for (event, mut group) in hook_template() {
        let list = hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("settings.json: hooks.{event} is not an array"))?;
        if !list.iter().any(|g| group_runs(g, command)) {
            group["hooks"][0]["command"] = json!(command);
            list.push(group);
        }
    }
    Ok(to_pretty(&root))
}

/// `settings` without the groups running `command`. Events (and `hooks`)
/// left empty by the removal are dropped.
fn claude_uninstall(settings: &str, command: &str) -> Result<String> {
    let mut root = parse_settings(settings)?;
    let obj = root.as_object_mut().expect("checked by parse_settings");
    if let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) {
        let mut removed = false;
        hooks.retain(|_, list| {
            let Some(arr) = list.as_array_mut() else {
                return true;
            };
            let before = arr.len();
            arr.retain(|g| !group_runs(g, command));
            removed |= arr.len() != before;
            arr.len() == before || !arr.is_empty()
        });
        if removed && hooks.is_empty() {
            obj.remove("hooks");
        }
    }
    Ok(to_pretty(&root))
}

/// True when every template event has a group running `command`.
fn claude_installed(settings: &str, command: &str) -> bool {
    let Ok(root) = parse_settings(settings) else {
        return false;
    };
    hook_template().iter().all(|(event, _)| {
        root["hooks"][event]
            .as_array()
            .is_some_and(|l| l.iter().any(|g| group_runs(g, command)))
    })
}

// ---- Codex: ~/.codex/config.toml -----------------------------------------

#[derive(Debug, PartialEq)]
enum CodexState {
    Installed,
    Missing,
    /// The user has their own top-level `notify`; we never replace it.
    Conflict,
}

fn codex_state(config: &str, script: &str) -> CodexState {
    let ours = codex_notify_config_line(script);
    if config
        .lines()
        .take_while(|l| !l.trim_start().starts_with('['))
        .any(|l| l == ours.trim_end())
    {
        CodexState::Installed
    } else if config_has_top_level_notify(config) {
        CodexState::Conflict
    } else {
        CodexState::Missing
    }
}

// ---- Plans ---------------------------------------------------------------

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Push a write of `content` to `path` unless the file already holds it.
fn write_if_changed(edits: &mut Vec<Edit>, path: PathBuf, content: String, what: &str) {
    if fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
        return;
    }
    let executable = path.extension().is_some_and(|e| e == "sh");
    edits.push(Edit {
        path,
        after: Some(content),
        executable,
        backup: false,
        what: what.to_string(),
    });
}

/// Edits for `install`, plus warnings for agents that were skipped.
fn plan_install(p: &Paths) -> Result<(Vec<Edit>, Vec<String>)> {
    let mut edits = Vec::new();
    let mut warnings = Vec::new();

    if p.claude_dir.is_dir() {
        write_if_changed(
            &mut edits,
            p.post_event(),
            PLUGIN_HOOKS_POST_EVENT.to_string(),
            "hook script for Claude Code",
        );
        let path = p.claude_settings();
        let before = read(&path);
        let after = claude_install(&before, &p.claude_command())?;
        if after != before && !claude_installed(&before, &p.claude_command()) {
            let events: Vec<String> = hook_template().into_iter().map(|(e, _)| e).collect();
            edits.push(Edit {
                path,
                after: Some(after),
                executable: false,
                backup: true,
                what: format!("add hooks: {}", events.join(", ")),
            });
        }
    } else {
        warnings.push(format!(
            "claude: {} not found, skipped",
            p.claude_dir.display()
        ));
    }

    if p.opencode_dir.is_dir() {
        write_if_changed(
            &mut edits,
            p.opencode_plugin(),
            OPENCODE_PLUGIN.to_string(),
            "global OpenCode plugin",
        );
    } else {
        warnings.push(format!(
            "opencode: {} not found, skipped",
            p.opencode_dir.display()
        ));
    }

    if p.codex_dir.is_dir() {
        let script = p.codex_notify().to_string_lossy().into_owned();
        write_if_changed(
            &mut edits,
            p.codex_notify(),
            CODEX_NOTIFY.to_string(),
            "notify script for Codex",
        );
        let path = p.codex_config();
        let config = read(&path);
        match codex_state(&config, &script) {
            CodexState::Installed => {}
            CodexState::Missing => edits.push(Edit {
                path,
                after: Some(codex_notify_config_line(&script) + &config),
                executable: false,
                backup: true,
                what: "add top-level `notify`".to_string(),
            }),
            CodexState::Conflict => warnings.push(format!(
                "codex: {} already sets `notify`; left unchanged. To use showrunner's, \
                 point it at {script}",
                path.display()
            )),
        }
    } else {
        warnings.push(format!(
            "codex: {} not found, skipped",
            p.codex_dir.display()
        ));
    }

    Ok((edits, warnings))
}

/// Edits for `uninstall`. The Codex notify script stays: showrunner's own
/// Codex sessions use it (see `tmux::ensure_codex_notify`).
fn plan_uninstall(p: &Paths) -> Result<Vec<Edit>> {
    let mut edits = Vec::new();

    let path = p.claude_settings();
    let before = read(&path);
    if !before.is_empty() {
        let after = claude_uninstall(&before, &p.claude_command())?;
        if parse_settings(&after)? != parse_settings(&before)? {
            edits.push(Edit {
                path,
                after: Some(after),
                executable: false,
                backup: true,
                what: "remove showrunner hooks".to_string(),
            });
        }
    }
    for (path, what) in [
        (p.post_event(), "hook script for Claude Code"),
        (p.opencode_plugin(), "global OpenCode plugin"),
    ] {
        if path.exists() {
            edits.push(Edit {
                path,
                after: None,
                executable: false,
                backup: false,
                what: what.to_string(),
            });
        }
    }

    let script = p.codex_notify().to_string_lossy().into_owned();
    let path = p.codex_config();
    let config = read(&path);
    if codex_state(&config, &script) == CodexState::Installed {
        edits.push(Edit {
            path,
            after: Some(config.replacen(&codex_notify_config_line(&script), "", 1)),
            executable: false,
            backup: true,
            what: "remove top-level `notify`".to_string(),
        });
    }

    Ok(edits)
}

fn status_lines(p: &Paths) -> Vec<String> {
    let claude = if !p.claude_dir.is_dir() {
        "not found"
    } else if claude_installed(&read(&p.claude_settings()), &p.claude_command())
        && p.post_event().exists()
    {
        "installed"
    } else {
        "not installed"
    };
    let opencode = if !p.opencode_dir.is_dir() {
        "not found"
    } else if p.opencode_plugin().exists() {
        "installed"
    } else {
        "not installed"
    };
    let codex = if !p.codex_dir.is_dir() {
        "not found"
    } else {
        let script = p.codex_notify().to_string_lossy().into_owned();
        match codex_state(&read(&p.codex_config()), &script) {
            CodexState::Installed => "installed",
            CodexState::Missing => "not installed",
            CodexState::Conflict => "conflict (own `notify` set)",
        }
    };
    vec![
        format!("claude    {claude}"),
        format!("opencode  {opencode}"),
        format!("codex     {codex}"),
    ]
}

// ---- Apply ---------------------------------------------------------------

/// Write via a temp file + rename, so a crash never leaves a half-written
/// settings file behind.
fn write_atomic(path: &Path, content: &str, executable: bool) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("showrunner-tmp");
    fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable {
            0o755
        } else {
            // Keep the existing file's mode (settings may be 0600).
            fs::metadata(path).map_or(0o644, |m| m.permissions().mode())
        };
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn apply(edits: &[Edit]) -> Result<()> {
    for e in edits {
        if e.backup && e.path.exists() {
            let mut bak = e.path.clone().into_os_string();
            bak.push(".showrunner.bak");
            fs::copy(&e.path, &bak).with_context(|| format!("backing up {}", e.path.display()))?;
        }
        match &e.after {
            Some(content) => write_atomic(&e.path, content, e.executable)?,
            None => fs::remove_file(&e.path)
                .with_context(|| format!("removing {}", e.path.display()))?,
        }
    }
    Ok(())
}

fn describe(e: &Edit) -> String {
    let verb = match (&e.after, e.path.exists()) {
        (None, _) => "remove",
        (Some(_), true) => "update",
        (Some(_), false) => "create",
    };
    let bak = if e.backup && e.path.exists() {
        " (backup: .showrunner.bak)"
    } else {
        ""
    };
    format!("  {verb} {}: {}{bak}", e.path.display(), e.what)
}

fn confirm() -> Result<bool> {
    print!("Apply? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn run_plan(edits: Vec<Edit>, warnings: &[String], yes: bool) -> Result<()> {
    for w in warnings {
        eprintln!("warning: {w}");
    }
    if edits.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }
    for e in &edits {
        println!("{}", describe(e));
    }
    if !yes && !confirm()? {
        println!("Aborted.");
        return Ok(());
    }
    apply(&edits)?;
    println!("Done.");
    Ok(())
}

pub fn cmd_hooks(args: &[String]) -> Result<()> {
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let p = Paths::from_home()?;
    match args.first().map(String::as_str) {
        Some("install") => {
            let (edits, warnings) = plan_install(&p)?;
            run_plan(edits, &warnings, yes)
        }
        Some("uninstall") => run_plan(plan_uninstall(&p)?, &[], yes),
        Some("status") => {
            for line in status_lines(&p) {
                println!("{line}");
            }
            Ok(())
        }
        _ => bail!("usage: showrunner hooks install|uninstall|status [--yes]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "bash \"/h/.showrunner/hooks/post-event.sh\"";

    #[test]
    fn claude_install_adds_every_event_and_keeps_user_content() {
        let before = r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]},"env":{}}"#;
        let after = claude_install(before, CMD).unwrap();
        let v: Value = serde_json::from_str(&after).unwrap();
        // Key order is preserved (serde_json `preserve_order`).
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "hooks", "env"]);
        // The user's own Stop hook is kept, ours appended.
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "mine");
        assert_eq!(v["hooks"]["Stop"][1]["hooks"][0]["command"], CMD);
        assert!(claude_installed(&after, CMD));
        assert!(!claude_installed(before, CMD));
    }

    #[test]
    fn claude_install_is_idempotent() {
        let once = claude_install("", CMD).unwrap();
        assert_eq!(claude_install(&once, CMD).unwrap(), once);
    }

    #[test]
    fn claude_uninstall_removes_only_ours() {
        let before = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#;
        let installed = claude_install(before, CMD).unwrap();
        let removed: Value =
            serde_json::from_str(&claude_uninstall(&installed, CMD).unwrap()).unwrap();
        assert_eq!(removed, serde_json::from_str::<Value>(before).unwrap());
        // Only ours present: `hooks` itself goes away.
        let only_ours = claude_install(r#"{"model":"x"}"#, CMD).unwrap();
        let v: Value = serde_json::from_str(&claude_uninstall(&only_ours, CMD).unwrap()).unwrap();
        assert_eq!(v, json!({"model": "x"}));
    }

    #[test]
    fn claude_rejects_non_object_settings() {
        assert!(claude_install("[1]", CMD).is_err());
        assert!(claude_install("{not json", CMD).is_err());
    }

    #[test]
    fn codex_state_detects_ours_theirs_and_none() {
        let ours = codex_notify_config_line("/s/codex-notify.sh");
        assert_eq!(
            codex_state(&format!("{ours}model = \"m\"\n"), "/s/codex-notify.sh"),
            CodexState::Installed
        );
        assert_eq!(
            codex_state("notify = [\"other\"]\n", "/s/codex-notify.sh"),
            CodexState::Conflict
        );
        assert_eq!(
            codex_state("[tui]\nnotify = [\"x\"]\n", "/s/codex-notify.sh"),
            CodexState::Missing
        );
    }

    /// Temp `$HOME`-like tree, removed on drop.
    struct Home(PathBuf);

    impl Home {
        fn new(tag: &str, agents: &[&str]) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let root = std::env::temp_dir().join(format!(
                "showrunner-hooks-test-{}-{nanos}-{tag}",
                std::process::id()
            ));
            for a in agents {
                fs::create_dir_all(root.join(a)).unwrap();
            }
            Self(root)
        }

        fn paths(&self) -> Paths {
            Paths {
                scripts: self.0.join(".showrunner/hooks"),
                claude_dir: self.0.join(".claude"),
                opencode_dir: self.0.join(".config/opencode"),
                codex_dir: self.0.join(".codex"),
            }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn install_then_uninstall_round_trips() {
        let home = Home::new("roundtrip", &[".claude", ".config/opencode", ".codex"]);
        let p = home.paths();
        fs::write(p.claude_settings(), "{\"model\": \"opus\"}\n").unwrap();
        fs::write(p.codex_config(), "model = \"m\"\n").unwrap();

        let (edits, warnings) = plan_install(&p).unwrap();
        assert!(warnings.is_empty());
        apply(&edits).unwrap();
        assert!(
            status_lines(&p)
                .iter()
                .all(|l| l.ends_with("installed") && !l.contains("not"))
        );
        assert!(
            fs::read_to_string(p.claude_settings().with_extension("json.showrunner.bak")).is_ok()
        );
        // Second install has nothing left to do.
        assert!(plan_install(&p).unwrap().0.is_empty());

        apply(&plan_uninstall(&p).unwrap()).unwrap();
        let settings: Value = serde_json::from_str(&read(&p.claude_settings())).unwrap();
        assert_eq!(settings, json!({"model": "opus"}));
        assert_eq!(read(&p.codex_config()), "model = \"m\"\n");
        assert!(!p.opencode_plugin().exists());
        assert!(plan_uninstall(&p).unwrap().is_empty());
    }

    #[test]
    fn install_skips_missing_agents_and_codex_conflict() {
        let home = Home::new("skip", &[".codex"]);
        let p = home.paths();
        fs::write(p.codex_config(), "notify = [\"mine\"]\n").unwrap();
        let (edits, warnings) = plan_install(&p).unwrap();
        assert_eq!(warnings.len(), 3);
        assert!(warnings[2].contains("already sets `notify`"));
        // Only the codex script itself; the user's config is untouched.
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, p.codex_notify());
    }
}
