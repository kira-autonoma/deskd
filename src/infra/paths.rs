//! Infrastructure path helpers — deskd filesystem layout.
//!
//! All paths are relative to `$HOME/.deskd/`.

use std::path::PathBuf;

/// Where agent state files are stored: `~/.deskd/agents/`.
pub fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = PathBuf::from(home).join(".deskd").join("agents");
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

/// Where one-shot reminder JSON files are stored: `~/.deskd/reminders/`.
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
