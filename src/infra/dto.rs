//! Infrastructure DTOs — wire and storage formats.
//!
//! These types own their serde derives and decouple the infra layer
//! from domain types. Conversion happens at port boundaries via From/Into.

use serde::{Deserialize, Serialize};
use tracing::warn;

// ─── Bus wire format ─────────────────────────────────────────────────────────

/// Wire format for messages on the Unix socket bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub id: String,
    pub source: String,
    pub target: String,
    pub payload: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub metadata: BusMetadata,
}

/// Wire format for message metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BusMetadata {
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default)]
    pub fresh: bool,
}

fn default_priority() -> u8 {
    5
}

/// Wire format for the registration handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusRegister {
    pub name: String,
    #[serde(default)]
    pub subscriptions: Vec<String>,
}

/// Wire-level envelope: tagged union for all bus protocol messages.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BusEnvelope {
    Register(BusRegister),
    Message(BusMessage),
    List,
}

// ─── Domain ↔ Bus conversions ────────────────────────────────────────────────

use crate::domain::message::{Envelope, Message, Metadata, Register};

impl From<BusMessage> for Message {
    fn from(dto: BusMessage) -> Self {
        Self {
            id: dto.id,
            source: dto.source,
            target: dto.target,
            payload: dto.payload,
            reply_to: dto.reply_to,
            metadata: Metadata {
                priority: dto.metadata.priority,
                fresh: dto.metadata.fresh,
            },
        }
    }
}

impl From<&Message> for BusMessage {
    fn from(msg: &Message) -> Self {
        Self {
            id: msg.id.clone(),
            source: msg.source.clone(),
            target: msg.target.clone(),
            payload: msg.payload.clone(),
            reply_to: msg.reply_to.clone(),
            metadata: BusMetadata {
                priority: msg.metadata.priority,
                fresh: msg.metadata.fresh,
            },
        }
    }
}

impl From<BusEnvelope> for Envelope {
    fn from(dto: BusEnvelope) -> Self {
        match dto {
            BusEnvelope::Register(r) => Envelope::Register(Register {
                name: r.name,
                subscriptions: r.subscriptions,
            }),
            BusEnvelope::Message(m) => Envelope::Message(m.into()),
            BusEnvelope::List => Envelope::List,
        }
    }
}

// ─── Store format: Task ──────────────────────────────────────────────────────

/// Storage format for tasks (JSON file on disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTask {
    pub id: String,
    pub description: String,
    pub status: String,
    pub assignee: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub criteria: StoredTaskCriteria,
    #[serde(default)]
    pub sm_instance_id: Option<String>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub turns: Option<u32>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub timed_out_at: Option<String>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub retry_after: Option<String>,
}

/// Storage format for task matching criteria.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredTaskCriteria {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

use crate::domain::task::{Task, TaskCriteria, TaskStatus};

impl From<StoredTask> for Task {
    fn from(dto: StoredTask) -> Self {
        Self {
            id: dto.id,
            description: dto.description,
            status: match dto.status.as_str() {
                "pending" => TaskStatus::Pending,
                "active" => TaskStatus::Active,
                "done" => TaskStatus::Done,
                "failed" => TaskStatus::Failed,
                "cancelled" => TaskStatus::Cancelled,
                "dead_letter" => TaskStatus::DeadLetter,
                other => {
                    warn!(status = other, "unknown task status, defaulting to Pending");
                    TaskStatus::Pending
                }
            },
            assignee: dto.assignee,
            result: dto.result,
            error: dto.error,
            created_by: dto.created_by,
            created_at: dto.created_at,
            updated_at: dto.updated_at,
            criteria: TaskCriteria {
                model: dto.criteria.model,
                labels: dto.criteria.labels,
            },
            sm_instance_id: dto.sm_instance_id,
            cost_usd: dto.cost_usd,
            turns: dto.turns,
            metadata: dto.metadata,
            timed_out_at: dto.timed_out_at,
            attempt: dto.attempt,
            max_retries: dto.max_retries,
            retry_after: dto.retry_after,
        }
    }
}

impl From<&Task> for StoredTask {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            description: task.description.clone(),
            status: match task.status {
                TaskStatus::Pending => "pending",
                TaskStatus::Active => "active",
                TaskStatus::Done => "done",
                TaskStatus::Failed => "failed",
                TaskStatus::Cancelled => "cancelled",
                TaskStatus::DeadLetter => "dead_letter",
            }
            .to_string(),
            assignee: task.assignee.clone(),
            result: task.result.clone(),
            error: task.error.clone(),
            created_by: task.created_by.clone(),
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
            criteria: StoredTaskCriteria {
                model: task.criteria.model.clone(),
                labels: task.criteria.labels.clone(),
            },
            sm_instance_id: task.sm_instance_id.clone(),
            cost_usd: task.cost_usd,
            turns: task.turns,
            metadata: task.metadata.clone(),
            timed_out_at: task.timed_out_at.clone(),
            attempt: task.attempt,
            max_retries: task.max_retries,
            retry_after: task.retry_after.clone(),
        }
    }
}

// ─── Store format: StateMachine Instance ─────────────────────────────────────

/// Storage format for state machine instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredInstance {
    pub id: String,
    pub model: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub state: String,
    pub assignee: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub history: Vec<StoredTransition>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub total_cost: f64,
    #[serde(default)]
    pub total_turns: u32,
}

/// Storage format for a recorded state transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTransition {
    pub from: String,
    pub to: String,
    pub trigger: String,
    pub timestamp: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub turns: Option<u32>,
}

use crate::domain::statemachine::{Instance, Transition};

impl From<StoredInstance> for Instance {
    fn from(dto: StoredInstance) -> Self {
        Self {
            id: dto.id,
            model: dto.model,
            title: dto.title,
            body: dto.body,
            state: dto.state,
            assignee: dto.assignee,
            result: dto.result,
            error: dto.error,
            created_by: dto.created_by,
            created_at: dto.created_at,
            updated_at: dto.updated_at,
            history: dto.history.into_iter().map(Into::into).collect(),
            metadata: dto.metadata,
            total_cost: dto.total_cost,
            total_turns: dto.total_turns,
        }
    }
}

impl From<&Instance> for StoredInstance {
    fn from(inst: &Instance) -> Self {
        Self {
            id: inst.id.clone(),
            model: inst.model.clone(),
            title: inst.title.clone(),
            body: inst.body.clone(),
            state: inst.state.clone(),
            assignee: inst.assignee.clone(),
            result: inst.result.clone(),
            error: inst.error.clone(),
            created_by: inst.created_by.clone(),
            created_at: inst.created_at.clone(),
            updated_at: inst.updated_at.clone(),
            history: inst.history.iter().map(Into::into).collect(),
            metadata: inst.metadata.clone(),
            total_cost: inst.total_cost,
            total_turns: inst.total_turns,
        }
    }
}

impl From<StoredTransition> for Transition {
    fn from(dto: StoredTransition) -> Self {
        Self {
            from: dto.from,
            to: dto.to,
            trigger: dto.trigger,
            timestamp: dto.timestamp,
            note: dto.note,
            cost_usd: dto.cost_usd,
            turns: dto.turns,
        }
    }
}

impl From<&Transition> for StoredTransition {
    fn from(t: &Transition) -> Self {
        Self {
            from: t.from.clone(),
            to: t.to.clone(),
            trigger: t.trigger.clone(),
            timestamp: t.timestamp.clone(),
            note: t.note.clone(),
            cost_usd: t.cost_usd,
            turns: t.turns,
        }
    }
}

// ─── Config format: SessionMode / AgentRuntime ──────────────────────────────

use crate::domain::agent::{AgentRuntime, SessionMode};

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
        }
    }
}

impl From<&AgentRuntime> for ConfigAgentRuntime {
    fn from(rt: &AgentRuntime) -> Self {
        match rt {
            AgentRuntime::Claude => ConfigAgentRuntime::Claude,
            AgentRuntime::Acp => ConfigAgentRuntime::Acp,
        }
    }
}

// ─── Config format: ModelDef / TransitionDef ────────────────────────────────

use crate::domain::statemachine::{ModelDef, TransitionDef};

/// Config-level state machine model definition (serde for YAML parsing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigModelDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub states: Vec<String>,
    pub initial: String,
    #[serde(default)]
    pub terminal: Vec<String>,
    pub transitions: Vec<ConfigTransitionDef>,
}

/// Config-level transition definition (serde for YAML parsing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigTransitionDef {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub on: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(rename = "type", default)]
    pub step_type: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub notify: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub timeout_goto: Option<String>,
    #[serde(default)]
    pub criteria: Option<ConfigTaskCriteria>,
    #[serde(default)]
    pub max_retries: u32,
}

/// Config-level task criteria (serde for YAML parsing).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigTaskCriteria {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

impl TryFrom<ConfigModelDef> for ModelDef {
    type Error = String;

    fn try_from(dto: ConfigModelDef) -> Result<Self, Self::Error> {
        let transitions: Result<Vec<_>, _> =
            dto.transitions.into_iter().map(TryInto::try_into).collect();
        Ok(Self {
            name: dto.name,
            description: dto.description,
            states: dto.states,
            initial: dto.initial,
            terminal: dto.terminal,
            transitions: transitions?,
        })
    }
}

impl From<&ModelDef> for ConfigModelDef {
    fn from(m: &ModelDef) -> Self {
        Self {
            name: m.name.clone(),
            description: m.description.clone(),
            states: m.states.clone(),
            initial: m.initial.clone(),
            terminal: m.terminal.clone(),
            transitions: m.transitions.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<ConfigTransitionDef> for TransitionDef {
    type Error = String;

    fn try_from(dto: ConfigTransitionDef) -> Result<Self, Self::Error> {
        use crate::domain::statemachine::StepType;
        let step_type = match dto.step_type.as_deref() {
            Some(s) => StepType::parse(s)?,
            None => StepType::default(),
        };
        Ok(Self {
            from: dto.from,
            to: dto.to,
            trigger: dto.trigger,
            on: dto.on,
            assignee: dto.assignee,
            prompt: dto.prompt,
            step_type,
            command: dto.command,
            notify: dto.notify,
            timeout: dto.timeout,
            timeout_goto: dto.timeout_goto,
            criteria: dto.criteria.map(Into::into),
            max_retries: dto.max_retries,
        })
    }
}

impl From<&TransitionDef> for ConfigTransitionDef {
    fn from(t: &TransitionDef) -> Self {
        use crate::domain::statemachine::StepType;
        let step_type = if t.step_type == StepType::Agent {
            None // omit default value
        } else {
            Some(t.step_type.as_str().to_string())
        };
        Self {
            from: t.from.clone(),
            to: t.to.clone(),
            trigger: t.trigger.clone(),
            on: t.on.clone(),
            assignee: t.assignee.clone(),
            prompt: t.prompt.clone(),
            step_type,
            command: t.command.clone(),
            notify: t.notify.clone(),
            timeout: t.timeout.clone(),
            timeout_goto: t.timeout_goto.clone(),
            criteria: t.criteria.as_ref().map(Into::into),
            max_retries: t.max_retries,
        }
    }
}

impl From<ConfigTaskCriteria> for TaskCriteria {
    fn from(dto: ConfigTaskCriteria) -> Self {
        Self {
            model: dto.model,
            labels: dto.labels,
        }
    }
}

impl From<&TaskCriteria> for ConfigTaskCriteria {
    fn from(c: &TaskCriteria) -> Self {
        Self {
            model: c.model.clone(),
            labels: c.labels.clone(),
        }
    }
}

// ─── Config format: ContextConfig ───────────────────────────────────────────

use crate::domain::context::{
    CachedResult, ContextConfig, MainBranch, MaterializedMessage, Node, NodeKind,
};

/// Config-level context configuration (serde for YAML parsing).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

// ─── Persistence format: MainBranch / Node / NodeKind ───────────────────────

/// Storage format for the main context branch (YAML on disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMainBranch {
    pub agent: String,
    pub budget_tokens: u32,
    pub nodes: Vec<StoredNode>,
}

/// Storage format for a context node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredNode {
    pub id: String,
    pub kind: StoredNodeKind,
    pub label: String,
    pub tokens_estimate: u32,
}

/// Storage format for node kinds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StoredNodeKind {
    Static {
        role: String,
        content: String,
    },
    Live {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        max_age_secs: Option<u64>,
        inject_as: String,
    },
}

/// Storage format for cached command results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCachedResult {
    pub content: String,
    pub fetched_at: String,
}

/// Storage format for materialized messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMaterializedMessage {
    pub role: String,
    pub content: String,
}

impl From<StoredMainBranch> for MainBranch {
    fn from(dto: StoredMainBranch) -> Self {
        Self {
            agent: dto.agent,
            budget_tokens: dto.budget_tokens,
            nodes: dto.nodes.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<&MainBranch> for StoredMainBranch {
    fn from(b: &MainBranch) -> Self {
        Self {
            agent: b.agent.clone(),
            budget_tokens: b.budget_tokens,
            nodes: b.nodes.iter().map(Into::into).collect(),
        }
    }
}

impl From<StoredNode> for Node {
    fn from(dto: StoredNode) -> Self {
        Self {
            id: dto.id,
            kind: dto.kind.into(),
            label: dto.label,
            tokens_estimate: dto.tokens_estimate,
        }
    }
}

impl From<&Node> for StoredNode {
    fn from(n: &Node) -> Self {
        Self {
            id: n.id.clone(),
            kind: (&n.kind).into(),
            label: n.label.clone(),
            tokens_estimate: n.tokens_estimate,
        }
    }
}

impl From<StoredNodeKind> for NodeKind {
    fn from(dto: StoredNodeKind) -> Self {
        match dto {
            StoredNodeKind::Static { role, content } => NodeKind::Static { role, content },
            StoredNodeKind::Live {
                command,
                args,
                max_age_secs,
                inject_as,
            } => NodeKind::Live {
                command,
                args,
                max_age_secs,
                inject_as,
                cached_result: None,
            },
        }
    }
}

impl From<&NodeKind> for StoredNodeKind {
    fn from(nk: &NodeKind) -> Self {
        match nk {
            NodeKind::Static { role, content } => StoredNodeKind::Static {
                role: role.clone(),
                content: content.clone(),
            },
            NodeKind::Live {
                command,
                args,
                max_age_secs,
                inject_as,
                ..
            } => StoredNodeKind::Live {
                command: command.clone(),
                args: args.clone(),
                max_age_secs: *max_age_secs,
                inject_as: inject_as.clone(),
            },
        }
    }
}

impl From<StoredCachedResult> for CachedResult {
    fn from(dto: StoredCachedResult) -> Self {
        Self {
            content: dto.content,
            fetched_at: dto.fetched_at,
        }
    }
}

impl From<&CachedResult> for StoredCachedResult {
    fn from(c: &CachedResult) -> Self {
        Self {
            content: c.content.clone(),
            fetched_at: c.fetched_at.clone(),
        }
    }
}

impl From<StoredMaterializedMessage> for MaterializedMessage {
    fn from(dto: StoredMaterializedMessage) -> Self {
        Self {
            role: dto.role,
            content: dto.content,
        }
    }
}

impl From<&MaterializedMessage> for StoredMaterializedMessage {
    fn from(m: &MaterializedMessage) -> Self {
        Self {
            role: m.role.clone(),
            content: m.content.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bus_message_to_domain_roundtrip() {
        let wire = BusMessage {
            id: "msg-1".into(),
            source: "cli".into(),
            target: "agent:dev".into(),
            payload: serde_json::json!({"task": "hello"}),
            reply_to: Some("cli".into()),
            metadata: BusMetadata {
                priority: 3,
                fresh: true,
            },
        };
        let domain: Message = wire.clone().into();
        assert_eq!(domain.id, "msg-1");
        assert_eq!(domain.source, "cli");
        assert_eq!(domain.metadata.priority, 3);
        assert!(domain.metadata.fresh);

        let back: BusMessage = (&domain).into();
        assert_eq!(back.id, wire.id);
        assert_eq!(back.reply_to, wire.reply_to);
    }

    #[test]
    fn test_bus_envelope_deserialize_message() {
        let json = r#"{"type":"message","id":"1","source":"a","target":"b","payload":{}}"#;
        let env: BusEnvelope = serde_json::from_str(json).unwrap();
        let domain: Envelope = env.into();
        match domain {
            Envelope::Message(m) => assert_eq!(m.source, "a"),
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn test_bus_envelope_deserialize_register() {
        let json = r#"{"type":"register","name":"cli","subscriptions":["agent:cli"]}"#;
        let env: BusEnvelope = serde_json::from_str(json).unwrap();
        let domain: Envelope = env.into();
        match domain {
            Envelope::Register(r) => assert_eq!(r.name, "cli"),
            _ => panic!("expected Register"),
        }
    }

    #[test]
    fn test_stored_task_roundtrip() {
        let task = Task {
            id: "task-1".into(),
            description: "test task".into(),
            status: TaskStatus::Active,
            assignee: Some("dev".into()),
            result: None,
            error: None,
            created_by: "cli".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            criteria: TaskCriteria {
                model: Some("claude-sonnet-4-6".into()),
                labels: vec!["rust".into()],
            },
            sm_instance_id: None,
            cost_usd: None,
            turns: None,
            metadata: serde_json::Value::Null,
            timed_out_at: None,
            attempt: 0,
            max_retries: 0,
            retry_after: None,
        };
        let stored: StoredTask = (&task).into();
        assert_eq!(stored.status, "active");
        let restored: Task = stored.into();
        assert_eq!(restored.status, TaskStatus::Active);
        assert_eq!(restored.id, "task-1");
        assert_eq!(
            restored.criteria.model.as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn test_stored_instance_roundtrip() {
        let inst = Instance {
            id: "sm-1".into(),
            model: "review".into(),
            title: "PR review".into(),
            body: String::new(),
            state: "open".into(),
            assignee: "dev".into(),
            result: None,
            error: None,
            created_by: "cli".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            history: vec![Transition {
                from: "new".into(),
                to: "open".into(),
                trigger: "create".into(),
                timestamp: "2026-01-01T00:00:00Z".into(),
                note: None,
                cost_usd: None,
                turns: None,
            }],
            metadata: serde_json::json!({}),
            total_cost: 0.0,
            total_turns: 0,
        };
        let stored: StoredInstance = (&inst).into();
        let restored: Instance = stored.into();
        assert_eq!(restored.id, "sm-1");
        assert_eq!(restored.state, "open");
        assert_eq!(restored.history.len(), 1);
        assert_eq!(restored.history[0].from, "new");
    }

    #[test]
    fn test_invalid_step_type_errors_at_parse() {
        let dto = ConfigTransitionDef {
            from: "a".into(),
            to: "b".into(),
            trigger: None,
            on: None,
            assignee: None,
            prompt: None,
            step_type: Some("chekc".into()),
            command: None,
            notify: None,
            timeout: None,
            timeout_goto: None,
            criteria: None,
            max_retries: 0,
        };
        let result: Result<TransitionDef, String> = dto.try_into();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown step_type"));
    }

    #[test]
    fn test_valid_step_type_parses() {
        for (input, expected) in [
            ("check", crate::domain::statemachine::StepType::Check),
            ("human", crate::domain::statemachine::StepType::Human),
        ] {
            let dto = ConfigTransitionDef {
                from: "a".into(),
                to: "b".into(),
                trigger: None,
                on: None,
                assignee: None,
                prompt: None,
                step_type: Some(input.into()),
                command: None,
                notify: None,
                timeout: None,
                timeout_goto: None,
                criteria: None,
                max_retries: 0,
            };
            let td: TransitionDef = dto.try_into().unwrap();
            assert_eq!(td.step_type, expected);
        }
    }

    #[test]
    fn test_omitted_step_type_defaults_to_agent() {
        let dto = ConfigTransitionDef {
            from: "a".into(),
            to: "b".into(),
            trigger: None,
            on: None,
            assignee: None,
            prompt: None,
            step_type: None,
            command: None,
            notify: None,
            timeout: None,
            timeout_goto: None,
            criteria: None,
            max_retries: 0,
        };
        let td: TransitionDef = dto.try_into().unwrap();
        assert_eq!(td.step_type, crate::domain::statemachine::StepType::Agent);
    }
}
