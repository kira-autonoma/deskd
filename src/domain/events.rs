//! Domain events — notifications emitted by state machine and task operations.
//!
//! Pure data types — no I/O, no bus logic. Published to the bus by the
//! application layer (workflow engine, worker).

/// Domain event emitted by orchestration operations.
#[derive(Debug, Clone)]
pub enum DomainEvent {
    /// A new SM instance was created.
    InstanceCreated {
        instance_id: String,
        model: String,
        title: String,
        created_by: String,
    },
    /// A state machine transition was applied.
    TransitionApplied {
        instance_id: String,
        from: String,
        to: String,
        trigger: String,
    },
    /// A task was dispatched (queued or sent to an agent).
    TaskDispatched {
        task_id: String,
        instance_id: Option<String>,
        assignee: String,
    },
    /// A task was completed successfully.
    TaskCompleted {
        task_id: String,
        instance_id: Option<String>,
        result_summary: String,
    },
    /// A task failed.
    TaskFailed {
        task_id: String,
        instance_id: Option<String>,
        error: String,
    },
    /// A task timed out.
    TaskTimedOut {
        task_id: String,
        instance_id: Option<String>,
    },
    /// An instance reached a terminal state.
    InstanceCompleted {
        instance_id: String,
        model: String,
        final_state: String,
    },
}

impl DomainEvent {
    /// Event type as a string, used for bus target routing.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::InstanceCreated { .. } => "instance_created",
            Self::TransitionApplied { .. } => "transition_applied",
            Self::TaskDispatched { .. } => "task_dispatched",
            Self::TaskCompleted { .. } => "task_completed",
            Self::TaskFailed { .. } => "task_failed",
            Self::TaskTimedOut { .. } => "task_timed_out",
            Self::InstanceCompleted { .. } => "instance_completed",
        }
    }

    /// Serialize the event to a JSON value (payload for bus messages).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::InstanceCreated {
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
            Self::TransitionApplied {
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
            Self::TaskDispatched {
                task_id,
                instance_id,
                assignee,
            } => serde_json::json!({
                "event": "task_dispatched",
                "task_id": task_id,
                "instance_id": instance_id,
                "assignee": assignee,
            }),
            Self::TaskCompleted {
                task_id,
                instance_id,
                result_summary,
            } => serde_json::json!({
                "event": "task_completed",
                "task_id": task_id,
                "instance_id": instance_id,
                "result_summary": result_summary,
            }),
            Self::TaskFailed {
                task_id,
                instance_id,
                error,
            } => serde_json::json!({
                "event": "task_failed",
                "task_id": task_id,
                "instance_id": instance_id,
                "error": error,
            }),
            Self::TaskTimedOut {
                task_id,
                instance_id,
            } => serde_json::json!({
                "event": "task_timed_out",
                "task_id": task_id,
                "instance_id": instance_id,
            }),
            Self::InstanceCompleted {
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
    fn test_event_type_names() {
        let event = DomainEvent::InstanceCreated {
            instance_id: "sm-123".into(),
            model: "pipeline".into(),
            title: "Test".into(),
            created_by: "kira".into(),
        };
        assert_eq!(event.event_type(), "instance_created");

        let event = DomainEvent::TransitionApplied {
            instance_id: "sm-123".into(),
            from: "draft".into(),
            to: "review".into(),
            trigger: "auto".into(),
        };
        assert_eq!(event.event_type(), "transition_applied");
    }

    #[test]
    fn test_event_to_json_instance_created() {
        let event = DomainEvent::InstanceCreated {
            instance_id: "sm-abc".into(),
            model: "pipeline".into(),
            title: "Fix bug".into(),
            created_by: "kira".into(),
        };
        let json = event.to_json();
        assert_eq!(json["event"], "instance_created");
        assert_eq!(json["instance_id"], "sm-abc");
        assert_eq!(json["model"], "pipeline");
        assert_eq!(json["title"], "Fix bug");
        assert_eq!(json["created_by"], "kira");
    }

    #[test]
    fn test_event_to_json_transition_applied() {
        let event = DomainEvent::TransitionApplied {
            instance_id: "sm-123".into(),
            from: "draft".into(),
            to: "review".into(),
            trigger: "auto".into(),
        };
        let json = event.to_json();
        assert_eq!(json["event"], "transition_applied");
        assert_eq!(json["from"], "draft");
        assert_eq!(json["to"], "review");
    }

    #[test]
    fn test_event_to_json_task_completed() {
        let event = DomainEvent::TaskCompleted {
            task_id: "task-1".into(),
            instance_id: Some("sm-123".into()),
            result_summary: "Done".into(),
        };
        let json = event.to_json();
        assert_eq!(json["event"], "task_completed");
        assert_eq!(json["task_id"], "task-1");
        assert_eq!(json["instance_id"], "sm-123");
    }

    #[test]
    fn test_event_to_json_task_failed() {
        let event = DomainEvent::TaskFailed {
            task_id: "task-2".into(),
            instance_id: None,
            error: "timeout".into(),
        };
        let json = event.to_json();
        assert_eq!(json["event"], "task_failed");
        assert!(json["instance_id"].is_null());
        assert_eq!(json["error"], "timeout");
    }

    #[test]
    fn test_event_to_json_instance_completed() {
        let event = DomainEvent::InstanceCompleted {
            instance_id: "sm-done".into(),
            model: "pipeline".into(),
            final_state: "approved".into(),
        };
        let json = event.to_json();
        assert_eq!(json["event"], "instance_completed");
        assert_eq!(json["final_state"], "approved");
    }

    #[test]
    fn test_all_event_types_have_json() {
        // Ensure all variants can be serialized.
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
            let json = event.to_json();
            assert!(json["event"].is_string());
        }
    }
}
