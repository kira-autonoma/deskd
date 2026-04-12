//! `deskd tui` — launch the Ink/React terminal UI.

use std::process::Command;

use anyhow::{Context, bail};

use crate::config;

/// Resolve the bus socket path from explicit arg, env, or serve state.
fn resolve_socket(explicit: Option<String>) -> anyhow::Result<String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Ok(env) = std::env::var("DESKD_BUS_SOCKET") {
        return Ok(env);
    }
    if let Some(state) = config::ServeState::load()
        && let Some(agent_cfg) = state.find_agent_config()
    {
        let sock = std::path::Path::new(&agent_cfg.work_dir)
            .join(".deskd")
            .join("bus.sock");
        if sock.exists() {
            return Ok(sock.to_string_lossy().to_string());
        }
    }
    // Fall back to default
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(std::path::PathBuf::from(home)
        .join(".deskd")
        .join("bus.sock")
        .to_string_lossy()
        .to_string())
}

/// Find the tui/ directory relative to the deskd binary or CWD.
fn find_tui_dir() -> anyhow::Result<std::path::PathBuf> {
    // Try relative to current exe
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        // Binary in target/debug or target/release — tui/ is at repo root
        let candidates: Vec<Option<std::path::PathBuf>> = vec![
            Some(parent.join("tui")),
            parent.join("../../tui").canonicalize().ok(),
            parent.join("../../../tui").canonicalize().ok(),
        ];
        for candidate in candidates.into_iter().flatten() {
            if candidate.join("src/index.tsx").exists() {
                return Ok(candidate);
            }
        }
    }

    // Try relative to CWD
    let cwd = std::env::current_dir()?;
    let cwd_tui = cwd.join("tui");
    if cwd_tui.join("src/index.tsx").exists() {
        return Ok(cwd_tui);
    }

    bail!(
        "cannot find tui/ directory — run `deskd tui` from the deskd repo root, \
         or ensure tui/src/index.tsx exists relative to the binary"
    )
}

pub fn handle(socket: Option<String>) -> anyhow::Result<()> {
    let socket_path = resolve_socket(socket)?;
    let tui_dir = find_tui_dir()?;
    let entry = tui_dir.join("src/index.tsx");

    // Prefer bun if available, fall back to npx tsx
    let (program, args) = if which("bun") {
        ("bun", vec!["run", entry.to_str().unwrap()])
    } else if which("npx") {
        ("npx", vec!["tsx", entry.to_str().unwrap()])
    } else {
        bail!("neither `bun` nor `npx` found in PATH — install bun or Node.js")
    };

    let status = Command::new(program)
        .args(&args)
        .arg("--socket")
        .arg(&socket_path)
        .env("DESKD_BUS_SOCKET", &socket_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .with_context(|| format!("failed to spawn `{program}`"))?;

    if !status.success()
        && let Some(code) = status.code()
    {
        std::process::exit(code);
    }

    Ok(())
}

fn which(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
