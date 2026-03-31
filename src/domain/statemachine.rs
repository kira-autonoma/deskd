//! State machine domain types.
//!
//! Pure data types — no I/O, no persistence logic.

use serde::{Deserialize, Serialize};

/// A state machine model definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub states: Vec<String>,
    pub initial: String,
    #[serde(default)]
    pub terminal: Vec<String>,
    pub transitions: Vec<TransitionDef>,
}

/// A transition between states in a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionDef {
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
    pub notify: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub timeout_goto: Option<String>,
    /// Task queue criteria for this transition (model, labels).
    /// When set, dispatch creates a task in the queue instead of direct bus message.
    #[serde(default)]
    pub criteria: Option<crate::domain::task::TaskCriteria>,
}

/// An instance of a state machine model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
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
    pub history: Vec<Transition>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A recorded transition in the instance history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub from: String,
    pub to: String,
    pub trigger: String,
    pub timestamp: String,
    #[serde(default)]
    pub note: Option<String>,
}
