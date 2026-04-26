use crate::domain::config_types::{ConfigAgentRuntime, ConfigContextConfig, ConfigSessionMode};
use crate::infra::dto::ConfigModelDef;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// Re-export path helpers (infra layer — filesystem layout concerns).
pub use crate::infra::paths::{
    agent_bus_socket, ensure_dir_owned, log_dir, reminders_dir, state_dir,
};

/// A one-shot reminder that fires at a specific time and posts a message to the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemindDef {
    /// ISO 8601 timestamp at which to fire.
    pub at: String,
    /// Bus target (e.g. `agent:kira`).
    pub target: String,
    /// Payload text to post.
    pub message: String,
}

fn default_max_turns() -> u32 {
    100
}

fn default_budget_usd() -> f64 {
    50.0
}

// ─── Root workspace.yaml ─────────────────────────────────────────────────────

/// Top-level workspace config (workspace.yaml).
/// Managed by root or the admin user. Defines top-level agents, their unix
/// users, Telegram bots, and the path to each agent's own deskd.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub agents: Vec<AgentDef>,
    /// Named container profiles that agents can reference by name.
    #[serde(default)]
    pub containers: HashMap<String, ContainerConfig>,
    /// Named rooms — formal groupings of agents with shared context.
    /// When present, `room_list` returns these instead of treating each agent as a room.
    #[serde(default)]
    pub rooms: Vec<RoomDef>,
    /// Telegram user IDs allowed to run admin commands (/restart, etc.).
    #[serde(default)]
    pub admin_telegram_ids: Vec<i64>,
    /// A2A protocol configuration for cross-instance agent communication.
    #[serde(default)]
    pub a2a: Option<A2aConfig>,
}

/// A2A protocol configuration in workspace.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aConfig {
    /// Public URL for this deskd instance (e.g. "https://dev.agent.example.com").
    pub url: String,
    /// API key for authenticating incoming A2A requests.
    /// Typically set via ${A2A_API_KEY}.
    #[serde(default)]
    pub api_key: Option<String>,
    /// HTTP listen address for the A2A server (e.g. "0.0.0.0:3000").
    #[serde(default = "default_a2a_listen")]
    pub listen: String,
    /// Instance-level description shown in the Agent Card.
    #[serde(default)]
    pub description: Option<String>,
    /// Authentication mode: "api_key" (default), "jwt", or "none".
    #[serde(default = "default_a2a_auth")]
    pub auth: String,
    /// Path to Ed25519 private key PEM for JWT signing.
    /// Default: ~/.deskd/a2a_key.pem
    #[serde(default)]
    pub private_key: Option<String>,
    /// Trusted public keys for JWT verification (base64url-encoded Ed25519 keys).
    /// Incoming JWTs are verified against these keys. Add remote agents' public keys here.
    #[serde(default)]
    pub trusted_keys: Vec<String>,
}

fn default_a2a_listen() -> String {
    "0.0.0.0:3000".to_string()
}

fn default_a2a_auth() -> String {
    "api_key".to_string()
}

/// A room is a named work context: namespace + context folder + set of agents.
/// Rooms are the primary drill-down unit in dashboard navigation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomDef {
    pub name: String,
    /// Working directory for the room (agents inherit this).
    pub work_dir: String,
    /// Path to a context file (e.g. CLAUDE.md) shared by all agents in the room.
    #[serde(default)]
    pub context: Option<String>,
    /// Names of agents that belong to this room (must be defined in `agents` section).
    #[serde(default)]
    pub agents: Vec<String>,
}

/// Telegram bot adapter config. Defined per-agent — each agent has its own bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather. Typically set via ${TELEGRAM_BOT_TOKEN}.
    pub token: String,
}

/// Discord bot adapter config. Defined per-agent in workspace.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Bot token from Discord Developer Portal. Typically set via ${DISCORD_BOT_TOKEN}.
    pub token: String,
}

/// Discord channel routing config in the per-user deskd.yaml.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordRoutesConfig {
    pub routes: Vec<DiscordRoute>,
}

/// A single Discord channel route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordRoute {
    /// Discord channel ID (u64).
    pub channel_id: u64,
    /// Human-readable name for this channel, shown to the agent as context.
    pub name: Option<String>,
}

/// Container runtime config for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    /// OCI image to use (e.g. "claude-code-local:official").
    pub image: String,
    /// Host paths to bind-mount. Format: "host_path" or "host_path:container_path"
    /// or "host_path:container_path:ro" for read-only.
    #[serde(default)]
    pub mounts: Vec<String>,
    /// Docker volumes. Format: "volume_name:container_path".
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Environment variables to set inside the container.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Container runtime binary. Defaults to "docker".
    #[serde(default = "default_container_runtime")]
    pub runtime: String,
}

fn default_container_runtime() -> String {
    "docker".to_string()
}

/// Definition of a top-level agent in workspace.yaml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    /// Linux user to run the agent as. Required for isolation.
    pub unix_user: Option<String>,
    /// Agent's working directory (also determines bus socket path).
    pub work_dir: String,
    /// Path to the agent's own deskd.yaml. Defaults to {work_dir}/deskd.yaml.
    pub config: Option<String>,
    /// Telegram bot for this agent. When set, a Telegram adapter is started
    /// on the agent's bus when deskd serves this workspace.
    pub telegram: Option<TelegramConfig>,
    /// Discord bot for this agent. When set, a Discord adapter is started
    /// on the agent's bus when deskd serves this workspace.
    pub discord: Option<DiscordConfig>,
    /// Claude model override. Default is set in the agent's deskd.yaml.
    #[serde(default)]
    pub model: Option<String>,
    /// Command to run as the agent process. Defaults to ["claude"].
    #[serde(default = "default_command")]
    pub command: Vec<String>,
    /// Budget cap in USD. Worker rejects tasks when exceeded.
    /// A value of `0` (or any non-positive value) disables the cap and treats
    /// the agent as having an unlimited budget; see worker::check_budget.
    #[serde(default = "default_budget_usd")]
    pub budget_usd: f64,
    /// Container config. When set, the agent process runs inside a container.
    #[serde(default)]
    pub container: Option<ContainerConfig>,
    /// Agent runtime protocol: claude (default) or acp.
    #[serde(default)]
    pub runtime: ConfigAgentRuntime,
}

impl AgentDef {
    /// Derive the path to the agent's deskd.yaml config file.
    pub fn config_path(&self) -> String {
        self.config.clone().unwrap_or_else(|| {
            PathBuf::from(&self.work_dir)
                .join("deskd.yaml")
                .to_string_lossy()
                .into_owned()
        })
    }

    /// Derive the agent's bus socket path.
    pub fn bus_socket(&self) -> String {
        agent_bus_socket(&self.work_dir)
    }
}

fn default_command() -> Vec<String> {
    vec!["claude".to_string()]
}

impl WorkspaceConfig {
    /// Load and parse a workspace config file, expanding ${ENV_VAR} references.
    /// Resolves named container profile references (string → inline config).
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read workspace config: {}", path))?;
        let expanded = expand_env_vars(&raw);
        let expanded = Self::resolve_container_profiles(&expanded)?;
        let cfg: WorkspaceConfig =
            serde_yaml::from_str(&expanded).context("failed to parse workspace config")?;
        Ok(cfg)
    }

    /// Pre-process YAML: resolve string container references to inline objects.
    ///
    /// When an agent's `container:` field is a string (e.g. `container: personal`),
    /// replace it with the corresponding entry from the top-level `containers:` map.
    fn resolve_container_profiles(yaml_str: &str) -> Result<String> {
        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(yaml_str).context("failed to pre-parse workspace config")?;

        // Extract named container profiles.
        let profiles = doc
            .get("containers")
            .and_then(|v| v.as_mapping())
            .cloned()
            .unwrap_or_default();

        if profiles.is_empty() {
            // No named profiles — nothing to resolve, return original.
            return Ok(yaml_str.to_string());
        }

        // Resolve string references in agents[].container.
        if let Some(agents) = doc.get_mut("agents").and_then(|v| v.as_sequence_mut()) {
            for agent in agents.iter_mut() {
                if let Some(container_val) = agent.get("container")
                    && let Some(profile_name) = container_val.as_str()
                {
                    let key = serde_yaml::Value::String(profile_name.to_string());
                    let resolved = profiles.get(&key).cloned().ok_or_else(|| {
                        anyhow::anyhow!(
                            "agent references unknown container profile '{}'",
                            profile_name
                        )
                    })?;
                    agent
                        .as_mapping_mut()
                        .unwrap()
                        .insert(serde_yaml::Value::String("container".to_string()), resolved);
                }
            }
        }

        serde_yaml::to_string(&doc).context("failed to re-serialize workspace config")
    }
}

// ─── Runtime serve state ────────────────────────────────────────────────────

/// Runtime state written by `deskd serve` so other commands can auto-discover config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeState {
    /// Path to the workspace.yaml that serve was started with.
    pub workspace_config: String,
    /// ISO 8601 timestamp when serve started.
    pub started_at: String,
    /// Per-agent runtime info.
    #[serde(default)]
    pub agents: HashMap<String, AgentServeState>,
    /// Formal room definitions from workspace config.
    #[serde(default)]
    pub rooms: Vec<RoomDef>,
}

/// Per-agent runtime info in serve state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentServeState {
    pub work_dir: String,
    pub bus_socket: String,
    pub config_path: String,
}

impl ServeState {
    /// Path to the serve state file: `~/.deskd/serve.state.yaml`.
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".deskd").join("serve.state.yaml")
    }

    /// Load serve state from disk. Returns None if not running.
    pub fn load() -> Option<Self> {
        let path = Self::path();
        let content = std::fs::read_to_string(&path).ok()?;
        serde_yaml::from_str(&content).ok()
    }

    /// Write serve state to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let content = serde_yaml::to_string(self).context("failed to serialize serve state")?;
        std::fs::write(&path, content).context("failed to write serve state")?;
        Ok(())
    }

    /// Remove the serve state file (on shutdown).
    pub fn remove() {
        let _ = std::fs::remove_file(Self::path());
    }

    /// Find the first agent that has a user config with SM models.
    pub fn find_agent_config(&self) -> Option<&AgentServeState> {
        self.agents.values().next()
    }

    /// Find a specific agent by name.
    pub fn agent(&self, name: &str) -> Option<&AgentServeState> {
        self.agents.get(name)
    }

    /// Get any bus socket from the running agents.
    pub fn any_bus_socket(&self) -> Option<&str> {
        self.agents.values().next().map(|a| a.bus_socket.as_str())
    }
}

// ─── Per-user deskd.yaml ─────────────────────────────────────────────────────

/// Per-user agent config (deskd.yaml, lives in the agent's work_dir).
/// Defines the agent's own model, system prompt, sub-agents, channels,
/// Telegram routes, and schedules. Managed by the agent's unix user.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct UserConfig {
    /// Claude model for the main agent. Overridden by workspace.yaml `model` if set.
    #[serde(default = "default_model")]
    pub model: String,
    /// System prompt for the main agent.
    #[serde(default)]
    pub system_prompt: String,
    /// Max turns per task.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Named broadcast/task channels this agent participates in.
    #[serde(default)]
    pub channels: Vec<ChannelDef>,
    /// Sub-agents spawned and managed within this agent's bus scope.
    #[serde(default)]
    pub agents: Vec<SubAgentDef>,
    /// Telegram channel routing for this agent.
    pub telegram: Option<TelegramRoutesConfig>,
    /// Discord channel routing for this agent.
    pub discord: Option<DiscordRoutesConfig>,
    /// Scheduled actions (cron → bus messages).
    #[serde(default)]
    pub schedules: Vec<ScheduleDef>,
    /// MCP server config JSON string or file path, passed to claude via --mcp-config.
    /// Example: '{"mcpServers":{"deskd":{"command":"deskd","args":["mcp","--agent","kira"]}}}'
    #[serde(default)]
    pub mcp_config: Option<String>,
    /// State machine model definitions.
    #[serde(default)]
    pub models: Vec<ConfigModelDef>,
    /// Context system configuration (main branch, compaction).
    #[serde(default)]
    pub context: Option<ConfigContextConfig>,
    /// A2A skills advertised in the Agent Card.
    #[serde(default)]
    pub skills: Vec<SkillDef>,
    /// A2A needs — what this agent wants done (custom extension to A2A spec).
    #[serde(default)]
    pub needs: Vec<NeedDef>,
    /// Optional allow-list of inboxes this top-level agent can read (glob patterns).
    /// If `None` (absent in deskd.yaml), behavior is unrestricted (current default).
    /// If `Some`, only the agent's own inbox plus any inbox matching one of the
    /// listed patterns is readable. Useful on shared-user systems (e.g. macOS dev
    /// laptops) where unix-level file permissions cannot isolate agent inboxes.
    /// Example: `inbox_acl: ["dev", "collab-*"]`.
    #[serde(default)]
    pub inbox_acl: Option<Vec<String>>,
    /// Auto-compact threshold in absolute tokens (top-level / global default for
    /// this deskd.yaml). Sub-agents inherit this when their own SubAgentDef does
    /// not set one. Built-in fallback is `DEFAULT_AUTO_COMPACT_THRESHOLD` (300k).
    #[serde(default)]
    pub auto_compact_threshold_tokens: Option<u64>,
}

/// An A2A skill advertised in the Agent Card (per A2A spec).
/// Defined in deskd.yaml under `skills:`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillDef {
    /// Unique skill identifier (e.g. "code-review").
    pub id: String,
    /// Human-readable name (e.g. "Code Review").
    pub name: String,
    /// What this skill does.
    #[serde(default)]
    pub description: String,
    /// Tags for discovery (e.g. ["go", "rust"]).
    #[serde(default)]
    pub tags: Vec<String>,
}

/// An A2A need — what the agent wants done (custom extension).
/// Defined in deskd.yaml under `needs:`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NeedDef {
    /// Unique need identifier (e.g. "want-restart-button").
    pub id: String,
    /// Human-readable description of what's needed.
    pub description: String,
    /// Tags for discovery (e.g. ["ux", "telegram"]).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Priority: "low", "medium", "high".
    #[serde(default = "default_need_priority")]
    pub priority: String,
}

fn default_need_priority() -> String {
    "medium".to_string()
}

fn default_model() -> String {
    "claude-sonnet-4-6".to_string()
}

/// A named channel for broadcast or task-queue communication.
/// The name becomes the bus target, e.g. `news:ecosystem` or `queue:reviews`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChannelDef {
    pub name: String,
    pub description: String,
}

// Re-export domain types for backward compatibility.
pub use crate::domain::agent::{AgentRuntime, SessionMode};

/// Scope type for sub-agents: controls isolation level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScopeType {
    /// Sub-agent inherits parent's full scope — sees siblings, same FS access.
    #[default]
    Inherit,
    /// Sub-agent gets isolated sub-scope — sees only its own children.
    Narrow,
}

/// A sub-agent running within a parent agent's bus scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubAgentDef {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub system_prompt: String,
    /// Bus targets this agent receives messages from.
    /// Supports glob patterns: `telegram.in:*`, `agent:researcher`.
    pub subscribe: Vec<String>,
    /// Optional allow-list of targets this agent can publish to.
    /// If None, publish to any target is allowed.
    pub publish: Option<Vec<String>>,
    /// Optional allow-list of inboxes this agent can read (glob patterns).
    /// If None, the agent can only read its own inbox (matching its name).
    /// Example: `["kira", "collab-*"]` allows reading the `kira` inbox and
    /// any inbox starting with `collab-`.
    pub inbox_read: Option<Vec<String>>,
    /// Scope type: inherit (default) or narrow.
    /// Inherit = shares parent scope; narrow = isolated sub-scope.
    #[serde(default)]
    pub scope: ScopeType,
    /// Optional allow-list of targets this agent can send messages to.
    /// If None, the agent can message any target (unrestricted).
    /// Example: `["agent:parent", "agent:sibling"]`.
    pub can_message: Option<Vec<String>>,
    /// Optional working directory override. Must be under parent's work_dir.
    pub work_dir: Option<String>,
    /// Optional environment variables for this agent. Isolated from parent env.
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    /// Session mode: persistent (default) or ephemeral.
    /// Ephemeral agents start a fresh session for each task.
    #[serde(default)]
    pub session: ConfigSessionMode,
    /// Agent runtime protocol: claude (default) or acp.
    #[serde(default)]
    pub runtime: ConfigAgentRuntime,
    /// Per-agent context configuration (overrides global UserConfig.context).
    #[serde(default)]
    pub context: Option<ConfigContextConfig>,
    /// Memory agent: context usage fraction (0.0–1.0) that triggers compaction.
    /// Default: 0.8. Only used when runtime is `memory`.
    #[serde(default)]
    pub compact_threshold: Option<f64>,
    /// Memory agent: compaction strategy name. Default: "smart".
    /// Only used when runtime is `memory`.
    #[serde(default)]
    pub compact_strategy: Option<String>,
    /// Auto-compact threshold in absolute tokens for this sub-agent.
    /// Falls back to the parent UserConfig's `auto_compact_threshold_tokens`,
    /// then to the built-in default (300k).
    #[serde(default)]
    pub auto_compact_threshold_tokens: Option<u64>,
}

impl SubAgentDef {
    /// Returns the full scoped name for this agent under the given parent.
    pub fn scoped_name(&self, parent: &str) -> String {
        format!("{}/{}", parent, self.name)
    }

    /// Validate that work_dir is within parent's work_dir (scope containment).
    pub fn validate_work_dir(&self, parent_work_dir: &str) -> anyhow::Result<()> {
        if let Some(ref wd) = self.work_dir {
            let child = std::path::Path::new(wd)
                .canonicalize()
                .unwrap_or_else(|_| wd.into());
            let parent = std::path::Path::new(parent_work_dir)
                .canonicalize()
                .unwrap_or_else(|_| parent_work_dir.into());
            if !child.starts_with(&parent) {
                anyhow::bail!(
                    "sub-agent work_dir '{}' is outside parent scope '{}'",
                    wd,
                    parent_work_dir
                );
            }
        }
        Ok(())
    }
}

/// Telegram channel routing config in the per-user deskd.yaml.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelegramRoutesConfig {
    #[serde(default)]
    pub routes: Vec<TelegramRoute>,
}

/// A single Telegram chat route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelegramRoute {
    /// Telegram chat_id (positive for users/groups, negative for channels/supergroups).
    pub chat_id: i64,
    /// If true, only respond when the bot is @mentioned in this chat.
    #[serde(default)]
    pub mention_only: bool,
    /// Human-readable name for this chat, shown to the agent as context.
    pub name: Option<String>,
    /// Bus target override. When set, incoming messages from this chat are published
    /// to this target (e.g. "agent:collab") instead of the default "telegram.in:<chat_id>".
    #[serde(default)]
    pub route_to: Option<String>,
}

/// A scheduled action that fires on a cron expression and posts a message to the bus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleDef {
    /// Cron expression, e.g. `"0 9 * * *"` for 9 AM daily.
    pub cron: String,
    /// Bus target to post to.
    pub target: String,
    /// What action to take when the schedule fires.
    pub action: ScheduleAction,
    /// Action-specific configuration (e.g. repos list for github_poll).
    pub config: Option<serde_yaml::Value>,
    /// IANA timezone name (e.g. "Europe/Berlin"). Cron fires in this timezone.
    /// Falls back to UTC if not specified.
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    /// Poll GitHub repos for issues with a label, post new issues to target.
    GithubPoll,
    /// Post a static payload string to the target.
    Raw,
    /// Run an arbitrary shell command via `sh -c`.
    /// `config.command` — the shell command to execute.
    /// If the command produces stdout and `target` is non-empty, stdout is posted to the bus.
    Shell,
}

pub use crate::domain::statemachine::{ModelDef, TransitionDef};

impl UserConfig {
    /// Load and parse a deskd.yaml file, expanding ${ENV_VAR} references.
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read user config: {}", path))?;
        let expanded = expand_env_vars(&raw);
        let cfg: UserConfig =
            serde_yaml::from_str(&expanded).context("failed to parse user config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate config invariants that serde can't express on its own.
    pub fn validate(&self) -> Result<()> {
        if let Some(0) = self.auto_compact_threshold_tokens {
            anyhow::bail!("auto_compact_threshold_tokens must be > 0");
        }
        for sub in &self.agents {
            if let Some(0) = sub.auto_compact_threshold_tokens {
                anyhow::bail!(
                    "agent '{}': auto_compact_threshold_tokens must be > 0",
                    sub.name
                );
            }
        }
        Ok(())
    }
}

// ─── Env var expansion ────────────────────────────────────────────────────────

/// Replace `${VAR}` and `$VAR` occurrences with their environment variable values.
/// Unknown variables are left as-is.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let var: String = chars.by_ref().take_while(|&c| c != '}').collect();
                if let Ok(val) = std::env::var(&var) {
                    result.push_str(&val);
                } else {
                    result.push_str(&format!("${{{}}}", var));
                }
            } else if chars
                .peek()
                .map(|c| c.is_alphanumeric() || *c == '_')
                .unwrap_or(false)
            {
                let mut var = String::new();
                while chars
                    .peek()
                    .map(|c| c.is_alphanumeric() || *c == '_')
                    .unwrap_or(false)
                {
                    var.push(chars.next().unwrap());
                }
                if let Ok(val) = std::env::var(&var) {
                    result.push_str(&val);
                } else {
                    result.push_str(&format!("${}", var));
                }
            } else {
                result.push(ch);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars_braces() {
        unsafe { std::env::set_var("TEST_TOKEN_DESKD", "abc123") };
        let result = expand_env_vars("token: ${TEST_TOKEN_DESKD}");
        assert_eq!(result, "token: abc123");
    }

    #[test]
    fn test_expand_env_vars_dollar() {
        unsafe { std::env::set_var("TEST_VAR_DESKD", "hello") };
        let result = expand_env_vars("val: $TEST_VAR_DESKD end");
        assert_eq!(result, "val: hello end");
    }

    #[test]
    fn test_expand_env_vars_unknown_left_as_is() {
        let result = expand_env_vars("val: ${DEFINITELY_NOT_SET_XYZ123}");
        assert_eq!(result, "val: ${DEFINITELY_NOT_SET_XYZ123}");
    }

    #[test]
    fn test_workspace_config_minimal() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
    unix_user: kira
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].name, "kira");
        assert_eq!(cfg.agents[0].unix_user.as_deref(), Some("kira"));
        assert!(cfg.agents[0].telegram.is_none());
        assert!(cfg.agents[0].config.is_none());
    }

    #[test]
    fn test_workspace_config_admin_telegram_ids() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
admin_telegram_ids:
  - 123456789
  - 987654321
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.admin_telegram_ids, vec![123456789i64, 987654321i64]);
    }

    #[test]
    fn test_workspace_config_admin_telegram_ids_default() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.admin_telegram_ids.is_empty());
    }

    #[test]
    fn test_workspace_config_with_telegram() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
    unix_user: kira
    telegram:
      token: "bot-token-123"
  - name: dev
    work_dir: /home/dev
    unix_user: dev
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.agents[0].telegram.is_some());
        assert_eq!(
            cfg.agents[0].telegram.as_ref().unwrap().token,
            "bot-token-123"
        );
        assert!(cfg.agents[1].telegram.is_none());
    }

    #[test]
    fn test_agent_def_bus_socket() {
        let def = AgentDef {
            name: "kira".into(),
            unix_user: Some("kira".into()),
            work_dir: "/home/kira".into(),
            config: None,
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
            runtime: ConfigAgentRuntime::default(),
        };
        assert_eq!(def.bus_socket(), "/home/kira/.deskd/bus.sock");
        assert_eq!(def.config_path(), "/home/kira/deskd.yaml");
    }

    #[test]
    fn test_agent_def_explicit_config_path() {
        let def = AgentDef {
            name: "kira".into(),
            unix_user: None,
            work_dir: "/home/kira".into(),
            config: Some("/etc/agents/kira.yaml".into()),
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
            runtime: ConfigAgentRuntime::default(),
        };
        assert_eq!(def.config_path(), "/etc/agents/kira.yaml");
    }

    #[test]
    fn test_user_config_defaults() {
        let yaml = r#"
model: claude-opus-4-6
system_prompt: "You are Kira."
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.model, "claude-opus-4-6");
        assert_eq!(cfg.max_turns, 100);
        assert!(cfg.channels.is_empty());
        assert!(cfg.agents.is_empty());
        assert!(cfg.schedules.is_empty());
    }

    #[test]
    fn test_user_config_full() {
        let yaml = r#"
model: claude-opus-4-6
system_prompt: "You are Kira."

channels:
  - name: "news:ecosystem"
    description: "Ecosystem updates"
  - name: "queue:reviews"
    description: "PR review requests"

agents:
  - name: dev
    model: claude-sonnet-4-6
    system_prompt: "You implement code."
    subscribe:
      - "agent:dev"
    publish:
      - "agent:*"
      - "telegram.out:*"

  - name: researcher
    model: claude-haiku-4-5
    system_prompt: "You research topics."
    subscribe:
      - "agent:researcher"

telegram:
  routes:
    - chat_id: -1001234567890
    - chat_id: -1001234567891

schedules:
  - cron: "0 9 * * *"
    target: "telegram.out:-1001234567890"
    action: raw
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.channels.len(), 2);
        assert_eq!(cfg.agents.len(), 2);
        assert_eq!(cfg.agents[0].subscribe, vec!["agent:dev"]);
        assert_eq!(cfg.agents[0].publish.as_ref().unwrap().len(), 2);
        assert!(cfg.agents[1].publish.is_none()); // allow all
        assert_eq!(cfg.telegram.unwrap().routes[0].chat_id, -1001234567890);
        assert_eq!(cfg.schedules.len(), 1);
    }

    #[test]
    fn test_sub_agent_session_mode() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"

agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe:
      - "agent:worker"
    session: ephemeral

  - name: researcher
    model: claude-sonnet-4-6
    system_prompt: "Researcher"
    subscribe:
      - "agent:researcher"
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].session, ConfigSessionMode::Ephemeral);
        assert_eq!(cfg.agents[1].session, ConfigSessionMode::Persistent); // default
    }

    #[test]
    fn test_workspace_config_with_container() {
        let yaml = r#"
agents:
  - name: dev
    work_dir: /home/dev
    container:
      image: claude-code-local:official
      mounts:
        - "~/.ssh:ro"
        - "~/.gitconfig:ro"
      volumes:
        - "claude-history:/commandhistory"
      env:
        GH_TOKEN: "my-token"
    command: [claude, --output-format, stream-json]
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        let agent = &cfg.agents[0];
        assert!(agent.container.is_some());
        let c = agent.container.as_ref().unwrap();
        assert_eq!(c.image, "claude-code-local:official");
        assert_eq!(c.mounts.len(), 2);
        assert_eq!(c.volumes.len(), 1);
        assert_eq!(c.env.get("GH_TOKEN").unwrap(), "my-token");
        assert_eq!(c.runtime, "docker");
    }

    #[test]
    fn test_workspace_config_no_container() {
        let yaml = r#"
agents:
  - name: dev
    work_dir: /home/dev
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.agents[0].container.is_none());
    }

    #[test]
    fn test_workspace_config_with_rooms() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
  - name: dev
    work_dir: /home/dev
rooms:
  - name: features
    work_dir: ~/work/project
    context: ~/work/project/CLAUDE.md
    agents: [kira]
  - name: reviews
    work_dir: ~/work/reviews
    agents: [dev]
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.rooms.len(), 2);
        assert_eq!(cfg.rooms[0].name, "features");
        assert_eq!(cfg.rooms[0].work_dir, "~/work/project");
        assert_eq!(
            cfg.rooms[0].context.as_deref(),
            Some("~/work/project/CLAUDE.md")
        );
        assert_eq!(cfg.rooms[0].agents, vec!["kira"]);
        assert_eq!(cfg.rooms[1].name, "reviews");
        assert!(cfg.rooms[1].context.is_none());
    }

    #[test]
    fn test_workspace_config_rooms_default_empty() {
        let yaml = r#"
agents:
  - name: kira
    work_dir: /home/kira
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.rooms.is_empty());
    }

    #[test]
    fn test_telegram_route_with_route_to() {
        let yaml = r#"
telegram:
  routes:
    - chat_id: -1001234567890
      name: "personal"
    - chat_id: -1001234567891
      name: "collab"
      mention_only: true
      route_to: "agent:collab"
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        let routes = cfg.telegram.unwrap().routes;
        assert_eq!(routes.len(), 2);
        assert!(routes[0].route_to.is_none());
        assert_eq!(routes[1].route_to.as_deref(), Some("agent:collab"));
        assert!(routes[1].mention_only);
    }

    #[test]
    fn test_agent_def_config_path_uses_agent_config_when_set() {
        // When an agent defines config_path in workspace.yaml, that path is used
        // for loading schedules and other agent-level config.
        let def = AgentDef {
            name: "family".into(),
            unix_user: Some("family".into()),
            work_dir: "/home/family".into(),
            config: Some("/home/family/deskd.yaml".into()),
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
            runtime: Default::default(),
        };
        assert_eq!(def.config_path(), "/home/family/deskd.yaml");
    }

    #[test]
    fn test_agent_def_config_path_defaults_to_work_dir() {
        // When config is not set, config_path defaults to {work_dir}/deskd.yaml.
        let def = AgentDef {
            name: "family".into(),
            unix_user: Some("family".into()),
            work_dir: "/home/family".into(),
            config: None,
            telegram: None,
            discord: None,
            model: None,
            command: vec!["claude".into()],
            budget_usd: 50.0,
            container: None,
            runtime: Default::default(),
        };
        assert_eq!(def.config_path(), "/home/family/deskd.yaml");
    }

    #[test]
    fn test_user_config_with_schedules() {
        // Verify that schedules defined in an agent-level deskd.yaml are parsed
        // correctly, supporting all three action types.
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Family assistant"

schedules:
  - cron: "3 7 * * *"
    target: "agent:family"
    action: raw
    config: "Morning brief"
  - cron: "3 21 * * *"
    target: "agent:family"
    action: github_poll
    config:
      repos:
        - kgatilin/deskd
      label: agent-ready
  - cron: "7 22 * * *"
    target: "agent:family"
    action: shell
    config:
      command: "echo receipts"
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.schedules.len(), 3);
        assert_eq!(cfg.schedules[0].cron, "3 7 * * *");
        assert!(matches!(cfg.schedules[0].action, ScheduleAction::Raw));
        assert!(matches!(
            cfg.schedules[1].action,
            ScheduleAction::GithubPoll
        ));
        assert!(matches!(cfg.schedules[2].action, ScheduleAction::Shell));
    }

    #[test]
    fn test_sub_agent_context_config() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"

agents:
  - name: memory-arch
    model: claude-haiku-4-5
    system_prompt: "You hold architecture context."
    subscribe:
      - "agent:memory-arch"
    context:
      enabled: true
      main_path: contexts/arch-main.yaml
      main_budget_tokens: 50000
      compact_threshold_tokens: 40000

  - name: worker
    model: claude-sonnet-4-6
    system_prompt: "Worker"
    subscribe:
      - "agent:worker"

context:
  enabled: true
  main_path: contexts/default.yaml
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        // Per-agent context on memory-arch
        let ctx = cfg.agents[0].context.as_ref().unwrap();
        assert!(ctx.enabled);
        assert_eq!(ctx.main_path.as_deref(), Some("contexts/arch-main.yaml"));
        assert_eq!(ctx.main_budget_tokens, Some(50000));
        assert_eq!(ctx.compact_threshold_tokens, Some(40000));
        // Worker has no per-agent context
        assert!(cfg.agents[1].context.is_none());
        // Global context fallback exists
        let global = cfg.context.as_ref().unwrap();
        assert!(global.enabled);
        assert_eq!(global.main_path.as_deref(), Some("contexts/default.yaml"));
    }

    #[test]
    fn test_sub_agent_memory_runtime() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"

agents:
  - name: memory-all
    model: claude-haiku-4-5
    system_prompt: "You accumulate bus events as context."
    subscribe:
      - "agent:memory-all"
      - "agent:*"
    runtime: memory
    compact_threshold: 0.75
    compact_strategy: aggressive

  - name: worker
    model: claude-sonnet-4-6
    system_prompt: "Worker"
    subscribe:
      - "agent:worker"
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].runtime, ConfigAgentRuntime::Memory);
        assert_eq!(cfg.agents[0].compact_threshold, Some(0.75));
        assert_eq!(
            cfg.agents[0].compact_strategy.as_deref(),
            Some("aggressive")
        );
        // Worker has default runtime (Claude).
        assert_eq!(cfg.agents[1].runtime, ConfigAgentRuntime::Claude);
        assert!(cfg.agents[1].compact_threshold.is_none());
        assert!(cfg.agents[1].compact_strategy.is_none());
    }

    #[test]
    fn test_workspace_agent_acp_runtime() {
        // Issue #93: top-level AgentDef must accept `runtime: acp`.
        let yaml = r#"
agents:
  - name: dev
    unix_user: dev
    work_dir: /home/dev
    runtime: acp
    command:
      - gemini
      - --acp
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].runtime, ConfigAgentRuntime::Acp);
        assert_eq!(cfg.agents[0].command, vec!["gemini", "--acp"]);
    }

    #[test]
    fn test_workspace_agent_runtime_defaults_to_claude() {
        // When `runtime` is unset, behavior must be identical to today.
        let yaml = r#"
agents:
  - name: classic
    unix_user: dev
    work_dir: /home/dev
"#;
        let cfg: WorkspaceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].runtime, ConfigAgentRuntime::Claude);
    }

    #[test]
    fn test_sub_agent_acp_runtime() {
        // Sub-agents (in deskd.yaml) also accept runtime: acp.
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "parent"

agents:
  - name: acp-child
    model: ignored
    system_prompt: "ACP sub-agent"
    subscribe:
      - "agent:acp-child"
    runtime: acp
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].runtime, ConfigAgentRuntime::Acp);
    }

    #[test]
    fn test_sub_agent_memory_defaults() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"

agents:
  - name: memory-all
    model: claude-haiku-4-5
    system_prompt: "Memory agent"
    subscribe:
      - "agent:*"
    runtime: memory
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].runtime, ConfigAgentRuntime::Memory);
        // Defaults are None — worker.rs applies the actual defaults.
        assert!(cfg.agents[0].compact_threshold.is_none());
        assert!(cfg.agents[0].compact_strategy.is_none());
    }

    #[test]
    fn test_resolve_container_profiles_string_ref() {
        let yaml = r#"
containers:
  personal:
    image: claude-code:latest
    mounts:
      - "/home/dev/.ssh:/home/dev/.ssh:ro"
    env:
      TOKEN: secret
agents:
  - name: dev
    work_dir: /home/dev
    container: personal
"#;
        let resolved = WorkspaceConfig::resolve_container_profiles(yaml).unwrap();
        let cfg: WorkspaceConfig = serde_yaml::from_str(&resolved).unwrap();
        let container = cfg.agents[0].container.as_ref().unwrap();
        assert_eq!(container.image, "claude-code:latest");
        assert_eq!(container.mounts, vec!["/home/dev/.ssh:/home/dev/.ssh:ro"]);
        assert_eq!(container.env.get("TOKEN").unwrap(), "secret");
    }

    #[test]
    fn test_resolve_container_profiles_inline_unchanged() {
        let yaml = r#"
containers:
  personal:
    image: ignored
agents:
  - name: dev
    work_dir: /home/dev
    container:
      image: inline-image
      env:
        KEY: val
"#;
        let resolved = WorkspaceConfig::resolve_container_profiles(yaml).unwrap();
        let cfg: WorkspaceConfig = serde_yaml::from_str(&resolved).unwrap();
        let container = cfg.agents[0].container.as_ref().unwrap();
        assert_eq!(container.image, "inline-image");
    }

    #[test]
    fn test_resolve_container_profiles_unknown_errors() {
        let yaml = r#"
containers:
  personal:
    image: claude-code:latest
agents:
  - name: dev
    work_dir: /home/dev
    container: nonexistent
"#;
        let result = WorkspaceConfig::resolve_container_profiles(yaml);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown container profile")
        );
    }

    #[test]
    fn test_resolve_container_profiles_no_profiles_section() {
        let yaml = r#"
agents:
  - name: dev
    work_dir: /home/dev
"#;
        let resolved = WorkspaceConfig::resolve_container_profiles(yaml).unwrap();
        let cfg: WorkspaceConfig = serde_yaml::from_str(&resolved).unwrap();
        assert!(cfg.agents[0].container.is_none());
    }

    #[test]
    fn test_resolve_container_profiles_multiple_agents() {
        let yaml = r#"
containers:
  personal:
    image: claude-personal
  work:
    image: claude-work
    env:
      API_KEY: work-key
agents:
  - name: dev
    work_dir: /home/dev
    container: personal
  - name: ops
    work_dir: /home/ops
    container: work
  - name: bare
    work_dir: /home/bare
"#;
        let resolved = WorkspaceConfig::resolve_container_profiles(yaml).unwrap();
        let cfg: WorkspaceConfig = serde_yaml::from_str(&resolved).unwrap();
        assert_eq!(
            cfg.agents[0].container.as_ref().unwrap().image,
            "claude-personal"
        );
        assert_eq!(
            cfg.agents[1].container.as_ref().unwrap().image,
            "claude-work"
        );
        assert_eq!(
            cfg.agents[1]
                .container
                .as_ref()
                .unwrap()
                .env
                .get("API_KEY")
                .unwrap(),
            "work-key"
        );
        assert!(cfg.agents[2].container.is_none());
    }

    // ─── Scope tests ────────────────────────────────────────────────────────

    #[test]
    fn test_scope_type_defaults_to_inherit() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"
agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe: ["agent:worker"]
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].scope, ScopeType::Inherit);
    }

    #[test]
    fn test_scope_type_narrow() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"
agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe: ["agent:worker"]
    scope: narrow
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].scope, ScopeType::Narrow);
    }

    #[test]
    fn test_can_message_config() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"
agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe: ["agent:worker"]
    can_message: ["agent:parent", "telegram.out:*"]
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        let cm = cfg.agents[0].can_message.as_ref().unwrap();
        assert_eq!(cm, &["agent:parent", "telegram.out:*"]);
    }

    #[test]
    fn test_can_message_default_unrestricted() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"
agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe: ["agent:worker"]
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.agents[0].can_message.is_none());
    }

    #[test]
    fn test_sub_agent_work_dir_config() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"
agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe: ["agent:worker"]
    work_dir: /home/dev/tasks/abc
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            cfg.agents[0].work_dir.as_deref(),
            Some("/home/dev/tasks/abc")
        );
    }

    #[test]
    fn test_sub_agent_env_config() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"
agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe: ["agent:worker"]
    env:
      ANTHROPIC_API_KEY: sk-worker-key
      CUSTOM_VAR: hello
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        let env = cfg.agents[0].env.as_ref().unwrap();
        assert_eq!(env.get("ANTHROPIC_API_KEY").unwrap(), "sk-worker-key");
        assert_eq!(env.get("CUSTOM_VAR").unwrap(), "hello");
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn test_scoped_name() {
        let yaml = r#"
model: claude-sonnet-4-6
system_prompt: "Test"
agents:
  - name: worker
    model: claude-haiku-4-5
    system_prompt: "Worker"
    subscribe: ["agent:worker"]
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.agents[0].scoped_name("dev"), "dev/worker");
    }

    #[test]
    fn test_validate_work_dir_within_parent() {
        let sub = SubAgentDef {
            name: "worker".into(),
            model: "haiku".into(),
            system_prompt: String::new(),
            subscribe: vec![],
            publish: None,
            inbox_read: None,
            scope: ScopeType::Narrow,
            can_message: None,
            work_dir: Some("/tmp/parent/child".into()),
            env: None,
            session: ConfigSessionMode::default(),
            runtime: ConfigAgentRuntime::default(),
            context: None,
            compact_threshold: None,
            compact_strategy: None,
            auto_compact_threshold_tokens: None,
        };
        assert!(sub.validate_work_dir("/tmp/parent").is_ok());
    }

    #[test]
    fn test_validate_work_dir_outside_parent_fails() {
        let sub = SubAgentDef {
            name: "worker".into(),
            model: "haiku".into(),
            system_prompt: String::new(),
            subscribe: vec![],
            publish: None,
            inbox_read: None,
            scope: ScopeType::Narrow,
            can_message: None,
            work_dir: Some("/etc/evil".into()),
            env: None,
            session: ConfigSessionMode::default(),
            runtime: ConfigAgentRuntime::default(),
            context: None,
            compact_threshold: None,
            compact_strategy: None,
            auto_compact_threshold_tokens: None,
        };
        assert!(sub.validate_work_dir("/tmp/parent").is_err());
    }
}
