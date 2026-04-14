//! Bus wire-format DTOs and domain conversions.
//!
//! Wire types (`BusMessage`, `BusMetadata`, `BusRegister`, `BusEnvelope`) have moved
//! to `ports::bus_wire`. This module re-exports them for backward compatibility within
//! infra, and provides additional conversions (DomainEvent → JSON, TokenUsage parsing).

use crate::domain::events::DomainEvent;

// Re-export wire types from ports.
pub use crate::ports::bus_wire::{BusEnvelope, BusMessage, BusMetadata, BusRegister};

// ─── Domain Event → JSON ────────────────────────────────────────────────────

impl From<&DomainEvent> for serde_json::Value {
    fn from(event: &DomainEvent) -> Self {
        match event {
            DomainEvent::InstanceCreated {
                instance_id,
                model,
                title,
                created_by,
            } => serde_json::json!({
                "event": "instance_created",
                "instance_id": instance_id,
                "model": model,
                "title": title,
                "created_by": created_by,
            }),
            DomainEvent::TransitionApplied {
                instance_id,
                from,
                to,
                trigger,
            } => serde_json::json!({
                "event": "transition_applied",
                "instance_id": instance_id,
                "from": from,
                "to": to,
                "trigger": trigger,
            }),
            DomainEvent::TaskDispatched {
                task_id,
                instance_id,
                assignee,
            } => serde_json::json!({
                "event": "task_dispatched",
                "task_id": task_id,
                "instance_id": instance_id,
                "assignee": assignee,
            }),
            DomainEvent::TaskCompleted {
                task_id,
                instance_id,
                result_summary,
            } => serde_json::json!({
                "event": "task_completed",
                "task_id": task_id,
                "instance_id": instance_id,
                "result_summary": result_summary,
            }),
            DomainEvent::TaskFailed {
                task_id,
                instance_id,
                error,
            } => serde_json::json!({
                "event": "task_failed",
                "task_id": task_id,
                "instance_id": instance_id,
                "error": error,
            }),
            DomainEvent::TaskTimedOut {
                task_id,
                instance_id,
            } => serde_json::json!({
                "event": "task_timed_out",
                "task_id": task_id,
                "instance_id": instance_id,
            }),
            DomainEvent::InstanceCompleted {
                instance_id,
                model,
                final_state,
            } => serde_json::json!({
                "event": "instance_completed",
                "instance_id": instance_id,
                "model": model,
                "final_state": final_state,
            }),
        }
    }
}

// ─── TokenUsage JSON parsing ────────────────────────────────────────────────

use crate::ports::executor::TokenUsage;

/// Parse a Claude `usage` JSON object into a [`TokenUsage`] struct.
///
/// Expects the `usage` object from Claude's assistant message, e.g.:
/// ```json
/// { "input_tokens": 100, "output_tokens": 50, "cache_read_input_tokens": 10 }
/// ```
impl From<&serde_json::Value> for TokenUsage {
    fn from(usage: &serde_json::Value) -> Self {
        Self {
            input_tokens: usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_creation_input_tokens: usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_read_input_tokens: usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Message;

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
        let domain: crate::domain::message::Envelope = env.into();
        match domain {
            crate::domain::message::Envelope::Message(m) => assert_eq!(m.source, "a"),
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn test_bus_envelope_deserialize_register() {
        let json = r#"{"type":"register","name":"cli","subscriptions":["agent:cli"]}"#;
        let env: BusEnvelope = serde_json::from_str(json).unwrap();
        let domain: crate::domain::message::Envelope = env.into();
        match domain {
            crate::domain::message::Envelope::Register(r) => assert_eq!(r.name, "cli"),
            _ => panic!("expected Register"),
        }
    }

    #[test]
    fn test_domain_event_to_json_instance_created() {
        let event = DomainEvent::InstanceCreated {
            instance_id: "sm-abc".into(),
            model: "pipeline".into(),
            title: "Fix bug".into(),
            created_by: "kira".into(),
        };
        let json = serde_json::Value::from(&event);
        assert_eq!(json["event"], "instance_created");
        assert_eq!(json["instance_id"], "sm-abc");
        assert_eq!(json["model"], "pipeline");
        assert_eq!(json["title"], "Fix bug");
        assert_eq!(json["created_by"], "kira");
    }

    #[test]
    fn test_domain_event_to_json_transition_applied() {
        let event = DomainEvent::TransitionApplied {
            instance_id: "sm-123".into(),
            from: "draft".into(),
            to: "review".into(),
            trigger: "auto".into(),
        };
        let json = serde_json::Value::from(&event);
        assert_eq!(json["event"], "transition_applied");
        assert_eq!(json["from"], "draft");
        assert_eq!(json["to"], "review");
    }

    #[test]
    fn test_domain_event_to_json_task_completed() {
        let event = DomainEvent::TaskCompleted {
            task_id: "task-1".into(),
            instance_id: Some("sm-123".into()),
            result_summary: "Done".into(),
        };
        let json = serde_json::Value::from(&event);
        assert_eq!(json["event"], "task_completed");
        assert_eq!(json["task_id"], "task-1");
        assert_eq!(json["instance_id"], "sm-123");
    }

    #[test]
    fn test_domain_event_to_json_task_failed() {
        let event = DomainEvent::TaskFailed {
            task_id: "task-2".into(),
            instance_id: None,
            error: "timeout".into(),
        };
        let json = serde_json::Value::from(&event);
        assert_eq!(json["event"], "task_failed");
        assert!(json["instance_id"].is_null());
        assert_eq!(json["error"], "timeout");
    }

    #[test]
    fn test_domain_event_to_json_instance_completed() {
        let event = DomainEvent::InstanceCompleted {
            instance_id: "sm-done".into(),
            model: "pipeline".into(),
            final_state: "approved".into(),
        };
        let json = serde_json::Value::from(&event);
        assert_eq!(json["event"], "instance_completed");
        assert_eq!(json["final_state"], "approved");
    }

    #[test]
    fn test_domain_event_all_variants_have_json() {
        let events = vec![
            DomainEvent::InstanceCreated {
                instance_id: "a".into(),
                model: "b".into(),
                title: "c".into(),
                created_by: "d".into(),
            },
            DomainEvent::TransitionApplied {
                instance_id: "a".into(),
                from: "b".into(),
                to: "c".into(),
                trigger: "d".into(),
            },
            DomainEvent::TaskDispatched {
                task_id: "a".into(),
                instance_id: Some("b".into()),
                assignee: "c".into(),
            },
            DomainEvent::TaskCompleted {
                task_id: "a".into(),
                instance_id: None,
                result_summary: "b".into(),
            },
            DomainEvent::TaskFailed {
                task_id: "a".into(),
                instance_id: None,
                error: "b".into(),
            },
            DomainEvent::TaskTimedOut {
                task_id: "a".into(),
                instance_id: None,
            },
            DomainEvent::InstanceCompleted {
                instance_id: "a".into(),
                model: "b".into(),
                final_state: "c".into(),
            },
        ];
        for event in &events {
            let json = serde_json::Value::from(event);
            assert!(json["event"].is_string());
        }
    }
}
