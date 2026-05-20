//! Configuration value types — serde-enabled enums for YAML parsing.
//!
//! These types are used across layers (config, app, infra) so they live
//! in the domain layer as shared vocabulary types. Conversion impls
//! to/from domain enums live alongside these types.

use serde::{Deserialize, Serialize};

use super::agent::{AgentKind, AgentRuntime, SessionMode};
use super::context::ContextConfig;

// ─── SessionMode / AgentRuntime ─────────────────────────────────────────────

/// Config-level session mode (serde for YAML parsing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSessionMode {
    #[default]
    Persistent,
    Ephemeral,
}

/// Config-level agent runtime (serde for YAML parsing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConfigAgentRuntime {
    #[default]
    Claude,
    Acp,
    Memory,
}

/// How the agent process is launched (#452).
///
/// `Subprocess` is the historical behaviour — `deskd serve` spawns a child
/// process and supervises it directly. `Tmux` (opt-in) launches the agent's
/// Claude REPL inside a detached tmux session named `deskd-<agent>`, so the
/// session survives operator disconnects and can be `tmux attach`'d for
/// inspection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConfigLaunchMode {
    #[default]
    Subprocess,
    Tmux,
}

impl From<ConfigSessionMode> for SessionMode {
    fn from(dto: ConfigSessionMode) -> Self {
        match dto {
            ConfigSessionMode::Persistent => SessionMode::Persistent,
            ConfigSessionMode::Ephemeral => SessionMode::Ephemeral,
        }
    }
}

impl From<&SessionMode> for ConfigSessionMode {
    fn from(mode: &SessionMode) -> Self {
        match mode {
            SessionMode::Persistent => ConfigSessionMode::Persistent,
            SessionMode::Ephemeral => ConfigSessionMode::Ephemeral,
        }
    }
}

impl From<ConfigAgentRuntime> for AgentRuntime {
    fn from(dto: ConfigAgentRuntime) -> Self {
        match dto {
            ConfigAgentRuntime::Claude => AgentRuntime::Claude,
            ConfigAgentRuntime::Acp => AgentRuntime::Acp,
            ConfigAgentRuntime::Memory => AgentRuntime::Memory,
        }
    }
}

impl From<&AgentRuntime> for ConfigAgentRuntime {
    fn from(rt: &AgentRuntime) -> Self {
        match rt {
            AgentRuntime::Claude => ConfigAgentRuntime::Claude,
            AgentRuntime::Acp => ConfigAgentRuntime::Acp,
            AgentRuntime::Memory => ConfigAgentRuntime::Memory,
        }
    }
}

// ─── AgentKind ──────────────────────────────────────────────────────────────

/// Config-level agent kind (serde for YAML parsing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConfigAgentKind {
    #[default]
    Executor,
    Context,
}

impl From<ConfigAgentKind> for AgentKind {
    fn from(dto: ConfigAgentKind) -> Self {
        match dto {
            ConfigAgentKind::Executor => AgentKind::Executor,
            ConfigAgentKind::Context => AgentKind::Context,
        }
    }
}

impl From<&AgentKind> for ConfigAgentKind {
    fn from(k: &AgentKind) -> Self {
        match k {
            AgentKind::Executor => ConfigAgentKind::Executor,
            AgentKind::Context => ConfigAgentKind::Context,
        }
    }
}

// ─── ContextConfig ─────────────────────────────────────────────────────────

/// Config-level context configuration (serde for YAML parsing).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ConfigContextConfig {
    pub enabled: bool,
    pub main_budget_tokens: Option<u32>,
    pub compact_threshold_tokens: Option<u32>,
    pub main_path: Option<String>,
}

impl From<ConfigContextConfig> for ContextConfig {
    fn from(dto: ConfigContextConfig) -> Self {
        Self {
            enabled: dto.enabled,
            main_budget_tokens: dto.main_budget_tokens,
            compact_threshold_tokens: dto.compact_threshold_tokens,
            main_path: dto.main_path,
        }
    }
}

impl From<&ContextConfig> for ConfigContextConfig {
    fn from(c: &ContextConfig) -> Self {
        Self {
            enabled: c.enabled,
            main_budget_tokens: c.main_budget_tokens,
            compact_threshold_tokens: c.compact_threshold_tokens,
            main_path: c.main_path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_agent_runtime_memory_roundtrip() {
        let config_rt = ConfigAgentRuntime::Memory;
        let domain_rt: AgentRuntime = config_rt.into();
        assert_eq!(domain_rt, AgentRuntime::Memory);
        let back: ConfigAgentRuntime = (&domain_rt).into();
        assert_eq!(back, ConfigAgentRuntime::Memory);
    }

    #[test]
    fn config_agent_runtime_serde() {
        let yaml = "memory";
        let rt: ConfigAgentRuntime = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(rt, ConfigAgentRuntime::Memory);
    }

    #[test]
    fn config_agent_kind_default_is_executor() {
        assert_eq!(ConfigAgentKind::default(), ConfigAgentKind::Executor);
    }

    #[test]
    fn config_agent_kind_roundtrip() {
        let dto = ConfigAgentKind::Context;
        let domain: AgentKind = dto.into();
        assert_eq!(domain, AgentKind::Context);
        let back: ConfigAgentKind = (&domain).into();
        assert_eq!(back, ConfigAgentKind::Context);
    }

    #[test]
    fn config_agent_kind_serde() {
        let executor: ConfigAgentKind = serde_yaml::from_str("executor").unwrap();
        assert_eq!(executor, ConfigAgentKind::Executor);
        let context: ConfigAgentKind = serde_yaml::from_str("context").unwrap();
        assert_eq!(context, ConfigAgentKind::Context);
    }
}
