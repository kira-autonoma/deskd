//! Context configuration and persistence DTOs.

use serde::{Deserialize, Serialize};

use crate::domain::context::{CachedResult, MainBranch, MaterializedMessage, Node, NodeKind};

// Re-export config type from domain.
pub use crate::domain::config_types::ConfigContextConfig;

// ─── Persistence format ─────────────────────────────────────────────────────

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
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
            tags: dto.tags,
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
            tags: n.tags.clone(),
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
