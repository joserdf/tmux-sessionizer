//! Manual session categories, stored in the tmux session option `@category`
//! so plain tmux tools (formats, scripts) see them too.

use std::process::Command;

use anyhow::{Result, bail};

/// Longest category name, in characters.
pub const MAX_LEN: usize = 32;

/// A valid category: 1..=[`MAX_LEN`] letters, digits, `-`, `_` or `.`.
pub fn validate(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_LEN {
        bail!("category must be 1-{MAX_LEN} characters");
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("category may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(name)
}

/// `tmux` arguments setting (or with `None`, clearing) a session's category.
/// `=name:` matches the session name exactly, never by prefix.
fn set_args(session: &str, category: Option<&str>) -> Vec<String> {
    let target = format!("={session}:");
    let args: Vec<&str> = match category {
        Some(c) => vec!["set-option", "-t", &target, "@category", c],
        None => vec!["set-option", "-u", "-t", &target, "@category"],
    };
    args.into_iter().map(String::from).collect()
}

/// Set a session's category; `None` clears it.
pub fn set(session: &str, category: Option<&str>) -> Result<()> {
    let category = category.map(validate).transpose()?;
    let out = Command::new("tmux")
        .args(set_args(session, category))
        .output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Parse `list-sessions -F "#{session_name}\t#{@category}"` output.
fn parse_list(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(s, c)| (s.to_string(), c.to_string()))
        .collect()
}

/// Every session with its category (empty when uncategorized).
pub fn list() -> Vec<(String, String)> {
    Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}\t#{@category}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_list(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// `showrunner category set <session> <name> | clear <session> | list [--json]`.
pub fn cmd_category(args: &[String]) -> Result<()> {
    let arg = |i: usize| args.get(i).map(String::as_str);
    match (arg(0), arg(1), arg(2)) {
        (Some("set"), Some(session), Some(name)) => set(session, Some(name)),
        (Some("clear"), Some(session), None) => set(session, None),
        (Some("list"), json, None) if json.is_none_or(|j| j == "--json") => {
            let sessions = list();
            if json.is_some() {
                let rows: Vec<_> = sessions
                    .iter()
                    .map(|(s, c)| serde_json::json!({ "session": s, "category": c }))
                    .collect();
                println!("{}", serde_json::Value::Array(rows));
            } else {
                for (session, category) in sessions {
                    println!("{session}\t{category}");
                }
            }
            Ok(())
        }
        _ => bail!(
            "usage: showrunner category set <session> <name> | clear <session> | list [--json]"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_table() {
        for ok in ["work", "pesquisa", "ação", "a-b_c.d", "  work  "] {
            assert!(validate(ok).is_ok(), "{ok:?}");
        }
        assert_eq!(validate("  work ").unwrap(), "work");
        let long = "x".repeat(MAX_LEN + 1);
        for bad in ["", "   ", "two words", "a;", "a\tb", "#{x}", long.as_str()] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn set_args_target_the_exact_session() {
        assert_eq!(
            set_args("gates", Some("work")).join(" "),
            "set-option -t =gates: @category work"
        );
        assert_eq!(
            set_args("gates", None).join(" "),
            "set-option -u -t =gates: @category"
        );
    }

    #[test]
    fn parse_list_keeps_empty_categories() {
        assert_eq!(
            parse_list("main\t\ngates\twork\n"),
            vec![
                ("main".to_string(), String::new()),
                ("gates".to_string(), "work".to_string())
            ]
        );
    }
}
