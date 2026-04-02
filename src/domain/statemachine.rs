//! State machine domain types.
//!
//! Pure data types — no I/O, no persistence logic.
//! Serde lives on infra DTOs (infra::dto), not here.

/// A state machine model definition.
#[derive(Debug, Clone)]
pub struct ModelDef {
    pub name: String,
    pub description: String,
    pub states: Vec<String>,
    pub initial: String,
    pub terminal: Vec<String>,
    pub transitions: Vec<TransitionDef>,
}

/// A transition between states in a model.
#[derive(Debug, Clone)]
pub struct TransitionDef {
    pub from: String,
    pub to: String,
    pub trigger: Option<String>,
    pub on: Option<String>,
    pub assignee: Option<String>,
    pub prompt: Option<String>,
    pub step_type: Option<String>,
    pub notify: Option<String>,
    pub timeout: Option<String>,
    pub timeout_goto: Option<String>,
    /// Task queue criteria for this transition (model, labels).
    /// When set, dispatch creates a task in the queue instead of direct bus message.
    pub criteria: Option<crate::domain::task::TaskCriteria>,
}

/// An instance of a state machine model.
#[derive(Debug, Clone)]
pub struct Instance {
    pub id: String,
    pub model: String,
    pub title: String,
    pub body: String,
    pub state: String,
    pub assignee: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub history: Vec<Transition>,
    pub metadata: serde_json::Value,
    /// Cumulative cost across all transitions.
    pub total_cost: f64,
    /// Cumulative turns across all transitions.
    pub total_turns: u32,
}

/// A recorded transition in the instance history.
#[derive(Debug, Clone)]
pub struct Transition {
    pub from: String,
    pub to: String,
    pub trigger: String,
    pub timestamp: String,
    pub note: Option<String>,
    /// Cost in USD for the step that triggered this transition.
    pub cost_usd: Option<f64>,
    /// Number of turns for the step.
    pub turns: Option<u32>,
}
