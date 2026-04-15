/// Config watcher — detects system_prompt changes and injects updates into the
/// running session.
///
/// Polls the agent's deskd.yaml file for system_prompt changes. When detected,
/// updates the stored agent state and sends a bus message with the new prompt
/// content so the worker injects it into the existing session (no restart).
use tracing::{info, warn};

/// Watch a config file for system_prompt changes and inject updates.
///
/// Polls every 30 seconds (same cadence as schedule watcher). When the
/// system_prompt in deskd.yaml differs from the running agent's state,
/// updates the state file and sends a bus message containing the new prompt.
pub async fn watch_system_prompt(config_path: String, bus_socket: String, agent_name: String) {
    // Load initial system_prompt from config.
    let mut last_prompt = match crate::config::UserConfig::load(&config_path) {
        Ok(cfg) => cfg.system_prompt,
        Err(e) => {
            warn!(agent = %agent_name, error = %e, "config_watcher: failed to load initial config");
            String::new()
        }
    };
    let mut last_modified = file_mtime(&config_path);

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        // Skip if file hasn't changed on disk.
        let current_mtime = file_mtime(&config_path);
        if current_mtime == last_modified {
            continue;
        }
        last_modified = current_mtime;

        // Reload config and check system_prompt.
        let cfg = match crate::config::UserConfig::load(&config_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(agent = %agent_name, error = %e, "config_watcher: failed to reload config");
                continue;
            }
        };

        if cfg.system_prompt == last_prompt {
            continue;
        }

        info!(
            agent = %agent_name,
            "system_prompt changed, updating agent state and injecting into session"
        );

        // Update the stored agent state with the new system_prompt.
        match crate::app::agent::load_state(&agent_name) {
            Ok(mut state) => {
                state.config.system_prompt = cfg.system_prompt.clone();
                if let Err(e) = crate::app::agent::save_state_pub(&state) {
                    warn!(agent = %agent_name, error = %e, "config_watcher: failed to save updated state");
                    continue;
                }
            }
            Err(e) => {
                warn!(agent = %agent_name, error = %e, "config_watcher: failed to load agent state");
                continue;
            }
        }

        // Send a normal bus message with the updated instructions injected as
        // context. This preserves session history — no fresh restart needed.
        let target = format!("agent:{}", agent_name);
        let task = format!(
            "Your system prompt has been updated. New instructions:\n\n{}\n\nAcknowledge the update.",
            cfg.system_prompt
        );
        if let Err(e) =
            crate::app::bus::send_message(&bus_socket, "config-watcher", &target, &task).await
        {
            warn!(agent = %agent_name, error = %e, "config_watcher: failed to send prompt update message");
            continue;
        }

        last_prompt = cfg.system_prompt;
        info!(agent = %agent_name, "system_prompt hot-reloaded successfully");
    }
}

fn file_mtime(path: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_mtime_returns_none_for_missing_file() {
        assert!(file_mtime("/tmp/nonexistent-deskd-test-file-xyz").is_none());
    }

    #[test]
    fn file_mtime_returns_some_for_existing_file() {
        // Cargo.toml always exists in the repo root.
        assert!(file_mtime("Cargo.toml").is_some());
    }
}
