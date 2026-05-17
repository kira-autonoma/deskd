//! Infrastructure path helpers — deskd filesystem layout.
//!
//! Two flavours of helper coexist:
//!   - `*_for(work_dir)` — resolves under an explicit agent work directory.
//!     Use these in all production code (#467).
//!   - `state_dir() / log_dir() / reminders_dir()` — resolve via `$HOME`. Kept
//!     for tests that isolate via a tempdir-as-`HOME`. Do NOT add new
//!     production call sites — `$HOME` differs between processes that share
//!     a work_dir (root daemon vs. per-user agent subprocess).

use std::path::{Path, PathBuf};

/// Ensure a directory exists and is owned by `unix_user` (if provided).
///
/// When `deskd serve` runs as root but agents run as a different user
/// (via `unix_user` in workspace.yaml), directories created by the serve
/// process would be owned by root. This helper chowns the directory tree
/// to the target user so agents can write to it.
pub fn ensure_dir_owned(dir: &Path, unix_user: Option<&str>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;

    #[cfg(unix)]
    if let Some(user) = unix_user {
        chown_recursive(dir, user);
    }

    let _ = unix_user; // suppress unused warning on non-unix
    Ok(())
}

/// Recursively chown a directory tree to the given unix user.
#[cfg(unix)]
fn chown_recursive(dir: &Path, user: &str) {
    use std::process::Command;
    // Use chown -R to set ownership on the directory and all contents.
    match Command::new("chown")
        .args(["-R", &format!("{user}:{user}"), &dir.to_string_lossy()])
        .status()
    {
        Ok(s) if !s.success() => {
            tracing::warn!(
                dir = %dir.display(),
                user = %user,
                exit_code = s.code().unwrap_or(-1),
                "chown .deskd directory failed (not running as root?)"
            );
        }
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                user = %user,
                error = %e,
                "failed to run chown on .deskd directory"
            );
        }
        _ => {}
    }
}

/// Where agent state files are stored: `~/.deskd/agents/`.
pub fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    state_dir_in(Path::new(&home))
}

/// Like `state_dir`, but reads the base from an explicit home path instead
/// of `$HOME`. Lets tests place state files under a tempdir without mutating
/// process env (which is unsafe under the parallel test harness; see #423
/// CI failure on PR #428).
pub fn state_dir_in(home: &Path) -> PathBuf {
    let dir = home.join(".deskd").join("agents");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Where agent logs are stored: `~/.deskd/logs/`.
pub fn log_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("logs");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Where reminder JSON files are stored for a specific agent:
/// `{work_dir}/.deskd/reminders/`.
///
/// This is the canonical reminder location. Both the MCP `create_reminder` tool
/// (which runs in the agent's subprocess, typically as a non-root `unix_user`)
/// and the firing scanner (which runs in `deskd serve`, often as root) MUST
/// agree on this path. Resolving via `work_dir` instead of `$HOME` is the fix
/// for #467 — see that issue for the bug story.
///
/// Mirrors the `agent_bus_socket(work_dir)` convention.
pub fn reminders_dir_for(work_dir: &Path) -> PathBuf {
    let dir = work_dir.join(".deskd").join("reminders");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// `$HOME`-based reminder dir. Retained ONLY for tests that isolate via a
/// tempdir-as-`HOME`; production code paths resolve via
/// `reminders_dir_for(work_dir)` since #467. New call sites must use the
/// explicit `work_dir` variant — process `$HOME` differs between the MCP
/// subprocess (per-agent) and the `deskd serve` daemon (often root), which
/// is the exact split that caused #467.
pub fn reminders_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("reminders");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Derive the bus socket path for an agent from its work directory.
/// Convention: `{work_dir}/.deskd/bus.sock`
pub fn agent_bus_socket(work_dir: &str) -> String {
    PathBuf::from(work_dir)
        .join(".deskd")
        .join("bus.sock")
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_dir_owned_creates_directory() {
        let base = std::env::temp_dir().join("deskd-test-ensure-dir-owned");
        let _ = std::fs::remove_dir_all(&base);
        let target = base.join("sub").join("nested");
        assert!(!target.exists());

        ensure_dir_owned(&target, None).unwrap();
        assert!(target.is_dir());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ensure_dir_owned_idempotent() {
        let base = std::env::temp_dir().join("deskd-test-ensure-dir-idempotent");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        ensure_dir_owned(&base, None).unwrap();
        assert!(base.is_dir());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reminders_dir_for_resolves_under_work_dir() {
        // The canonical layout is `{work_dir}/.deskd/reminders/`. Confirm the
        // helper returns that exact path AND creates it.
        let base = tempfile::tempdir().unwrap();
        let work_dir = base.path();

        let dir = reminders_dir_for(work_dir);
        assert_eq!(dir, work_dir.join(".deskd").join("reminders"));
        assert!(dir.is_dir(), "reminders_dir_for must create the directory");
    }

    #[test]
    fn reminders_dir_for_two_work_dirs_are_isolated() {
        // The #467 fix relies on two agents with different work_dirs getting
        // ENTIRELY separate reminder dirs — otherwise reminders cross-pollute
        // between agents running on the same host.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();

        let dir_a = reminders_dir_for(a.path());
        let dir_b = reminders_dir_for(b.path());
        assert_ne!(dir_a, dir_b);

        // Writing into one dir must not affect the other.
        std::fs::write(dir_a.join("marker.json"), "{}").unwrap();
        let b_entries: Vec<_> = std::fs::read_dir(&dir_b).unwrap().flatten().collect();
        assert!(b_entries.is_empty(), "agent B's reminder dir leaked from A");
    }
}
