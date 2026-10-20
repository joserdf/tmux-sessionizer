//! Shared formatting for inline review comments (web UI diff notes, hunk, and
//! difit all funnel into the same raw comment block and prompt).

/// A single inline review note: a file path, an optional new-side line, and the
/// body text. `line` is `None` for notes on deleted lines.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ReviewComment {
    pub file: String,
    pub line: Option<i64>,
    pub body: String,
}

/// Format review comments into a raw comment block (`- file:line — body`
/// bullets). Wrapped by [`review_prompt`] before it's forwarded to the agent.
pub fn format_comments(comments: &[ReviewComment]) -> String {
    comments
        .iter()
        .map(|c| {
            let body = c.body.trim();
            match c.line {
                Some(l) => format!("- {}:{l} — {body}", c.file),
                None => format!("- {} — {body}", c.file),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The prompt forwarded to an agent session carrying review comments (web,
/// difit, or hunk); `comments` is the tool's raw comment block.
pub fn review_prompt(comments: &str) -> String {
    format!(
        "The following code review comments were left during review. \
         Please address them:\n\n{comments}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(file: &str, line: Option<i64>, body: &str) -> ReviewComment {
        ReviewComment {
            file: file.to_string(),
            line,
            body: body.to_string(),
        }
    }

    #[test]
    fn format_comments_renders_file_line_and_body() {
        let out = format_comments(&[
            comment("src/f.txt", Some(2), "Please rename this variable"),
            comment("src/g.txt", None, "Removed too much"),
        ]);
        assert!(out.contains("- src/f.txt:2 — Please rename this variable"));
        assert!(out.contains("- src/g.txt — Removed too much"));
    }

    #[test]
    fn format_comments_trims_bodies() {
        let out = format_comments(&[comment("a.txt", Some(1), "  spaced  ")]);
        assert_eq!(out, "- a.txt:1 — spaced");
    }

    #[test]
    fn review_prompt_embeds_the_comments() {
        let prompt = review_prompt("- rename foo\n- drop bar");
        assert!(prompt.contains("- rename foo"));
        assert!(prompt.contains("- drop bar"));
        assert!(prompt.contains("Please address them"));
    }
}
