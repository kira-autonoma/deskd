//! Bus wire-format DTOs and domain conversions.

use serde::{Deserialize, Serialize};

use crate::domain::events::DomainEvent;
use crate::domain::message::{Envelope, Message, Metadata, Register};

// ─── Wire types ─────────────────────────────────────────────────────────────

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

// ─── Domain ↔ Bus conversions ───────────────────────────────────────────────

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
