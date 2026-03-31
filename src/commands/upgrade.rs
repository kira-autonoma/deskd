//! `deskd upgrade` subcommand handler.

use anyhow::Result;

use super::restart::find_serve_process;

pub async fn handle(install_dir_override: Option<String>) -> Result<()> {
    let install_dir = if let Some(dir) = install_dir_override {
        std::path::PathBuf::from(dir)
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                std::path::PathBuf::from(home).join(".local").join("bin")
            })
    };

    let os_name = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => anyhow::bail!("Unsupported OS for upgrade: {}", other),
    };
    let arch_name = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => anyhow::bail!("Unsupported architecture for upgrade: {}", other),
    };

    let artifact = format!("deskd-{}-{}", os_name, arch_name);
    let url = format!(
        "https://github.com/kgatilin/deskd/releases/latest/download/{}",
        artifact
    );

    println!("Downloading {} ...", url);
    std::fs::create_dir_all(&install_dir)?;

    let tmp_path = install_dir.join(".deskd-upgrade.tmp");
    let dest_path = install_dir.join("deskd");

    let status = std::process::Command::new("curl")
        .args([
            "-fsSL",
            &url,
            "-o",
            tmp_path.to_str().unwrap_or("/tmp/.deskd-upgrade.tmp"),
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run curl: {}", e))?;

    if !status.success() {
        anyhow::bail!(
            "curl download failed (exit {}). Check https://github.com/kgatilin/deskd/releases",
            status
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }

    std::fs::rename(&tmp_path, &dest_path)?;
    println!("Installed: {}", dest_path.display());

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("codesign")
            .args([
                "--force",
                "--sign",
                "-",
                dest_path.to_str().unwrap_or("deskd"),
            ])
            .status();
    }

    match find_serve_process() {
        Some((pid, ref config_path)) if !config_path.is_empty() => {
            println!(
                "Restarting deskd serve (pid {}) with config {}",
                pid, config_path
            );
            super::restart::handle(Some(config_path.clone())).await?;
        }
        Some((pid, _)) => {
            println!(
                "deskd serve (pid {}) is running but config path is unknown — \
                 please restart it manually.",
                pid
            );
        }
        None => {
            println!("No running deskd serve detected — upgrade complete.");
        }
    }

    Ok(())
}
