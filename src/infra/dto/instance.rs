//! State machine instance storage DTOs and domain conversions.

use serde::{Deserialize, Serialize};

use crate::domain::statemachine::{Instance, Transition};

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
    /// Task IDs owned by this instance.
    #[serde(default)]
    pub task_ids: Vec<String>,
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
    #[serde(default)]
    pub task_id: Option<String>,
}

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
            task_ids: dto.task_ids,
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
            task_ids: inst.task_ids.clone(),
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
            task_id: dto.task_id,
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
            task_id: t.task_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                task_id: None,
            }],
            metadata: serde_json::json!({}),
            total_cost: 0.0,
            total_turns: 0,
            task_ids: vec!["task-abc123".into()],
        };
        let stored: StoredInstance = (&inst).into();
        let restored: Instance = stored.into();
        assert_eq!(restored.id, "sm-1");
        assert_eq!(restored.state, "open");
        assert_eq!(restored.history.len(), 1);
        assert_eq!(restored.history[0].from, "new");
    }
}
