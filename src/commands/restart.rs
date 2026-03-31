//! `deskd restart` subcommand handler.

use anyhow::Result;
use tracing::info;

/// Find a running `deskd serve` process by scanning /proc (Linux only).
/// Returns (pid, config_path) of the first match found.
#[cfg(target_os = "linux")]
pub fn find_serve_process() -> Option<(u32, String)> {
    use std::fs;
    let proc = fs::read_dir("/proc").ok()?;
    for entry in proc.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid == std::process::id() {
            continue;
        }
        let cmdline_path = format!("/proc/{}/cmdline", pid);
        let Ok(cmdline) = fs::read(&cmdline_path) else {
            continue;
        };
        let args: Vec<&str> = cmdline
            .split(|&b| b == 0)
            .filter_map(|s| {
                let s = std::str::from_utf8(s).ok()?;
                if s.is_empty() { None } else { Some(s) }
            })
            .collect();

        let is_deskd = args.first().map(|a| a.ends_with("deskd")).unwrap_or(false);
        let has_serve = args.contains(&"serve");
        if is_deskd && has_serve {
            let config = args.windows(2).find_map(|w| {
                if w[0] == "--config" {
                    Some(w[1].to_string())
                } else {
                    None
                }
            });
            return Some((pid, config.unwrap_or_default()));
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn find_serve_process() -> Option<(u32, String)> {
    None
}

/// Kill the running `deskd serve` and restart it with the same (or provided) config.
pub async fn handle(config_override: Option<String>) -> Result<()> {
    use std::time::Duration;

    let (pid, detected_config) = match find_serve_process() {
        Some(p) => p,
        None => {
            if let Some(cfg) = config_override {
                info!("No running deskd serve found — starting fresh with {}", cfg);
                crate::serve::serve(cfg).await?;
                return Ok(());
            }
            anyhow::bail!(
                "No running `deskd serve` process found. \
                 Pass --config to start one, or start it manually first."
            );
        }
    };

    let config_path = config_override
        .or_else(|| {
            if detected_config.is_empty() {
                None
            } else {
                Some(detected_config.clone())
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Found deskd serve (pid {}) but could not determine its --config path. \
                 Pass --config explicitly.",
                pid
            )
        })?;

    info!(pid = pid, config = %config_path, "sending SIGTERM to deskd serve");
    let kill_status = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run kill: {}", e))?;
    if !kill_status.success() {
        anyhow::bail!("kill -TERM {} failed: {}", pid, kill_status);
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !std::path::Path::new(&format!("/proc/{}", pid)).exists() {
            break;
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "deskd serve (pid {}) did not exit within 10 s after SIGTERM",
                pid
            );
        }
    }
    info!(pid = pid, "old deskd serve exited — restarting");

    crate::serve::serve(config_path).await
}
