//! Bearer token guarding the daemon's MCP streamable-HTTP endpoint (`/mcp`).
//!
//! The token lives in `~/.showrunner/mcp_token`, created on daemon start (or
//! first `showrunner mcp token`) if absent, with 32 random bytes hex-encoded
//! and file permissions 0600. It is never printed, logged or echoed in an
//! error; `showrunner mcp token` only prints the file's path. The token itself
//! stays in the file and in the daemon's memory.
//!
//! The functions take an explicit path so the create/rotate/verify logic can
//! be unit-tested against a temp dir.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The token file's canonical location in the showrunner data dir.
pub fn token_path() -> PathBuf {
    crate::config::base_dir().join("mcp_token")
}

/// Load the token, creating the file (0600) if it is missing. The daemon calls
/// this once at startup so the file exists before the first client connects.
pub fn load_or_create() -> Result<String> {
    load_or_create_at(&token_path())
}

pub(crate) fn load_or_create_at(path: &Path) -> Result<String> {
    if let Some(token) = read_token(path)
        && !token.is_empty()
    {
        return Ok(token);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let token = generate();
    match write_token(path, &token) {
        Ok(()) => Ok(token),
        // Another process won the create race; read its value instead.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            read_token(path).ok_or(e).context("reading existing token file")
        }
        Err(e) => Err(e).with_context(|| format!("writing {}", path.display())),
    }
}

/// Regenerate the token in place (used by `showrunner mcp token --rotate`).
pub fn rotate() -> Result<String> {
    rotate_at(&token_path())
}

pub(crate) fn rotate_at(path: &Path) -> Result<String> {
    let token = generate();
    write_token(path, &token)?;
    Ok(token)
}

/// Whether an `Authorization` header value matches the stored token. Missing,
/// non-`Bearer`, empty or wrong values all fail; correct tokens are compared in
/// constant time.
pub fn verify(header: Option<&str>, stored: &str) -> bool {
    let Some(header) = header else {
        return false;
    };
    let Some(presented) = header.strip_prefix("Bearer ") else {
        return false;
    };
    let presented = presented.trim();
    if presented.is_empty() {
        return false;
    }
    constant_time_eq(presented.as_bytes(), stored.trim().as_bytes())
}

/// Constant-time equality: returns false early on length mismatch but never
/// leaks how many prefix bytes matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 32 random bytes, hex-encoded (64 chars). Sourced from `/dev/urandom`; the
/// daemon and CLI both run on unix (the 0600 permission mode is unix-only too).
fn generate() -> String {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("read /dev/urandom for the mcp token");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_token(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Write the token atomically-ish: `create_new` guards the create-if-missing
/// path (so we never clobber a concurrent writer) and sets 0600 at creation.
/// Rotate truncates the existing file, keeping its inode and permissions; we
/// never chmod after writing.
fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    let exists = path.exists();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).mode(0o600);
    if exists {
        opts.truncate(true);
    } else {
        opts.create_new(true);
    }
    let mut f = opts.open(path)?;
    f.write_all(token.as_bytes())?;
    f.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh, unique temp dir for one test (cleaned up on drop).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "showrunner-mcp-token-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("mcp_token")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn create_if_missing_writes_0600() {
        let dir = TempDir::new();
        let path = dir.file();
        let token = load_or_create_at(&path).unwrap();
        assert_eq!(token.len(), 64);
        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        // Owner read/write only: no group or other bits.
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn create_does_not_overwrite_existing() {
        let dir = TempDir::new();
        let path = dir.file();
        let first = load_or_create_at(&path).unwrap();
        let second = load_or_create_at(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn verify_accepts_correct_and_rejects_wrong_or_missing() {
        let stored = "deadbeefcafe";
        assert!(verify(Some("Bearer deadbeefcafe"), stored));
        // Wrong token, wrong prefix, missing header, empty value.
        assert!(!verify(Some("Bearer wrongtoken"), stored));
        assert!(!verify(Some("Basic deadbeefcafe"), stored));
        assert!(!verify(Some("Bearer "), stored));
        assert!(!verify(None, stored));
        // Whitespace around the presented token is tolerated.
        assert!(verify(Some("Bearer  deadbeefcafe  "), stored));
    }

    #[test]
    fn rotate_changes_the_value() {
        let dir = TempDir::new();
        let path = dir.file();
        let before = load_or_create_at(&path).unwrap();
        let after = rotate_at(&path).unwrap();
        assert_ne!(before, after);
        assert_eq!(after.len(), 64);
        // The file now holds the rotated value.
        assert_eq!(read_token(&path).unwrap(), after);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn constant_time_eq_compares_length_then_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
