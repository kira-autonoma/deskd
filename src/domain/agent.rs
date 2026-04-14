//! Agent domain types — pure enums and structs, no I/O.
//! Serde lives on config DTOs (infra::dto), not here.

/// Session mode for an agent: persistent (default) or ephemeral.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionMode {
    /// Continue existing session across tasks (uses --resume).
    #[default]
    Persistent,
    /// Start a fresh session for each task (no --resume).
    Ephemeral,
}

/// Agent runtime protocol: claude (default), acp, or memory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentRuntime {
    /// Claude stream-json protocol (default).
    #[default]
    Claude,
    /// Agent Client Protocol (ACP) — JSON-RPC 2.0 over stdin/stdout.
    Acp,
    /// Memory agent — Claude executor with bus event accumulation.
    /// Injects bus events as context into a long-running conversation.
    /// Direct questions (target == agent:{name}) go through send_task().
    Memory,
}

/// Domain-level representation of an agent with capabilities and status.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub name: String,
    pub runtime: AgentRuntime,
    pub session_mode: SessionMode,
    pub capabilities: AgentCapabilities,
    pub status: AgentStatus,
}

/// What an agent can do — model and labels for routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub model: String,
    pub labels: Vec<String>,
}

/// Current status of an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    /// Agent is idle and ready for work.
    Ready,
    /// Agent is executing a task.
    Busy { task_id: String },
    /// Agent process is dead or unreachable.
    Unhealthy { since: String, reason: String },
}

impl AgentStatus {
    /// Short display label.
    pub fn label(&self) -> &str {
        match self {
            AgentStatus::Ready => "ready",
            AgentStatus::Busy { .. } => "busy",
            AgentStatus::Unhealthy { .. } => "unhealthy",
        }
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentStatus::Ready => write!(f, "ready"),
            AgentStatus::Busy { task_id } => write!(f, "busy ({})", task_id),
            AgentStatus::Unhealthy { reason, .. } => write!(f, "unhealthy: {}", reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_status_labels() {
        assert_eq!(AgentStatus::Ready.label(), "ready");
        assert_eq!(
            AgentStatus::Busy {
                task_id: "t1".into()
            }
            .label(),
            "busy"
        );
        assert_eq!(
            AgentStatus::Unhealthy {
                since: "2026-01-01".into(),
                reason: "process dead".into()
            }
            .label(),
            "unhealthy"
        );
    }

    #[test]
    fn agent_status_display() {
        assert_eq!(format!("{}", AgentStatus::Ready), "ready");
        assert_eq!(
            format!(
                "{}",
                AgentStatus::Busy {
                    task_id: "task-42".into()
                }
            ),
            "busy (task-42)"
        );
        assert_eq!(
            format!(
                "{}",
                AgentStatus::Unhealthy {
                    since: "2026-01-01".into(),
                    reason: "pid gone".into()
                }
            ),
            "unhealthy: pid gone"
        );
    }

    #[test]
    fn agent_entity_construction() {
        let agent = Agent {
            name: "dev".into(),
            runtime: AgentRuntime::Claude,
            session_mode: SessionMode::Persistent,
            capabilities: AgentCapabilities {
                model: "claude-sonnet-4-6".into(),
                labels: vec!["coding".into(), "review".into()],
            },
            status: AgentStatus::Ready,
        };
        assert_eq!(agent.name, "dev");
        assert_eq!(agent.capabilities.model, "claude-sonnet-4-6");
        assert_eq!(agent.capabilities.labels.len(), 2);
        assert_eq!(agent.status.label(), "ready");
    }

    #[test]
    fn agent_busy_status_carries_task_id() {
        let status = AgentStatus::Busy {
            task_id: "abc-123".into(),
        };
        assert_eq!(status.label(), "busy");
        if let AgentStatus::Busy { task_id } = &status {
            assert_eq!(task_id, "abc-123");
        } else {
            panic!("expected Busy");
        }
    }

    #[test]
    fn agent_unhealthy_carries_reason() {
        let status = AgentStatus::Unhealthy {
            since: "2026-04-06T00:00:00Z".into(),
            reason: "process exited".into(),
        };
        assert_eq!(status.label(), "unhealthy");
        assert!(format!("{status}").contains("process exited"));
    }

    #[test]
    fn default_session_mode_is_persistent() {
        assert_eq!(SessionMode::default(), SessionMode::Persistent);
    }

    #[test]
    fn default_runtime_is_claude() {
        assert_eq!(AgentRuntime::default(), AgentRuntime::Claude);
    }

    #[test]
    fn memory_runtime_variant_exists() {
        let rt = AgentRuntime::Memory;
        assert_ne!(rt, AgentRuntime::Claude);
        assert_ne!(rt, AgentRuntime::Acp);
    }
}
