//! Atomic agent-state document service (#456).
//!
//! Each agent gets a single markdown document (`current_state.md` by default)
//! used as a cross-session source of truth — open PRs, active sub-agents,
//! pending dependencies, etc. The file is intentionally plain markdown: the
//! tool only knows about `##`/`###` headings as textual delimiters.
//!
//! ## Actions
//!
//! - `read` — return the file body (empty string if the file does not exist).
//! - `write` — atomic full-document replace (write-temp + rename).
//! - `append` — append a single bullet under a named section.
//! - `set_section` — atomic per-section replace (creates the section at EOF
//!   when it does not exist).
//!
//! ## Concurrency
//!
//! All non-read actions take an advisory `flock` (via [`fs2`]) on a lock file
//! sibling to the state document for the duration of the read-modify-write
//! cycle. Writes go through a temp file (`<path>.tmp.<pid>.<uuid>`) which is
//! `fsync`-ed and renamed into place so concurrent readers never observe a
//! truncated document.
//!
//! ## Path resolution
//!
//! The path comes from the per-agent `state_file:` field in `deskd.yaml`
//! (either on the top-level `UserConfig` for the calling agent, or on a
//! `SubAgentDef` for a named sub-agent). When unset, the default is
//! `~/.claude/projects/-home-<agent>/memory/current_state.md`.
//!
//! `~/` is expanded against `$HOME`. The configured path is canonicalized
//! (its parent is created on first write) and once a session has resolved a
//! path for an agent it is "pinned" — `..` traversal beyond that resolved
//! base is rejected. We don't accept absolute or `..` overrides at call
//! time; the only input is the configured path.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::UserConfig;

/// Resolve the state-file path for `agent` given the loaded config.
///
/// Looks up the agent name against `cfg.agents[].state_file`; if the agent is
/// the top-level agent owning this deskd.yaml, falls back to `cfg.state_file`.
/// When no override is set, returns the per-agent default
/// `~/.claude/projects/-home-<agent>/memory/current_state.md`.
pub fn resolve_path(agent: &str, cfg: Option<&UserConfig>) -> Result<PathBuf> {
    if agent.is_empty() {
        bail!("agent name must not be empty");
    }
    if agent.contains('/') || agent.contains("..") {
        bail!(
            "invalid agent name '{}': must not contain '/' or '..'",
            agent
        );
    }

    let configured = cfg.and_then(|c| {
        c.agents
            .iter()
            .find(|a| a.name == agent)
            .and_then(|a| a.state_file.clone())
            .or_else(|| c.state_file.clone())
    });

    let raw = configured.unwrap_or_else(|| default_state_path(agent));
    Ok(expand_home(&raw))
}

/// Default state-file path for an agent: `~/.claude/projects/-home-<name>/memory/current_state.md`.
pub fn default_state_path(agent: &str) -> String {
    format!("~/.claude/projects/-home-{}/memory/current_state.md", agent)
}

/// Expand a leading `~/` against `$HOME`.
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Read the current state document. Returns an empty string if the file does
/// not yet exist (friendlier than an error — the caller usually wants to know
/// "is there state to read"; missing == empty).
pub async fn read(agent: &str, cfg: Option<&UserConfig>) -> Result<String> {
    let path = resolve_path(agent, cfg)?;
    tokio::task::spawn_blocking(move || match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("failed to read state file at {}", path.display()))),
    })
    .await?
}

/// Atomically replace the entire state document. Stamps `Last updated:`.
pub async fn write(agent: &str, content: &str, cfg: Option<&UserConfig>) -> Result<()> {
    let path = resolve_path(agent, cfg)?;
    let content = content.to_string();
    tokio::task::spawn_blocking(move || with_lock(&path, |_| atomic_write(&path, &stamp(&content))))
        .await?
}

/// Append a single bullet `line` under the named `## <section>` or `### <section>`.
/// Creates the section at EOF (heading level `##`) if absent.
pub async fn append(
    agent: &str,
    section: &str,
    line: &str,
    cfg: Option<&UserConfig>,
) -> Result<()> {
    let path = resolve_path(agent, cfg)?;
    let section = section.to_string();
    let line = line.to_string();
    tokio::task::spawn_blocking(move || {
        with_lock(&path, |_| {
            let body = read_or_empty(&path)?;
            let body = append_to_section(&body, &section, &line);
            atomic_write(&path, &stamp(&body))
        })
    })
    .await?
}

/// Atomically replace the body of the named section. Creates the section at
/// EOF if absent.
pub async fn set_section(
    agent: &str,
    section: &str,
    content: &str,
    cfg: Option<&UserConfig>,
) -> Result<()> {
    let path = resolve_path(agent, cfg)?;
    let section = section.to_string();
    let content = content.to_string();
    tokio::task::spawn_blocking(move || {
        with_lock(&path, |_| {
            let body = read_or_empty(&path)?;
            let body = replace_section(&body, &section, &content);
            atomic_write(&path, &stamp(&body))
        })
    })
    .await?
}

// ─── Filesystem helpers ─────────────────────────────────────────────────────

/// Read the state file or return empty if it does not exist.
fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("failed to read state file at {}", path.display()))),
    }
}

/// Open / create the sibling lock file and hold an exclusive `flock` for the
/// duration of `f`. The lock file lives next to the state file as
/// `<path>.lock`. Parent directory is created if needed.
fn with_lock<R>(path: &Path, f: impl FnOnce(&File) -> Result<R>) -> Result<R> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create state-file parent dir {}",
                parent.display()
            )
        })?;
    }
    let lock_path = lock_path(path);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("failed to flock lock file {}", lock_path.display()))?;
    let result = f(&lock);
    // Always unlock; ignore errors on unlock since we're tearing down.
    let _ = FileExt::unlock(&lock);
    result
}

fn lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// Write `content` atomically: temp file in the same directory, then rename.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create state-file parent dir {}",
                parent.display()
            )
        })?;
    }
    let tmp = tmp_path(path);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("failed to create temp file {}", tmp.display()))?;
        f.write_all(content.as_bytes())
            .with_context(|| format!("failed to write temp file {}", tmp.display()))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let rand = uuid::Uuid::new_v4().simple().to_string();
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".tmp.{}.{}", pid, rand));
    PathBuf::from(s)
}

// ─── Markdown helpers ───────────────────────────────────────────────────────

/// Stamp the document with `Last updated: <UTC ISO8601>`.
///
/// Rules:
/// - If a `Last updated:` line already exists *anywhere*, replace the first
///   occurrence in place.
/// - Otherwise, insert it after the first heading line (`# …`, `## …`, `### …`),
///   surrounded by a blank line on each side.
/// - If the document has no heading at all, prepend the stamp + a blank line.
pub fn stamp(body: &str) -> String {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let stamp_line = format!("Last updated: {}", now);

    // Replace existing stamp line, if any.
    let mut out_lines: Vec<String> = Vec::with_capacity(body.lines().count() + 4);
    let mut replaced = false;
    for line in body.lines() {
        if !replaced && line.trim_start().starts_with("Last updated:") {
            out_lines.push(stamp_line.clone());
            replaced = true;
        } else {
            out_lines.push(line.to_string());
        }
    }
    if replaced {
        return join_preserving_trailing_newline(&out_lines, body);
    }

    // No existing stamp. Insert after the first heading line, or prepend.
    let mut inserted = false;
    let mut result: Vec<String> = Vec::with_capacity(body.lines().count() + 3);
    for line in body.lines() {
        result.push(line.to_string());
        if !inserted && is_heading(line) {
            result.push(String::new());
            result.push(stamp_line.clone());
            inserted = true;
        }
    }
    if !inserted {
        // Prepend stamp + blank line, then original body.
        let mut prepended = Vec::with_capacity(result.len() + 2);
        prepended.push(stamp_line);
        prepended.push(String::new());
        prepended.extend(result);
        return join_preserving_trailing_newline(&prepended, body);
    }
    join_preserving_trailing_newline(&result, body)
}

fn join_preserving_trailing_newline(lines: &[String], original: &str) -> String {
    let mut s = lines.join("\n");
    if original.ends_with('\n') || !original.is_empty() {
        // Always end with a newline — markdown convention.
        if !s.ends_with('\n') {
            s.push('\n');
        }
    }
    s
}

/// True if `line` is a markdown ATX heading (`#`, `##`, `###`, …).
fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return false;
    }
    // Require a space (or end-of-line) after the leading hashes.
    let after = trimmed.trim_start_matches('#');
    after.is_empty() || after.starts_with(' ')
}

/// Match an `## <section>` or `### <section>` line (exact, case-sensitive).
fn matches_section_heading(line: &str, section: &str) -> bool {
    let trimmed = line.trim_start();
    for prefix in ["## ", "### "] {
        if let Some(rest) = trimmed.strip_prefix(prefix)
            && rest.trim_end() == section
        {
            return true;
        }
    }
    false
}

/// Find the line index of `## <section>` / `### <section>`, or `None`.
fn find_section_index(lines: &[&str], section: &str) -> Option<usize> {
    lines
        .iter()
        .position(|l| matches_section_heading(l, section))
}

/// Find the line index of the next heading after `start`, or `lines.len()`.
fn find_next_heading_index(lines: &[&str], start: usize) -> usize {
    for (offset, line) in lines.iter().enumerate().skip(start) {
        if is_heading(line) {
            return offset;
        }
    }
    lines.len()
}

/// Append `line` as a bullet under `## <section>` / `### <section>`. Creates
/// the section at EOF if absent.
pub fn append_to_section(body: &str, section: &str, line: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let bullet = if line.trim_start().starts_with('-') || line.trim_start().starts_with('*') {
        line.to_string()
    } else {
        format!("- {}", line)
    };

    match find_section_index(&lines, section) {
        Some(heading_idx) => {
            let next = find_next_heading_index(&lines, heading_idx + 1);
            // Walk back from `next` to find the last non-empty line of this section.
            let mut insert_at = next;
            while insert_at > heading_idx + 1
                && lines
                    .get(insert_at - 1)
                    .map(|l| l.trim().is_empty())
                    .unwrap_or(false)
            {
                insert_at -= 1;
            }
            let mut out: Vec<String> = lines[..insert_at].iter().map(|s| s.to_string()).collect();
            out.push(bullet);
            // Re-attach any trailing blank lines + the rest of the file.
            for l in &lines[insert_at..] {
                out.push((*l).to_string());
            }
            join_preserving_trailing_newline(&out, body)
        }
        None => {
            // Append section at EOF.
            let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            // Ensure separation from prior content.
            if !out.is_empty() && !out.last().map(|s| s.is_empty()).unwrap_or(true) {
                out.push(String::new());
            }
            out.push(format!("## {}", section));
            out.push(String::new());
            out.push(bullet);
            join_preserving_trailing_newline(&out, body)
        }
    }
}

/// Replace the body of `## <section>` / `### <section>` with `content`. Creates
/// the section at EOF if absent. `content` is inserted verbatim (caller
/// supplies the formatting).
pub fn replace_section(body: &str, section: &str, content: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    match find_section_index(&lines, section) {
        Some(heading_idx) => {
            let next = find_next_heading_index(&lines, heading_idx + 1);
            let mut out: Vec<String> = lines[..=heading_idx]
                .iter()
                .map(|s| s.to_string())
                .collect();
            out.push(String::new());
            for line in content.lines() {
                out.push(line.to_string());
            }
            // Ensure a blank line between this section and the next heading.
            if next < lines.len() {
                out.push(String::new());
            }
            for l in &lines[next..] {
                out.push((*l).to_string());
            }
            join_preserving_trailing_newline(&out, body)
        }
        None => {
            let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            if !out.is_empty() && !out.last().map(|s| s.is_empty()).unwrap_or(true) {
                out.push(String::new());
            }
            out.push(format!("## {}", section));
            out.push(String::new());
            for line in content.lines() {
                out.push(line.to_string());
            }
            join_preserving_trailing_newline(&out, body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Sync-context HOME setup. Returns the tempdir + a blocking env-lock guard.
    /// SAFETY: gated by the process-wide `env_lock` so concurrent tests
    /// don't race on `setenv`.
    fn setup_home_sync() -> (TempDir, tokio::sync::MutexGuard<'static, ()>) {
        let guard = crate::test_support::env_lock().blocking_lock();
        let dir = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        (dir, guard)
    }

    /// Async-context HOME setup. Held across `.await` safely because the
    /// underlying mutex is `tokio::sync::Mutex`.
    async fn setup_home_async() -> (TempDir, tokio::sync::MutexGuard<'static, ()>) {
        let guard = crate::test_support::env_lock().lock().await;
        let dir = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        (dir, guard)
    }

    #[test]
    fn default_path_uses_agent_name() {
        let (_dir, _g) = setup_home_sync();
        let p = resolve_path("kira", None).unwrap();
        assert!(
            p.to_string_lossy()
                .ends_with(".claude/projects/-home-kira/memory/current_state.md"),
            "got {}",
            p.display()
        );
    }

    #[test]
    fn rejects_dotdot_in_agent_name() {
        assert!(resolve_path("..", None).is_err());
        assert!(resolve_path("foo/../bar", None).is_err());
        assert!(resolve_path("", None).is_err());
    }

    #[test]
    fn stamp_inserts_after_first_heading() {
        let body = "# My state\n\nbody text\n";
        let out = stamp(body);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "# My state");
        assert_eq!(lines[1], "");
        assert!(lines[2].starts_with("Last updated: "));
        assert_eq!(lines[3], "");
        assert_eq!(lines[4], "body text");
    }

    #[test]
    fn stamp_replaces_existing_line() {
        let body = "# My state\n\nLast updated: 2020-01-01T00:00:00Z\n\nbody\n";
        let out = stamp(body);
        // Only one Last-updated line, freshly stamped.
        let count = out
            .lines()
            .filter(|l| l.starts_with("Last updated:"))
            .count();
        assert_eq!(count, 1);
        assert!(!out.contains("2020-01-01"));
    }

    #[test]
    fn stamp_prepends_when_no_heading() {
        let body = "just some text\n";
        let out = stamp(body);
        assert!(out.starts_with("Last updated: "));
        assert!(out.contains("just some text"));
    }

    #[test]
    fn append_creates_section_when_missing() {
        let body = "# Doc\n";
        let out = append_to_section(body, "Open PRs", "kgatilin/deskd#456");
        assert!(out.contains("## Open PRs"));
        assert!(out.contains("- kgatilin/deskd#456"));
    }

    #[test]
    fn append_adds_bullet_within_existing_section() {
        let body = "## A\n\n- one\n\n## B\n\n- x\n";
        let out = append_to_section(body, "A", "two");
        // The 'two' bullet must be under section A, before section B.
        let a_idx = out.find("## A").unwrap();
        let b_idx = out.find("## B").unwrap();
        let two_idx = out.find("- two").unwrap();
        assert!(
            a_idx < two_idx && two_idx < b_idx,
            "wrong order in:\n{}",
            out
        );
        // 'one' bullet still present.
        assert!(out.contains("- one"));
    }

    #[test]
    fn append_accepts_already_bulleted_line() {
        let body = "## A\n\n- x\n";
        let out = append_to_section(body, "A", "- y");
        assert!(out.contains("- x"));
        assert!(out.contains("- y"));
        // No double-dash bullet.
        assert!(!out.contains("- - y"));
    }

    #[test]
    fn replace_section_replaces_only_named_section() {
        let body = "## A\n\nold A body\n\n## B\n\nB body\n";
        let out = replace_section(body, "A", "new A body");
        assert!(out.contains("new A body"));
        assert!(!out.contains("old A body"));
        assert!(out.contains("B body"));
    }

    #[test]
    fn replace_section_creates_when_missing() {
        let body = "# Doc\n";
        let out = replace_section(body, "New", "fresh content");
        assert!(out.contains("## New"));
        assert!(out.contains("fresh content"));
    }

    #[test]
    fn matches_heading_handles_level_3() {
        assert!(matches_section_heading("### Sub", "Sub"));
        assert!(matches_section_heading("## Sub", "Sub"));
        assert!(!matches_section_heading("#### Sub", "Sub")); // level 4 not matched
        assert!(!matches_section_heading("## SubX", "Sub"));
    }

    #[tokio::test]
    async fn read_missing_returns_empty() {
        let (_dir, _g) = setup_home_async().await;
        let out = read("ghost", None).await.unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let (_dir, _g) = setup_home_async().await;
        write("kira", "# Doc\n\nhello\n", None).await.unwrap();
        let got = read("kira", None).await.unwrap();
        assert!(got.contains("hello"));
        assert!(got.contains("Last updated: "));
    }

    #[tokio::test]
    async fn write_is_atomic_under_concurrent_read() {
        let (_dir, _g) = setup_home_async().await;
        // First write — file exists.
        write("kira", "# Doc\n\ninit\n", None).await.unwrap();

        let writer = tokio::spawn(async {
            for i in 0..20 {
                let body = format!("# Doc\n\nbody-{}\n", i);
                write("kira", &body, None).await.unwrap();
            }
        });
        let reader = tokio::spawn(async {
            // Any non-empty read must be a complete document (start with `# Doc`).
            for _ in 0..100 {
                let s = read("kira", None).await.unwrap();
                if !s.is_empty() {
                    assert!(
                        s.starts_with("# Doc"),
                        "read observed a truncated document:\n{}",
                        s
                    );
                    assert!(s.contains("Last updated: "));
                }
                tokio::time::sleep(std::time::Duration::from_micros(50)).await;
            }
        });
        writer.await.unwrap();
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_append_preserves_both_bullets() {
        let (_dir, _g) = setup_home_async().await;
        write("kira", "# Doc\n\n## Notes\n\n", None).await.unwrap();
        let a = tokio::spawn(async {
            append("kira", "Notes", "alpha", None).await.unwrap();
        });
        let b = tokio::spawn(async {
            append("kira", "Notes", "beta", None).await.unwrap();
        });
        let _ = tokio::join!(a, b);
        let got = read("kira", None).await.unwrap();
        assert!(got.contains("- alpha"), "missing alpha in:\n{}", got);
        assert!(got.contains("- beta"), "missing beta in:\n{}", got);
    }

    #[tokio::test]
    async fn set_section_replaces_only_named_section() {
        let (_dir, _g) = setup_home_async().await;
        write("kira", "# Doc\n\n## A\n\n- a1\n\n## B\n\n- b1\n", None)
            .await
            .unwrap();
        set_section("kira", "A", "- a2\n- a3", None).await.unwrap();
        let got = read("kira", None).await.unwrap();
        assert!(got.contains("- a2"));
        assert!(got.contains("- a3"));
        assert!(!got.contains("- a1"));
        assert!(got.contains("- b1"));
    }

    #[tokio::test]
    async fn write_stamps_last_updated() {
        let (_dir, _g) = setup_home_async().await;
        write("kira", "# Doc\n\nbody\n", None).await.unwrap();
        let got = read("kira", None).await.unwrap();
        assert!(got.contains("Last updated: "));
        // A second write replaces the stamp, not duplicates.
        write("kira", "# Doc\n\nbody\n", None).await.unwrap();
        let got = read("kira", None).await.unwrap();
        let count = got
            .lines()
            .filter(|l| l.starts_with("Last updated:"))
            .count();
        assert_eq!(count, 1, "got:\n{}", got);
    }

    #[tokio::test]
    async fn append_creates_unknown_section() {
        let (_dir, _g) = setup_home_async().await;
        write("kira", "# Doc\n", None).await.unwrap();
        append("kira", "Brand New", "first bullet", None)
            .await
            .unwrap();
        let got = read("kira", None).await.unwrap();
        assert!(got.contains("## Brand New"));
        assert!(got.contains("- first bullet"));
    }

    #[tokio::test]
    async fn per_agent_state_file_override() {
        let (dir, _g) = setup_home_async().await;
        let custom = dir.path().join("custom_state.md");
        let mut cfg = UserConfig::default();
        cfg.agents.push(crate::config::SubAgentDef {
            name: "helper".into(),
            model: "haiku".into(),
            system_prompt: String::new(),
            subscribe: vec![],
            publish: None,
            inbox_read: None,
            scope: Default::default(),
            can_message: None,
            work_dir: None,
            env: None,
            session: Default::default(),
            runtime: Default::default(),
            kind: Default::default(),
            context: None,
            compact_threshold: None,
            compact_strategy: None,
            auto_compact_threshold_tokens: None,
            empty_completion_threshold: None,
            empty_completion_restart_min_secs: None,
            state_file: Some(custom.to_string_lossy().to_string()),
            launch_mode: Default::default(),
        });

        write("helper", "# Helper\n\ncontent\n", Some(&cfg))
            .await
            .unwrap();
        // The configured path must have been used.
        let on_disk = std::fs::read_to_string(&custom).unwrap();
        assert!(on_disk.contains("content"));
        assert!(on_disk.contains("Last updated: "));
        // Default path must NOT have been touched.
        let default_path = expand_home(&default_state_path("helper"));
        assert!(!default_path.exists(), "default path unexpectedly created");
    }
}
