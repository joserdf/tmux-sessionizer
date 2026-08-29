//! Project discovery: find git repositories on this machine.
//!
//! The core functions perform only filesystem reads and string parsing, so
//! they are pure and testable. Process and environment I/O is isolated in the
//! thin wrappers [`from_ghq`] and [`from_zoxide`].

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Returns `true` if `path` is a git repository root.
///
/// A `.git` entry directly inside `path` marks it: a directory for regular
/// clones, a file for worktrees and submodules.
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Collects every git repository root found in `dir` and in its descendants
/// up to `max_depth` levels below `dir` (`dir` itself is included).
///
/// The result is sorted and deduplicated. Unreadable directories are skipped.
pub fn git_repos_in(dir: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    collect_repos(dir, 0, max_depth, &mut roots);
    roots.into_iter().collect()
}

/// Returns git repositories under a ghq root at depth <= 3
/// (ghq layout is `$root/<host>/<repo>`, or `$root/<host>/<org>/<repo>`).
///
/// The result is sorted and deduplicated. The root itself is never included,
/// even if it happens to be a repository.
pub fn ghq_repos(root: &Path) -> Vec<PathBuf> {
    git_repos_in(root, 3)
        .into_iter()
        .filter(|repo| repo != root)
        .collect()
}

/// Returns the ghq-managed repositories, if `GHQ_ROOT` is set and non-empty.
pub fn from_ghq() -> Vec<PathBuf> {
    match std::env::var("GHQ_ROOT") {
        Ok(root) if !root.is_empty() => ghq_repos(Path::new(&root)),
        _ => Vec::new(),
    }
}

/// Parses `zoxide query -l` output into paths.
///
/// Keeps only non-empty lines that start with `/`, trimmed. Does not check
/// whether the paths exist.
pub fn parse_zoxide_output(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

/// Returns frequently visited paths recorded by zoxide.
///
/// Best effort: if zoxide is missing or fails to start, returns an empty
/// vector. Only paths that still exist are kept, sorted and deduplicated.
pub fn from_zoxide() -> Vec<PathBuf> {
    let output = match std::process::Command::new("zoxide")
        .args(["query", "--global", "-l"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = BTreeSet::new();
    for path in parse_zoxide_output(&stdout) {
        if path.exists() {
            paths.insert(path);
        }
    }
    paths.into_iter().collect()
}

/// Returns the user's home directory, if it can be determined.
pub fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn collect_repos(dir: &Path, depth: usize, max_depth: usize, roots: &mut BTreeSet<PathBuf>) {
    if is_git_repo(dir) {
        roots.insert(dir.to_path_buf());
    }
    if depth >= max_depth {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_repos(&path, depth + 1, max_depth, roots);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp directory under [`std::env::temp_dir`], removed on drop
    /// (best effort, including when a test panics).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "showrunner-discover-test-{}-{}-{tag}",
                std::process::id(),
                nanos
            ));
            fs::create_dir_all(&path).expect("failed to create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_git_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".git")).expect("failed to create .git dir");
    }

    #[test]
    fn is_git_repo_detects_dot_git_dir_and_file() {
        let base = TempDir::new("is_git_repo");

        let fresh = base.path().join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        assert!(!is_git_repo(&fresh));

        let with_dot_git_dir = base.path().join("with_dir");
        make_git_repo(&with_dot_git_dir);
        assert!(is_git_repo(&with_dot_git_dir));

        let with_dot_git_file = base.path().join("with_file");
        fs::create_dir_all(&with_dot_git_file).unwrap();
        fs::write(with_dot_git_file.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(is_git_repo(&with_dot_git_file));
    }

    #[test]
    fn git_repos_in_collects_sorted_unique_roots_within_depth() {
        let base = TempDir::new("git_repos_in");
        make_git_repo(&base.path().join("a"));
        make_git_repo(&base.path().join("b").join("c"));
        fs::create_dir_all(base.path().join("plain")).unwrap();

        assert_eq!(
            git_repos_in(base.path(), 2),
            vec![base.path().join("a"), base.path().join("b").join("c")]
        );
        assert_eq!(git_repos_in(base.path(), 1), vec![base.path().join("a")]);
    }

    #[test]
    fn ghq_repos_finds_repos_at_depth_up_to_three() {
        let root = TempDir::new("ghq_repos");
        make_git_repo(&root.path().join("host1").join("repo1"));
        make_git_repo(&root.path().join("github.com").join("org").join("repo3"));
        fs::create_dir_all(root.path().join("host2")).unwrap();
        make_git_repo(&root.path().join("host4").join("a").join("b").join("too_deep"));

        assert_eq!(
            ghq_repos(root.path()),
            vec![
                root.path().join("github.com").join("org").join("repo3"),
                root.path().join("host1").join("repo1"),
            ]
        );
    }

    #[test]
    fn parse_zoxide_output_keeps_only_trimmed_absolute_paths() {
        let input = "/abs/one\nrelative/path\n\n   \n  /abs/two  \n~/home\n/abs/three\n";

        assert_eq!(
            parse_zoxide_output(input),
            vec![
                PathBuf::from("/abs/one"),
                PathBuf::from("/abs/two"),
                PathBuf::from("/abs/three"),
            ]
        );
    }
}
