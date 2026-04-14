//! Configuration DTOs — YAML parsing types and domain conversions.
//!
//! Config value types (`ConfigSessionMode`, `ConfigAgentRuntime`, `ConfigContextConfig`)
//! have moved to `domain::config_types`. This module re-exports them for backward
//! compatibility within infra, and provides additional config DTOs for state machine
//! model definitions and transition parsing.

use serde::{Deserialize, Serialize};

use crate::domain::statemachine::{ModelDef, TransitionDef};
use crate::domain::task::TaskCriteria;

// Re-export config value types from domain.
pub use crate::domain::config_types::{ConfigAgentRuntime, ConfigContextConfig, ConfigSessionMode};

// ─── ModelDef / TransitionDef ───────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

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
