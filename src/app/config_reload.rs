/// Unified config hot-reload — watches deskd.yaml and restarts restartable
/// components when the config changes.
///
/// Restartable components (cancelled/aborted and respawned on config change):
///   - Adapters (Telegram, Discord)
///   - Schedule watcher
///   - Reminder runner
///   - Sub-agent workers
///
/// Session-persistent components (NOT restarted):
///   - Bus server (transport layer)
///   - Main worker (Claude session)
///   - Bus API handler
///   - Workflow engine
///
/// System prompt changes are still injected via bus message (existing pattern
/// in config_watcher.rs) rather than restarting the worker.
///
/// # Graceful adapter shutdown (Part A)
///
/// Adapters that support cooperative cancellation (currently Telegram) are
/// cancelled via their `CancellationToken` first, giving them up to 1 second
/// to drain in-flight messages before the task handle is aborted.
///
/// # Selective reload (Part B)
///
/// `classify_config_change` inspects what actually changed and returns a
/// `ConfigChangeset` so only the affected components are restarted, avoiding
/// unnecessary adapter disruption on system-prompt-only or schedule-only edits.
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::{adapters, config_watcher, schedule, worker};
use crate::config;

// ─── Part B: config diff classifier ─────────────────────────────────────────

/// What changed between two `UserConfig` snapshots.
#[derive(Debug, Default, PartialEq)]
pub struct ConfigChangeset {
    /// Telegram routes, token, or admin_ids changed → adapter restart needed.
    pub adapters_changed: bool,
    /// Schedules changed → schedule_watcher restart needed.
    pub schedules_changed: bool,
    /// Sub-agent definitions changed → sub-agent worker restart needed.
    pub sub_agents_changed: bool,
    /// Only system_prompt changed (and nothing else requiring a restart).
    pub system_prompt_only: bool,
}

/// Compare two `UserConfig` snapshots and classify what changed.
/// Returns a `ConfigChangeset` that callers use to decide which components
/// need to be restarted.
pub fn classify_config_change(
    old: &config::UserConfig,
    new: &config::UserConfig,
) -> ConfigChangeset {
    let adapters_changed = old.telegram != new.telegram || old.discord != new.discord;
    let schedules_changed = old.schedules != new.schedules;
    let sub_agents_changed = old.agents != new.agents;
    let system_prompt_changed = old.system_prompt != new.system_prompt;

    // Classify as system-prompt-only when ONLY the system prompt differs and
    // nothing that requires restarting any component has changed.
    let system_prompt_only =
        system_prompt_changed && !adapters_changed && !schedules_changed && !sub_agents_changed;

    ConfigChangeset {
        adapters_changed,
        schedules_changed,
        sub_agents_changed,
        system_prompt_only,
    }
}

// ─── Part A: AgentComponents with cooperative cancellation ──────────────────

/// Holds all restartable component handles and cancellation tokens for a single agent.
pub struct AgentComponents {
    pub adapter_handles: Vec<JoinHandle<()>>,
    /// Cancellation tokens — one per adapter, in the same order as `adapter_handles`.
    pub adapter_cancel_tokens: Vec<CancellationToken>,
    pub schedule_watcher: Option<JoinHandle<()>>,
    pub config_watcher: Option<JoinHandle<()>>,
    pub reminder_runner: Option<JoinHandle<()>>,
    pub sub_agent_handles: Vec<JoinHandle<()>>,
}

impl AgentComponents {
    /// Gracefully cancel then abort all restartable components.
    ///
    /// 1. Cancel all adapter tokens (cooperative cancellation).
    /// 2. Wait up to 1 s for adapter tasks to finish.
    /// 3. Abort any adapters that are still running after the grace period.
    /// 4. Abort all remaining non-adapter components immediately.
    pub async fn abort_all(&mut self) {
        // Step 1: signal cooperative cancellation for all adapters.
        for token in &self.adapter_cancel_tokens {
            token.cancel();
        }

        // Step 2: wait up to 1 s for adapters to drain.
        // We can't await the JoinHandles directly without consuming them, so we
        // use a short sleep as the grace window — the tokens already signalled
        // cancellation; the tasks will exit their select! loops promptly.
        if !self.adapter_handles.is_empty() {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }

        // Step 3: abort adapter handles (no-op if already finished).
        for h in self.adapter_handles.drain(..) {
            h.abort();
        }
        self.adapter_cancel_tokens.clear();

        // Step 4: abort remaining components immediately.
        if let Some(h) = self.schedule_watcher.take() {
            h.abort();
        }
        if let Some(h) = self.config_watcher.take() {
            h.abort();
        }
        if let Some(h) = self.reminder_runner.take() {
            h.abort();
        }
        for h in self.sub_agent_handles.drain(..) {
            h.abort();
        }
    }

    /// Abort only adapter components (used for adapter-only config changes).
    pub async fn abort_adapters(&mut self) {
        for token in &self.adapter_cancel_tokens {
            token.cancel();
        }
        if !self.adapter_handles.is_empty() {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        for h in self.adapter_handles.drain(..) {
            h.abort();
        }
        self.adapter_cancel_tokens.clear();
    }

    /// Abort only schedule/reminder components (used for schedule-only config changes).
    pub fn abort_schedules(&mut self) {
        if let Some(h) = self.schedule_watcher.take() {
            h.abort();
        }
        if let Some(h) = self.reminder_runner.take() {
            h.abort();
        }
    }

    /// Abort only sub-agent worker components.
    pub fn abort_sub_agents(&mut self) {
        for h in self.sub_agent_handles.drain(..) {
            h.abort();
        }
    }

    /// Return a summary string of component counts.
    pub fn summary(&self) -> String {
        format!(
            "adapters={}, schedules={}, sub_agents={}",
            self.adapter_handles.len(),
            if self.schedule_watcher.is_some() {
                1
            } else {
                0
            },
            self.sub_agent_handles.len(),
        )
    }
}

/// Spawn all restartable components for an agent and return their handles.
pub async fn spawn_components(
    def: &config::AgentDef,
    user_cfg: Option<&config::UserConfig>,
    admin_telegram_ids: &[i64],
    bus_socket: &str,
    agent_name: &str,
    cfg_path: &str,
) -> anyhow::Result<AgentComponents> {
    let mut components = AgentComponents {
        adapter_handles: Vec::new(),
        adapter_cancel_tokens: Vec::new(),
        schedule_watcher: None,
        config_watcher: None,
        reminder_runner: None,
        sub_agent_handles: Vec::new(),
    };

    // Adapters (Telegram, Discord, etc.)
    for (adapter, cancel_token) in adapters::build_adapters(def, user_cfg, admin_telegram_ids) {
        let bus = bus_socket.to_string();
        let name = agent_name.to_string();
        let adapter_name = adapter.name().to_string();
        let handle = tokio::spawn(async move {
            if let Err(e) = adapter.run(bus, name).await {
                tracing::error!(adapter = %adapter_name, error = %e, "adapter failed");
            }
        });
        components.adapter_handles.push(handle);
        components.adapter_cancel_tokens.push(cancel_token);
    }

    // Schedule watcher
    {
        let bus = bus_socket.to_string();
        let name = agent_name.to_string();
        let config = cfg_path.to_string();
        let home = def.work_dir.clone();
        let handle = tokio::spawn(async move {
            schedule::watch_and_reload(config, bus, name, home).await;
        });
        components.schedule_watcher = Some(handle);
    }

    // Config watcher (system_prompt injection)
    {
        let bus = bus_socket.to_string();
        let name = agent_name.to_string();
        let config = cfg_path.to_string();
        let handle = tokio::spawn(async move {
            config_watcher::watch_system_prompt(config, bus, name).await;
        });
        components.config_watcher = Some(handle);
    }

    // Reminder runner
    {
        let bus = bus_socket.to_string();
        let name = agent_name.to_string();
        let handle = tokio::spawn(async move {
            schedule::run_reminders(bus, name).await;
        });
        components.reminder_runner = Some(handle);
    }

    // Sub-agent workers
    if let Some(ucfg) = user_cfg {
        for sub in &ucfg.agents {
            let mcp_json = serde_json::json!({
                "mcpServers": {
                    "deskd": {
                        "command": "deskd",
                        "args": ["mcp", "--agent", &sub.name]
                    }
                }
            })
            .to_string();

            let context_cfg = sub.context.clone().or_else(|| ucfg.context.clone());

            let sub_cfg = crate::app::agent::AgentConfig {
                name: sub.name.clone(),
                model: sub.model.clone(),
                system_prompt: sub.system_prompt.clone(),
                work_dir: def.work_dir.clone(),
                max_turns: ucfg.max_turns,
                unix_user: def.unix_user.clone(),
                budget_usd: def.budget_usd,
                command: vec![
                    "claude".into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                    "--dangerously-skip-permissions".into(),
                    "--model".into(),
                    sub.model.clone(),
                    "--max-turns".into(),
                    ucfg.max_turns.to_string(),
                    "--mcp-config".into(),
                    mcp_json,
                ],
                config_path: Some(cfg_path.to_string()),
                container: def.container.clone(),
                session: sub.session.clone(),
                runtime: sub.runtime.clone(),
                context: context_cfg,
                compact_threshold: sub.compact_threshold,
                auto_compact_threshold_tokens: sub
                    .auto_compact_threshold_tokens
                    .or(ucfg.auto_compact_threshold_tokens),
            };
            crate::app::agent::create_or_update_from_config(&sub_cfg).await?;

            let sub_name = sub.name.clone();
            let bus = bus_socket.to_string();
            let subs = sub.subscribe.clone();
            let sub_task_store = crate::app::task::TaskStore::default_for_home();
            let handle = tokio::spawn(async move {
                if let Err(e) = worker::run(
                    &sub_name,
                    &bus,
                    Some(bus.clone()),
                    Some(subs),
                    &sub_task_store,
                )
                .await
                {
                    tracing::error!(agent = %sub_name, error = %e, "sub-agent worker exited");
                }
            });
            components.sub_agent_handles.push(handle);
        }
    }

    Ok(components)
}

/// Spawn only adapter components and append them to existing `components`.
async fn spawn_adapters(
    def: &config::AgentDef,
    user_cfg: Option<&config::UserConfig>,
    admin_telegram_ids: &[i64],
    bus_socket: &str,
    agent_name: &str,
    components: &mut AgentComponents,
) {
    for (adapter, cancel_token) in adapters::build_adapters(def, user_cfg, admin_telegram_ids) {
        let bus = bus_socket.to_string();
        let name = agent_name.to_string();
        let adapter_name = adapter.name().to_string();
        let handle = tokio::spawn(async move {
            if let Err(e) = adapter.run(bus, name).await {
                tracing::error!(adapter = %adapter_name, error = %e, "adapter failed");
            }
        });
        components.adapter_handles.push(handle);
        components.adapter_cancel_tokens.push(cancel_token);
    }
}

/// Spawn only schedule/reminder components and store them in `components`.
fn spawn_schedules(
    def: &config::AgentDef,
    bus_socket: &str,
    agent_name: &str,
    cfg_path: &str,
    components: &mut AgentComponents,
) {
    {
        let bus = bus_socket.to_string();
        let name = agent_name.to_string();
        let config = cfg_path.to_string();
        let home = def.work_dir.clone();
        let handle = tokio::spawn(async move {
            schedule::watch_and_reload(config, bus, name, home).await;
        });
        components.schedule_watcher = Some(handle);
    }
    {
        let bus = bus_socket.to_string();
        let name = agent_name.to_string();
        let handle = tokio::spawn(async move {
            schedule::run_reminders(bus, name).await;
        });
        components.reminder_runner = Some(handle);
    }
}

/// Watch the agent's deskd.yaml for changes and hot-reload components selectively.
///
/// Polls the file mtime every 30 seconds. On change, uses `classify_config_change`
/// to determine which components need restarting:
///   - `system_prompt_only` → nothing restarted (system_prompt injected via bus by config_watcher)
///   - `adapters_changed` → cooperative cancel + respawn adapters only
///   - `schedules_changed` → abort + respawn schedule_watcher only
///   - otherwise → full abort_all + respawn
///
/// Bus server, main worker, bus API, and workflow engine are never affected.
pub async fn watch_and_reload(
    def: config::AgentDef,
    initial_components: AgentComponents,
    admin_telegram_ids: Vec<i64>,
    bus_socket: String,
    agent_name: String,
    cfg_path: String,
) {
    let mut components = initial_components;
    let mut last_modified = file_mtime(&cfg_path);
    // Track the last known config for diffing.
    let mut last_user_cfg = config::UserConfig::load(&cfg_path).ok();

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let current_mtime = file_mtime(&cfg_path);
        if current_mtime == last_modified {
            continue;
        }
        last_modified = current_mtime;

        info!(agent = %agent_name, "config file changed, analysing diff");

        // Reload config.
        let new_user_cfg = match config::UserConfig::load(&cfg_path) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(
                    agent = %agent_name,
                    error = %e,
                    "failed to reload config, components unchanged"
                );
                continue;
            }
        };

        // Classify what changed.
        let changeset = if let Some(ref old_cfg) = last_user_cfg {
            classify_config_change(old_cfg, &new_user_cfg)
        } else {
            // No previous config — treat everything as changed.
            ConfigChangeset {
                adapters_changed: true,
                schedules_changed: true,
                sub_agents_changed: true,
                system_prompt_only: false,
            }
        };
        last_user_cfg = Some(new_user_cfg.clone());

        if changeset.system_prompt_only {
            // system_prompt_only: config_watcher.rs injects the new prompt via bus.
            // No adapter or schedule disruption needed.
            info!(agent = %agent_name, "system_prompt changed only — no component restart needed");
            continue;
        }

        if !changeset.adapters_changed
            && !changeset.schedules_changed
            && !changeset.sub_agents_changed
        {
            // Only non-restartable fields changed (e.g. mcp_config, context config).
            info!(agent = %agent_name, "config change requires no component restart");
            continue;
        }

        // Selective restart.
        if changeset.adapters_changed
            && !changeset.schedules_changed
            && !changeset.sub_agents_changed
        {
            info!(agent = %agent_name, "adapter config changed — restarting adapters only");
            components.abort_adapters().await;
            spawn_adapters(
                &def,
                Some(&new_user_cfg),
                &admin_telegram_ids,
                &bus_socket,
                &agent_name,
                &mut components,
            )
            .await;
            info!(agent = %agent_name, adapters = components.adapter_handles.len(), "adapters restarted");
            continue;
        }

        if changeset.schedules_changed
            && !changeset.adapters_changed
            && !changeset.sub_agents_changed
        {
            info!(agent = %agent_name, "schedule config changed — restarting schedules only");
            components.abort_schedules();
            spawn_schedules(&def, &bus_socket, &agent_name, &cfg_path, &mut components);
            info!(agent = %agent_name, "schedules restarted");
            continue;
        }

        // Full restart for combined or sub-agent changes.
        info!(agent = %agent_name, "config change requires full component restart");
        let old_summary = components.summary();
        components.abort_all().await;
        info!(agent = %agent_name, old = %old_summary, "aborted old components");

        match spawn_components(
            &def,
            Some(&new_user_cfg),
            &admin_telegram_ids,
            &bus_socket,
            &agent_name,
            &cfg_path,
        )
        .await
        {
            Ok(new_components) => {
                let summary = new_components.summary();
                components = new_components;
                info!(
                    agent = %agent_name,
                    summary = %summary,
                    "config reloaded: {}", summary
                );
            }
            Err(e) => {
                warn!(
                    agent = %agent_name,
                    error = %e,
                    "failed to respawn components after config reload"
                );
            }
        }
    }
}

fn file_mtime(path: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ScheduleAction, ScheduleDef, TelegramRoute, TelegramRoutesConfig};

    fn base_config() -> config::UserConfig {
        config::UserConfig {
            system_prompt: "You are a helpful assistant.".into(),
            ..Default::default()
        }
    }

    // ── classify_config_change ────────────────────────────────────────────────

    #[test]
    fn test_classify_no_change() {
        let cfg = base_config();
        let cs = classify_config_change(&cfg, &cfg);
        assert_eq!(cs, ConfigChangeset::default());
        assert!(!cs.system_prompt_only);
    }

    #[test]
    fn test_classify_system_prompt_only() {
        let old = base_config();
        let mut new = old.clone();
        new.system_prompt = "New prompt.".into();
        let cs = classify_config_change(&old, &new);
        assert!(cs.system_prompt_only);
        assert!(!cs.adapters_changed);
        assert!(!cs.schedules_changed);
        assert!(!cs.sub_agents_changed);
    }

    #[test]
    fn test_classify_telegram_routes_changed() {
        let old = base_config();
        let mut new = old.clone();
        new.telegram = Some(TelegramRoutesConfig {
            routes: vec![TelegramRoute {
                chat_id: 12345,
                mention_only: false,
                name: None,
                route_to: None,
            }],
        });
        let cs = classify_config_change(&old, &new);
        assert!(cs.adapters_changed);
        assert!(!cs.system_prompt_only);
        assert!(!cs.schedules_changed);
    }

    #[test]
    fn test_classify_schedules_changed() {
        let old = base_config();
        let mut new = old.clone();
        new.schedules = vec![ScheduleDef {
            cron: "0 9 * * *".into(),
            target: "agent:dev".into(),
            action: ScheduleAction::Raw,
            config: None,
            timezone: None,
        }];
        let cs = classify_config_change(&old, &new);
        assert!(cs.schedules_changed);
        assert!(!cs.adapters_changed);
        assert!(!cs.system_prompt_only);
    }

    #[test]
    fn test_classify_system_prompt_and_adapters_not_prompt_only() {
        let old = base_config();
        let mut new = old.clone();
        new.system_prompt = "Updated.".into();
        new.telegram = Some(TelegramRoutesConfig { routes: vec![] });
        let cs = classify_config_change(&old, &new);
        assert!(
            !cs.system_prompt_only,
            "both changed — should not be prompt-only"
        );
        assert!(cs.adapters_changed);
    }

    // ── AgentComponents helpers ──────────────────────────────────────────────

    #[test]
    fn test_agent_components_summary() {
        let components = AgentComponents {
            adapter_handles: Vec::new(),
            adapter_cancel_tokens: Vec::new(),
            schedule_watcher: None,
            config_watcher: None,
            reminder_runner: None,
            sub_agent_handles: Vec::new(),
        };
        assert_eq!(
            components.summary(),
            "adapters=0, schedules=0, sub_agents=0"
        );
    }

    #[tokio::test]
    async fn test_agent_components_abort_all_empty() {
        let mut components = AgentComponents {
            adapter_handles: Vec::new(),
            adapter_cancel_tokens: Vec::new(),
            schedule_watcher: None,
            config_watcher: None,
            reminder_runner: None,
            sub_agent_handles: Vec::new(),
        };
        // Should not panic on empty components.
        components.abort_all().await;
        assert!(components.adapter_handles.is_empty());
        assert!(components.adapter_cancel_tokens.is_empty());
        assert!(components.schedule_watcher.is_none());
        assert!(components.sub_agent_handles.is_empty());
    }

    #[tokio::test]
    async fn test_adapter_cancel_token_fires_on_abort_adapters() {
        let token = CancellationToken::new();
        let child = token.clone();

        // Spawn a task that waits for cancellation.
        let handle = tokio::spawn(async move {
            child.cancelled().await;
        });

        let mut components = AgentComponents {
            adapter_handles: vec![handle],
            adapter_cancel_tokens: vec![token],
            schedule_watcher: None,
            config_watcher: None,
            reminder_runner: None,
            sub_agent_handles: Vec::new(),
        };

        components.abort_adapters().await;
        assert!(components.adapter_handles.is_empty());
        assert!(components.adapter_cancel_tokens.is_empty());
    }

    #[test]
    fn test_file_mtime_missing() {
        assert!(file_mtime("/tmp/nonexistent-deskd-config-reload-test").is_none());
    }

    #[test]
    fn test_file_mtime_existing() {
        assert!(file_mtime("Cargo.toml").is_some());
    }
}
