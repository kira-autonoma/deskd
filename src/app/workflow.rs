use anyhow::Result;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::app::statemachine;
use crate::domain::events::DomainEvent;
use crate::domain::message::{Message, Metadata};
use crate::domain::statemachine::{ModelDef, StepType};
use crate::domain::workitem::WorkItem;
use crate::ports::bus::MessageBus;
use crate::ports::store::{StateMachineRepository, TaskRepository};

/// Publish a domain event to the message bus.
///
/// Events are sent to `events:<event_type>` targets so subscribers can
/// use glob patterns like `events:*` to receive all events.
async fn emit_event(bus: &dyn MessageBus, event: &DomainEvent) {
    let msg = Message {
        id: Uuid::new_v4().to_string(),
        source: "workflow-engine".to_string(),
        target: format!("events:{}", event.event_type()),
        payload: event.to_json(),
        reply_to: None,
        metadata: Metadata {
            priority: 1,
            ..Default::default()
        },
    };
    if let Err(e) = bus.send(&msg).await {
        warn!(error = %e, event = %event.event_type(), "failed to emit domain event");
    }
}

/// Publish a domain event via a one-shot bus connection.
///
/// Used by callers outside the workflow engine (e.g. MCP handlers) to emit
/// events without holding a persistent bus connection.
pub async fn publish_event(bus_socket: &str, source: &str, event: &DomainEvent) -> Result<()> {
    let bus = crate::infra::unix_bus::UnixBus::connect(bus_socket).await?;
    bus.register(&format!("{}-event-pub", source), &[]).await?;
    let msg = Message {
        id: Uuid::new_v4().to_string(),
        source: source.to_string(),
        target: format!("events:{}", event.event_type()),
        payload: event.to_json(),
        reply_to: None,
        metadata: Metadata {
            priority: 1,
            ..Default::default()
        },
    };
    bus.send(&msg).await?;
    Ok(())
}

/// Notify the workflow engine that an SM instance was moved.
///
/// Connects to the bus, sends an `action: "moved"` message to `sm:<id>`,
/// and disconnects. The workflow engine picks this up and dispatches if needed.
pub async fn notify_moved(bus_socket: &str, instance_id: &str, source: &str) -> Result<()> {
    let bus = crate::infra::unix_bus::UnixBus::connect(bus_socket).await?;
    bus.register(&format!("{}-sm-notify", source), &[]).await?;
    let msg = Message {
        id: Uuid::new_v4().to_string(),
        source: source.to_string(),
        target: format!("sm:{}", instance_id),
        payload: serde_json::json!({"action": "moved"}),
        reply_to: None,
        metadata: Metadata::default(),
    };
    bus.send(&msg).await?;
    Ok(())
}

/// Run the workflow engine on the given bus.
/// Registers as `workflow-engine`, subscribes to `sm:*`.
/// On startup, dispatches pending instances. Then listens for completion messages.
pub async fn run(
    bus: &dyn MessageBus,
    models: Vec<ModelDef>,
    sm_store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
) -> Result<()> {
    bus.register("workflow-engine", &["sm:*".to_string()])
        .await?;

    info!("workflow engine started, subscribed to sm:*");

    // On startup: check for pending instances and dispatch them.
    dispatch_pending(bus, &models, sm_store, task_store).await;

    // Main loop: listen for completion messages.
    loop {
        let msg = bus.recv().await?;

        // Extract instance ID from target: "sm:sm-a1b2c3d4"
        let instance_id = match msg.target.strip_prefix("sm:") {
            Some(id) => id.to_string(),
            None => continue,
        };

        // Check if this is a move notification (from CLI/MCP sm_move).
        if msg.payload.get("action").and_then(|a| a.as_str()) == Some("moved") {
            info!(instance = %instance_id, "received move notification");
            if let Err(e) =
                handle_move_notification(bus, &models, sm_store, task_store, &instance_id).await
            {
                warn!(instance = %instance_id, error = %e, "failed to handle move notification");
            }
            continue;
        }

        // Extract result from payload.
        let result = msg
            .payload
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();

        let error = msg
            .payload
            .get("error")
            .and_then(|e| e.as_str())
            .map(|s| s.to_string());

        info!(instance = %instance_id, "received completion");

        if let Err(e) = handle_completion(
            bus,
            &models,
            sm_store,
            task_store,
            &instance_id,
            &result,
            error.as_deref(),
        )
        .await
        {
            warn!(instance = %instance_id, error = %e, "failed to process completion");
        }
    }
}

/// Handle a task completion: apply transitions and dispatch next step.
async fn handle_completion(
    bus: &dyn MessageBus,
    models: &[ModelDef],
    store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
    instance_id: &str,
    result: &str,
    error: Option<&str>,
) -> Result<()> {
    let mut inst = store.load(instance_id)?;
    let model = models
        .iter()
        .find(|m| m.name == inst.model)
        .ok_or_else(|| anyhow::anyhow!("model '{}' not found", inst.model))?;

    // Store result.
    inst.result = Some(result.to_string());
    if let Some(err) = error {
        inst.error = Some(err.to_string());
    }
    inst.updated_at = chrono::Utc::now().to_rfc3339();
    store.save(&inst)?;

    // Find matching transition.
    let current_state = inst.state.clone();
    let target_state = find_next_state(model, &current_state, result);

    if let Some(target) = target_state {
        store.move_to(&mut inst, model, &target, "auto", Some(result), None, None)?;

        info!(
            instance = %instance_id,
            from = %current_state,
            to = %target,
            "transition applied"
        );

        emit_event(
            bus,
            &DomainEvent::TransitionApplied {
                instance_id: instance_id.to_string(),
                from: current_state.clone(),
                to: target.clone(),
                trigger: "auto".into(),
            },
        )
        .await;

        // If terminal, emit completion event.
        if statemachine::is_terminal(model, &inst) {
            emit_event(
                bus,
                &DomainEvent::InstanceCompleted {
                    instance_id: instance_id.to_string(),
                    model: inst.model.clone(),
                    final_state: target,
                },
            )
            .await;
        } else {
            dispatch_instance(bus, model, &inst, store, task_store).await?;
        }
    } else {
        info!(instance = %instance_id, state = %inst.state, "no matching transition, awaiting manual move");
    }

    Ok(())
}

/// Handle a move notification: reload instance and dispatch if it has an assignee.
async fn handle_move_notification(
    bus: &dyn MessageBus,
    models: &[ModelDef],
    store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
    instance_id: &str,
) -> Result<()> {
    let inst = store.load(instance_id)?;
    let model = models
        .iter()
        .find(|m| m.name == inst.model)
        .ok_or_else(|| anyhow::anyhow!("model '{}' not found", inst.model))?;

    if statemachine::is_terminal(model, &inst) {
        info!(instance = %instance_id, state = %inst.state, "moved to terminal state, no dispatch");
        return Ok(());
    }

    if !inst.assignee.is_empty() {
        dispatch_instance(bus, model, &inst, store, task_store).await?;
        info!(instance = %instance_id, assignee = %inst.assignee, "dispatched after move");
    }

    Ok(())
}

/// Find the next state based on transition rules.
/// Priority: 1) keyword match (`on:`), 2) auto trigger, 3) none.
fn find_next_state(model: &ModelDef, current_state: &str, result: &str) -> Option<String> {
    let transitions = statemachine::valid_transitions(model, current_state);
    let result_upper = result.trim().to_uppercase();

    // 1. Check keyword matches first.
    for t in &transitions {
        if let Some(ref keyword) = t.on
            && result_upper.starts_with(&keyword.to_uppercase())
        {
            return Some(t.to.clone());
        }
    }

    // 2. Check auto triggers.
    for t in &transitions {
        if t.trigger.as_deref() == Some("auto") {
            return Some(t.to.clone());
        }
    }

    None
}

/// Build a WorkItem from a model and instance.
///
/// Extracts the transition definition for the current state, determines
/// step type, builds task text, and populates all fields. This is the
/// single creation path — all transitions go through here.
fn build_work_item(model: &ModelDef, inst: &statemachine::Instance) -> WorkItem {
    // Find the prompt for the transition that leads to the current state.
    let prompt = inst
        .history
        .last()
        .and_then(|h| {
            model
                .transitions
                .iter()
                .find(|t| t.to == inst.state && (t.from == h.from || t.from == "*"))
        })
        .and_then(|t| t.prompt.as_ref())
        .cloned()
        .unwrap_or_default();

    let transition_def = model.transitions.iter().find(|t| t.to == inst.state);

    let step_type = transition_def
        .map(|t| &t.step_type)
        .cloned()
        .unwrap_or_default();

    let task_text = build_task_text(&prompt, inst);

    WorkItem {
        instance_id: inst.id.clone(),
        step_type,
        assignee: inst.assignee.clone(),
        task_text,
        prompt,
        command: transition_def.and_then(|t| t.command.clone()),
        notify: transition_def.and_then(|t| t.notify.clone()),
        criteria: transition_def.and_then(|t| t.criteria.clone()),
        timeout: transition_def.and_then(|t| t.timeout.clone()),
        timeout_goto: transition_def.and_then(|t| t.timeout_goto.clone()),
        max_retries: transition_def.map(|t| t.max_retries).unwrap_or(0),
    }
}

/// Dispatch a WorkItem to the appropriate execution path.
///
/// Routes based on step_type and criteria:
/// - Human → notification to notify target
/// - Check → shell command execution
/// - Validate → lightweight LLM review
/// - Agent → always creates a task, then queue (pull) or bus (push)
///
/// Returns the task ID if a task was created (Agent steps always create one).
async fn dispatch_work_item(
    bus: &dyn MessageBus,
    model: &ModelDef,
    inst: &statemachine::Instance,
    work_item: &WorkItem,
    sm_store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
) -> Result<Option<String>> {
    match work_item.step_type {
        StepType::Human => {
            if let Some(ref notify_target) = work_item.notify {
                let msg = Message {
                    id: Uuid::new_v4().to_string(),
                    source: "workflow-engine".to_string(),
                    target: notify_target.clone(),
                    payload: serde_json::json!({
                        "task": work_item.task_text,
                        "sm_instance_id": work_item.instance_id,
                    }),
                    reply_to: Some(format!("sm:{}", work_item.instance_id)),
                    metadata: Metadata::default(),
                };
                bus.send(&msg).await?;
                info!(instance = %work_item.instance_id, target = %notify_target, "human notification sent");
                // Emit event so observers know a notification was sent.
                emit_event(
                    bus,
                    &DomainEvent::TaskDispatched {
                        task_id: String::new(),
                        instance_id: Some(work_item.instance_id.clone()),
                        assignee: notify_target.clone(),
                    },
                )
                .await;
            }
            Ok(None)
        }
        StepType::Check => {
            let transition_def = model.transitions.iter().find(|t| t.to == inst.state);
            dispatch_check_step(bus, model, inst, transition_def, sm_store, task_store).await?;
            Ok(None)
        }
        StepType::Validate => {
            dispatch_validate_step(bus, model, inst, work_item, sm_store, task_store).await?;
            Ok(None)
        }
        StepType::Agent => {
            let task_id = dispatch_agent(bus, inst, work_item, task_store).await?;
            Ok(Some(task_id))
        }
    }
}

/// Dispatch an agent step — always creates a task for tracking, then either
/// dispatches via task queue (pull-based, when criteria is set) or via direct
/// bus message (push-based, when no criteria).
///
/// Returns the task ID of the created task.
async fn dispatch_agent(
    bus: &dyn MessageBus,
    inst: &statemachine::Instance,
    work_item: &WorkItem,
    task_store: &dyn TaskRepository,
) -> Result<String> {
    let store = task_store;
    let criteria = work_item.criteria.clone().unwrap_or_default();
    let mut task = store.create_for_sm(
        &work_item.task_text,
        criteria,
        "workflow-engine",
        &work_item.instance_id,
    )?;
    if work_item.max_retries > 0 {
        task.max_retries = work_item.max_retries;
        store.save(&task)?;
    }
    let task_id = task.id.clone();

    if work_item.criteria.is_some() {
        // Dispatch via task queue (pull-based) — worker will claim from queue.
        info!(
            instance = %work_item.instance_id,
            task_id = %task_id,
            max_retries = work_item.max_retries,
            "task created in queue for SM dispatch (pull)"
        );
    } else {
        // Direct bus message dispatch — task exists for tracking but work
        // is pushed directly to the assignee.
        let msg = Message {
            id: Uuid::new_v4().to_string(),
            source: "workflow-engine".to_string(),
            target: inst.assignee.clone(),
            payload: serde_json::json!({
                "task": work_item.task_text,
                "sm_instance_id": work_item.instance_id,
                "task_queue_id": task_id,
            }),
            reply_to: Some(format!("sm:{}", work_item.instance_id)),
            metadata: Metadata::default(),
        };
        bus.send(&msg).await?;

        info!(
            instance = %work_item.instance_id,
            task_id = %task_id,
            assignee = %inst.assignee,
            "task created and dispatched via bus (push)"
        );
    }

    emit_event(
        bus,
        &DomainEvent::TaskDispatched {
            task_id: task_id.clone(),
            instance_id: Some(work_item.instance_id.clone()),
            assignee: work_item.assignee.clone(),
        },
    )
    .await;
    Ok(task_id)
}

/// Dispatch a task to the instance's current assignee.
///
/// Creates a WorkItem and routes it to the appropriate execution path.
async fn dispatch_instance(
    bus: &dyn MessageBus,
    model: &ModelDef,
    inst: &statemachine::Instance,
    sm_store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
) -> Result<()> {
    if inst.assignee.is_empty() {
        debug!(instance = %inst.id, state = %inst.state, "no assignee, skipping dispatch");
        return Ok(());
    }

    let work_item = build_work_item(model, inst);
    info!(
        instance = %inst.id,
        step_type = %work_item.step_type,
        "work item created"
    );
    let task_id = dispatch_work_item(bus, model, inst, &work_item, sm_store, task_store).await?;

    // Link the task_id to the instance aggregate and the last transition.
    if let Some(task_id) = task_id
        && let Ok(mut updated_inst) = sm_store.load(&inst.id)
    {
        updated_inst.record_task(&task_id);
        if let Some(last) = updated_inst.history.last_mut() {
            last.task_id = Some(task_id);
        }
        let _ = sm_store.save(&updated_inst);
    }

    Ok(())
}

/// Build the full task text with prompt and context.
fn build_task_text(prompt: &str, inst: &statemachine::Instance) -> String {
    let mut parts = Vec::new();

    if !prompt.is_empty() {
        parts.push(prompt.to_string());
    }

    parts.push(format!("---\n## Task: {}\n\n{}", inst.title, inst.body));

    if let Some(ref result) = inst.result
        && !result.is_empty()
    {
        parts.push(format!("---\n## Previous step result\n\n{}", result));
    }

    let mut meta_lines = format!(
        "---\n## Metadata\ninstance_id: {}\nmodel: {}\nstate: {}",
        inst.id, inst.model, inst.state
    );
    if !inst.metadata.is_null()
        && let Ok(pretty) = serde_json::to_string_pretty(&inst.metadata)
    {
        meta_lines.push_str(&format!("\nmetadata: {}", pretty));
    }
    parts.push(meta_lines);

    parts.join("\n\n")
}

/// Execute a check step: run a shell command and use the result to trigger
/// the next transition. Exit 0 = pass (result is stdout), non-zero = fail.
async fn dispatch_check_step(
    bus: &dyn MessageBus,
    model: &ModelDef,
    inst: &statemachine::Instance,
    transition_def: Option<&crate::domain::statemachine::TransitionDef>,
    sm_store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
) -> Result<()> {
    let command = match transition_def.and_then(|t| t.command.as_deref()) {
        Some(cmd) => cmd,
        None => {
            warn!(
                instance = %inst.id,
                state = %inst.state,
                "check step has no command, skipping"
            );
            return Ok(());
        }
    };

    info!(instance = %inst.id, command = %command, "executing check step");

    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    let (result, error) = if output.status.success() {
        info!(
            instance = %inst.id,
            exit_code = exit_code,
            "check step passed"
        );
        (stdout, None)
    } else {
        warn!(
            instance = %inst.id,
            exit_code = exit_code,
            stderr = %stderr,
            "check step failed"
        );
        let combined = format!("exit code {exit_code}\nstdout: {stdout}\nstderr: {stderr}");
        (
            combined,
            Some(format!("check failed: exit code {exit_code}")),
        )
    };

    // Process the check result through handle_completion, which applies
    // the transition and dispatches the next step. Box::pin to break
    // the async recursion cycle: handle_completion → dispatch_instance →
    // dispatch_check_step → handle_completion.
    let _ = Box::pin(handle_completion(
        bus,
        std::slice::from_ref(model),
        sm_store,
        task_store,
        &inst.id,
        &result,
        error.as_deref(),
    ))
    .await;

    Ok(())
}

/// Execute a validate step: run Claude in print mode (-p) with structured
/// output for a cheap LLM review. No full agent session — single inference.
async fn dispatch_validate_step(
    bus: &dyn MessageBus,
    model: &ModelDef,
    inst: &statemachine::Instance,
    work_item: &WorkItem,
    sm_store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
) -> Result<()> {
    let validate_prompt = if work_item.prompt.is_empty() {
        format!(
            "Review the following and give a pass/fail verdict.\n\n## Task: {}\n\n{}",
            inst.title, inst.body
        )
    } else {
        format!(
            "{}\n\n## Task: {}\n\n{}",
            work_item.prompt, inst.title, inst.body
        )
    };

    // Include previous step result if available.
    let context_prompt = if let Some(ref result) = inst.result
        && !result.is_empty()
    {
        format!(
            "{}\n\n## Previous step result\n\n{}",
            validate_prompt, result
        )
    } else {
        validate_prompt
    };

    // Append JSON format instructions so Claude returns structured output.
    let full_prompt = format!(
        "{}\n\n---\nRespond with JSON only, exactly this schema:\n{{\"verdict\": \"pass\" or \"fail\", \"reason\": \"one sentence\"}}",
        context_prompt
    );

    // Use the model from the command field if specified, otherwise default to haiku.
    let validate_model = work_item.command.as_deref().unwrap_or("claude-haiku-4-5");

    info!(
        instance = %inst.id,
        model = %validate_model,
        "executing validate step"
    );

    let output = tokio::process::Command::new("claude")
        .arg("-p")
        .arg(&full_prompt)
        .arg("--model")
        .arg(validate_model)
        .arg("--output-format")
        .arg("json")
        .arg("--max-turns")
        .arg("1")
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        warn!(
            instance = %inst.id,
            stderr = %stderr,
            "validate step: claude process failed"
        );
        let _ = Box::pin(handle_completion(
            bus,
            std::slice::from_ref(model),
            sm_store,
            task_store,
            &inst.id,
            &format!("validate error: {stderr}"),
            Some("validate step failed: claude process error"),
        ))
        .await;
        return Ok(());
    }

    // Parse the structured verdict from Claude's JSON output.
    // Claude with --output-format json wraps the response; extract the result text.
    let verdict_text = extract_claude_json_result(&stdout);
    let (result, error) = match serde_json::from_str::<serde_json::Value>(&verdict_text) {
        Ok(v) => {
            let verdict = v.get("verdict").and_then(|v| v.as_str()).unwrap_or("fail");
            let reason = v
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("no reason provided");

            if verdict == "pass" {
                info!(instance = %inst.id, reason = %reason, "validate step passed");
                (format!("PASS: {reason}"), None)
            } else {
                warn!(instance = %inst.id, reason = %reason, "validate step failed");
                (
                    format!("FAIL: {reason}"),
                    Some(format!("validation failed: {reason}")),
                )
            }
        }
        Err(e) => {
            warn!(
                instance = %inst.id,
                error = %e,
                raw = %verdict_text,
                "validate step: failed to parse verdict"
            );
            (
                format!("parse error: {e}\nraw: {verdict_text}"),
                Some(format!("validate step: could not parse verdict: {e}")),
            )
        }
    };

    let _ = Box::pin(handle_completion(
        bus,
        std::slice::from_ref(model),
        sm_store,
        task_store,
        &inst.id,
        &result,
        error.as_deref(),
    ))
    .await;

    Ok(())
}

/// Extract the result text from Claude's JSON output format.
///
/// When Claude runs with `--output-format json`, it outputs a JSON object
/// with a `result` field containing the actual response text.
fn extract_claude_json_result(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        // Claude JSON output: {"result": "...", ...}
        if let Some(result) = v.get("result").and_then(|r| r.as_str()) {
            return result.to_string();
        }
    }
    // Fallback: treat the whole output as the result.
    raw.trim().to_string()
}

/// On startup, find non-terminal instances and dispatch them.
///
/// Crash recovery: if an instance has a result but hasn't transitioned
/// (crash between result storage and transition), apply the transition
/// from the stored result instead of re-dispatching.
async fn dispatch_pending(
    bus: &dyn MessageBus,
    models: &[ModelDef],
    store: &dyn StateMachineRepository,
    task_store: &dyn TaskRepository,
) {
    let instances = match store.list_all() {
        Ok(list) => list,
        Err(_) => return,
    };

    // Build a set of SM instance IDs that already have an active task,
    // so we don't create duplicate dispatches.
    let active_sm_ids: std::collections::HashSet<String> = task_store
        .list(Some(crate::domain::task::TaskStatus::Active))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| t.sm_instance_id)
        .collect();

    for inst in &instances {
        let model = match models.iter().find(|m| m.name == inst.model) {
            Some(m) => m,
            None => continue,
        };

        // Skip terminal instances.
        if statemachine::is_terminal(model, inst) {
            continue;
        }

        // Crash recovery: result present but not transitioned.
        // Apply the transition from the stored result.
        if inst.result.as_ref().is_some_and(|r| !r.is_empty()) {
            let result = inst.result.as_deref().unwrap_or("");
            info!(
                instance = %inst.id,
                state = %inst.state,
                "recovering: result present but not transitioned, applying transition"
            );
            if let Err(e) = handle_completion(
                bus,
                models,
                store,
                task_store,
                &inst.id,
                result,
                inst.error.as_deref(),
            )
            .await
            {
                warn!(instance = %inst.id, error = %e, "failed to recover orphaned result");
            }
            continue;
        }

        // Skip if there's already an active task for this instance (idempotency).
        if active_sm_ids.contains(&inst.id) {
            debug!(instance = %inst.id, "skipping dispatch: active task already exists");
            continue;
        }

        // Dispatch if has assignee (pending work).
        if !inst.assignee.is_empty()
            && let Err(e) = dispatch_instance(bus, model, inst, store, task_store).await
        {
            warn!(instance = %inst.id, error = %e, "failed to dispatch pending instance");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::statemachine::{StepType, TransitionDef};
    use crate::ports::store::{StateMachineReader, StateMachineWriter};

    fn test_model() -> ModelDef {
        ModelDef {
            name: "pipeline".into(),
            description: "Test pipeline".into(),
            states: vec![
                "draft".into(),
                "review".into(),
                "approved".into(),
                "rejected".into(),
            ],
            initial: "draft".into(),
            terminal: vec!["approved".into(), "rejected".into()],
            transitions: vec![
                TransitionDef {
                    from: "draft".into(),
                    to: "review".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: Some("agent:reviewer".into()),
                    prompt: Some("Review this.".into()),
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "review".into(),
                    to: "approved".into(),
                    trigger: None,
                    on: Some("LGTM".into()),
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "review".into(),
                    to: "rejected".into(),
                    trigger: None,
                    on: Some("REJECT".into()),
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        }
    }

    #[test]
    fn test_find_next_state_keyword_match() {
        let model = test_model();
        // "LGTM looks good" starts with "LGTM" -> approved
        assert_eq!(
            find_next_state(&model, "review", "LGTM looks good"),
            Some("approved".into())
        );
        // Case-insensitive
        assert_eq!(
            find_next_state(&model, "review", "lgtm"),
            Some("approved".into())
        );
    }

    #[test]
    fn test_find_next_state_reject_keyword() {
        let model = test_model();
        assert_eq!(
            find_next_state(&model, "review", "REJECT: needs work"),
            Some("rejected".into())
        );
    }

    #[test]
    fn test_find_next_state_auto_trigger() {
        let model = test_model();
        // From "draft" there's an auto trigger to "review"
        assert_eq!(
            find_next_state(&model, "draft", "anything"),
            Some("review".into())
        );
    }

    #[test]
    fn test_find_next_state_no_match() {
        let model = test_model();
        // From "review" with result that doesn't match any keyword and no auto trigger
        assert_eq!(find_next_state(&model, "review", "not sure"), None);
    }

    #[test]
    fn test_find_next_state_keyword_priority_over_auto() {
        // Keywords should be checked before auto triggers
        let model = ModelDef {
            name: "test".into(),
            description: String::new(),
            states: vec!["a".into(), "b".into(), "c".into()],
            initial: "a".into(),
            terminal: vec!["c".into()],
            transitions: vec![
                TransitionDef {
                    from: "a".into(),
                    to: "b".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "a".into(),
                    to: "c".into(),
                    trigger: None,
                    on: Some("DONE".into()),
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        };
        // Keyword match should take priority over auto
        assert_eq!(find_next_state(&model, "a", "DONE"), Some("c".into()));
        // No keyword match -> auto
        assert_eq!(
            find_next_state(&model, "a", "something else"),
            Some("b".into())
        );
    }

    #[test]
    fn test_build_task_text_basic() {
        let inst = statemachine::Instance {
            id: "sm-test123".into(),
            model: "pipeline".into(),
            title: "Fix bug".into(),
            body: "Details here".into(),
            state: "review".into(),
            assignee: "agent:reviewer".into(),
            result: None,
            error: None,
            created_by: "kira".into(),
            created_at: String::new(),
            updated_at: String::new(),
            history: vec![],
            metadata: serde_json::Value::Null,
            total_cost: 0.0,
            total_turns: 0,
            task_ids: Vec::new(),
        };
        let text = build_task_text("Review this code.", &inst);
        assert!(text.contains("Review this code."));
        assert!(text.contains("Fix bug"));
        assert!(text.contains("Details here"));
        assert!(text.contains("sm-test123"));
    }

    #[test]
    fn test_build_task_text_with_previous_result() {
        let inst = statemachine::Instance {
            id: "sm-test456".into(),
            model: "pipeline".into(),
            title: "Task".into(),
            body: String::new(),
            state: "review".into(),
            assignee: "agent:reviewer".into(),
            result: Some("Previous output here".into()),
            error: None,
            created_by: "kira".into(),
            created_at: String::new(),
            updated_at: String::new(),
            history: vec![],
            metadata: serde_json::Value::Null,
            total_cost: 0.0,
            total_turns: 0,
            task_ids: Vec::new(),
        };
        let text = build_task_text("", &inst);
        assert!(text.contains("Previous output here"));
    }

    fn queue_dispatch_model() -> ModelDef {
        ModelDef {
            name: "feature".into(),
            description: "Feature pipeline with queue dispatch".into(),
            states: vec![
                "backlog".into(),
                "planning".into(),
                "implementing".into(),
                "done".into(),
            ],
            initial: "backlog".into(),
            terminal: vec!["done".into()],
            transitions: vec![
                TransitionDef {
                    from: "backlog".into(),
                    to: "planning".into(),
                    trigger: Some("start".into()),
                    on: None,
                    assignee: Some("agent:planner".into()),
                    prompt: Some("Create a plan.".into()),
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: Some(crate::app::task::TaskCriteria {
                        model: Some("claude-sonnet-4-6".into()),
                        labels: vec!["planning".into()],
                    }),
                    max_retries: 0,
                },
                TransitionDef {
                    from: "planning".into(),
                    to: "implementing".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: Some("agent:dev".into()),
                    prompt: Some("Implement the plan.".into()),
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: Some(crate::app::task::TaskCriteria {
                        model: Some("claude-sonnet-4-6".into()),
                        labels: vec![],
                    }),
                    max_retries: 0,
                },
                TransitionDef {
                    from: "implementing".into(),
                    to: "done".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        }
    }

    #[tokio::test]
    async fn test_sm_dispatch_creates_task_in_queue() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sm_dir = std::path::PathBuf::from(format!("/tmp/deskd_sm_queue_test_{}", ts));
        let task_dir = std::path::PathBuf::from(format!("/tmp/deskd_task_queue_test_{}", ts));

        let sm_store = statemachine::StateMachineStore::new(sm_dir.clone());
        let task_store = crate::app::task::TaskStore::new(task_dir.clone());
        let model = queue_dispatch_model();

        // Create SM instance.
        let inst = sm_store
            .create(&model, "FEAT-100", "Build widget", "kira")
            .unwrap();
        assert_eq!(inst.state, "backlog");

        // Move to planning — this transition has criteria.
        let mut inst = inst;
        sm_store
            .move_to(&mut inst, &model, "planning", "start", None, None, None)
            .unwrap();
        assert_eq!(inst.state, "planning");
        assert_eq!(inst.assignee, "agent:planner");

        // Simulate dispatch — create task via TaskStore (same as dispatch_instance with criteria).
        let transition = model
            .transitions
            .iter()
            .find(|t| t.to == "planning")
            .unwrap();
        let criteria = transition.criteria.clone().unwrap();
        let task_text = build_task_text(transition.prompt.as_deref().unwrap_or(""), &inst);
        let task = task_store
            .create_for_sm(&task_text, criteria, "workflow-engine", &inst.id)
            .unwrap();

        // Verify task was created with correct sm_instance_id.
        assert_eq!(task.sm_instance_id.as_deref(), Some(inst.id.as_str()));
        assert_eq!(task.status, crate::app::task::TaskStatus::Pending);
        assert!(task.description.contains("Create a plan."));
        assert!(task.description.contains("FEAT-100"));

        // Verify task list shows the SM-linked task.
        let all = task_store.list(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].sm_instance_id.as_deref(), Some(inst.id.as_str()));

        // Simulate completion — handle_completion applies auto transition.
        inst.result = Some("Plan: implement X, Y, Z".into());
        inst.updated_at = chrono::Utc::now().to_rfc3339();
        sm_store.save(&inst).unwrap();

        let target = find_next_state(&model, "planning", "Plan: implement X, Y, Z");
        assert_eq!(target, Some("implementing".into()));

        // Apply the transition.
        sm_store
            .move_to(
                &mut inst,
                &model,
                "implementing",
                "auto",
                Some("Plan: implement X, Y, Z"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(inst.state, "implementing");

        // The implementing transition also has criteria — would create another task.
        let impl_transition = model
            .transitions
            .iter()
            .find(|t| t.to == "implementing")
            .unwrap();
        assert!(impl_transition.criteria.is_some());

        let task2 = task_store
            .create_for_sm(
                &build_task_text(impl_transition.prompt.as_deref().unwrap_or(""), &inst),
                impl_transition.criteria.clone().unwrap(),
                "workflow-engine",
                &inst.id,
            )
            .unwrap();
        assert_eq!(task2.sm_instance_id.as_deref(), Some(inst.id.as_str()));
        assert!(task2.description.contains("Implement the plan."));

        // After implementing, auto goes to done (terminal).
        let final_target = find_next_state(&model, "implementing", "Done: all implemented");
        assert_eq!(final_target, Some("done".into()));
        sm_store
            .move_to(&mut inst, &model, "done", "auto", None, None, None)
            .unwrap();
        assert!(statemachine::is_terminal(&model, &inst));

        // Cleanup.
        let _ = std::fs::remove_dir_all(&sm_dir);
        let _ = std::fs::remove_dir_all(&task_dir);
    }

    #[test]
    fn test_dispatch_without_criteria_uses_bus() {
        // Transitions without criteria should NOT use task queue.
        let model = test_model();
        let transition = model.transitions.iter().find(|t| t.to == "review").unwrap();
        assert!(transition.criteria.is_none());
    }

    #[test]
    fn test_check_step_model_construction() {
        // Verify that a model with check step type and command can be constructed.
        let model = ModelDef {
            name: "verify".into(),
            description: "Verification pipeline".into(),
            states: vec!["start".into(), "checked".into(), "done".into()],
            initial: "start".into(),
            terminal: vec!["done".into()],
            transitions: vec![
                TransitionDef {
                    from: "start".into(),
                    to: "checked".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: Some("workflow-engine".into()),
                    prompt: None,
                    step_type: StepType::Check,
                    command: Some("test -f /tmp/output.txt".into()),
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "checked".into(),
                    to: "done".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        };
        let check_transition = model
            .transitions
            .iter()
            .find(|t| t.step_type == StepType::Check)
            .unwrap();
        assert_eq!(
            check_transition.command.as_deref(),
            Some("test -f /tmp/output.txt")
        );
        assert_eq!(check_transition.step_type, StepType::Check);
    }

    fn make_instance(state: &str, assignee: &str, from: &str) -> statemachine::Instance {
        statemachine::Instance {
            id: "sm-wi-test".into(),
            model: "pipeline".into(),
            title: "Test task".into(),
            body: "Task body".into(),
            state: state.into(),
            assignee: assignee.into(),
            result: None,
            error: None,
            created_by: "kira".into(),
            created_at: String::new(),
            updated_at: String::new(),
            history: vec![statemachine::Transition {
                from: from.into(),
                to: state.into(),
                trigger: "auto".into(),
                timestamp: String::new(),
                note: None,
                cost_usd: None,
                turns: None,
                task_id: None,
            }],
            metadata: serde_json::Value::Null,
            total_cost: 0.0,
            total_turns: 0,
            task_ids: Vec::new(),
        }
    }

    #[test]
    fn test_build_work_item_agent_without_criteria() {
        let model = test_model();
        let inst = make_instance("review", "agent:reviewer", "draft");

        let wi = build_work_item(&model, &inst);
        assert_eq!(wi.step_type, StepType::Agent);
        assert_eq!(wi.assignee, "agent:reviewer");
        assert!(wi.criteria.is_none());
        assert!(wi.task_text.contains("Review this."));
        assert!(wi.task_text.contains("Test task"));
        assert_eq!(wi.prompt, "Review this.");
        assert_eq!(wi.instance_id, "sm-wi-test");
    }

    #[test]
    fn test_build_work_item_agent_with_criteria() {
        let model = queue_dispatch_model();
        let inst = make_instance("planning", "agent:planner", "backlog");

        let wi = build_work_item(&model, &inst);
        assert_eq!(wi.step_type, StepType::Agent);
        assert!(wi.criteria.is_some());
        let criteria = wi.criteria.unwrap();
        assert_eq!(criteria.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(criteria.labels, vec!["planning"]);
        assert_eq!(wi.max_retries, 0);
    }

    #[test]
    fn test_build_work_item_check() {
        let model = ModelDef {
            name: "pipeline".into(),
            description: String::new(),
            states: vec!["start".into(), "checked".into(), "done".into()],
            initial: "start".into(),
            terminal: vec!["done".into()],
            transitions: vec![
                TransitionDef {
                    from: "start".into(),
                    to: "checked".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: Some("workflow-engine".into()),
                    prompt: None,
                    step_type: StepType::Check,
                    command: Some("cargo test".into()),
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "checked".into(),
                    to: "done".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        };
        let inst = make_instance("checked", "workflow-engine", "start");

        let wi = build_work_item(&model, &inst);
        assert_eq!(wi.step_type, StepType::Check);
        assert_eq!(wi.command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn test_build_work_item_human() {
        let model = ModelDef {
            name: "pipeline".into(),
            description: String::new(),
            states: vec!["start".into(), "awaiting".into(), "done".into()],
            initial: "start".into(),
            terminal: vec!["done".into()],
            transitions: vec![
                TransitionDef {
                    from: "start".into(),
                    to: "awaiting".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: Some("human".into()),
                    prompt: Some("Please review this manually.".into()),
                    step_type: StepType::Human,
                    command: None,
                    notify: Some("telegram.out:-1234".into()),
                    timeout: Some("24h".into()),
                    timeout_goto: Some("done".into()),
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "awaiting".into(),
                    to: "done".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        };
        let inst = make_instance("awaiting", "human", "start");

        let wi = build_work_item(&model, &inst);
        assert_eq!(wi.step_type, StepType::Human);
        assert_eq!(wi.notify.as_deref(), Some("telegram.out:-1234"));
        assert_eq!(wi.timeout.as_deref(), Some("24h"));
        assert_eq!(wi.timeout_goto.as_deref(), Some("done"));
        assert!(wi.task_text.contains("Please review this manually."));
    }

    #[test]
    fn test_build_work_item_with_retry() {
        let mut model = queue_dispatch_model();
        // Set max_retries on the planning transition.
        model.transitions[0].max_retries = 3;
        let inst = make_instance("planning", "agent:planner", "backlog");

        let wi = build_work_item(&model, &inst);
        assert_eq!(wi.max_retries, 3);
    }

    #[test]
    fn test_extract_claude_json_result_with_result_field() {
        let raw = r#"{"result": "{\"verdict\": \"pass\", \"reason\": \"looks good\"}", "cost_usd": 0.001}"#;
        let extracted = extract_claude_json_result(raw);
        assert!(extracted.contains("verdict"));
        assert!(extracted.contains("pass"));
    }

    #[test]
    fn test_extract_claude_json_result_raw_fallback() {
        let raw = r#"{"verdict": "fail", "reason": "bad code"}"#;
        let extracted = extract_claude_json_result(raw);
        assert_eq!(extracted, raw);
    }

    #[test]
    fn test_validate_step_model_construction() {
        let model = ModelDef {
            name: "review-pipeline".into(),
            description: "Pipeline with validation".into(),
            states: vec!["draft".into(), "validated".into(), "done".into()],
            initial: "draft".into(),
            terminal: vec!["done".into()],
            transitions: vec![
                TransitionDef {
                    from: "draft".into(),
                    to: "validated".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: Some("workflow-engine".into()),
                    prompt: Some("Review this code for security issues.".into()),
                    step_type: StepType::Validate,
                    command: Some("claude-haiku-4-5".into()),
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "validated".into(),
                    to: "done".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        };
        let validate_transition = model
            .transitions
            .iter()
            .find(|t| t.step_type == StepType::Validate)
            .unwrap();
        assert_eq!(
            validate_transition.prompt.as_deref(),
            Some("Review this code for security issues.")
        );
        assert_eq!(
            validate_transition.command.as_deref(),
            Some("claude-haiku-4-5")
        );
    }

    /// JSON schema for validation step structured output (reference for future --json-schema flag).
    const VALIDATE_SCHEMA: &str = r#"{
        "type": "object",
        "properties": {
            "verdict": { "type": "string", "enum": ["pass", "fail"] },
            "reason": { "type": "string" }
        },
        "required": ["verdict", "reason"]
    }"#;

    #[test]
    fn test_validate_schema_is_valid_json() {
        let parsed: serde_json::Value = serde_json::from_str(VALIDATE_SCHEMA).unwrap();
        assert_eq!(
            parsed["properties"]["verdict"]["enum"][0].as_str(),
            Some("pass")
        );
        assert_eq!(
            parsed["properties"]["verdict"]["enum"][1].as_str(),
            Some("fail")
        );
    }

    #[test]
    fn test_build_work_item_validate() {
        let model = ModelDef {
            name: "review-pipeline".into(),
            description: String::new(),
            states: vec!["draft".into(), "validated".into(), "done".into()],
            initial: "draft".into(),
            terminal: vec!["done".into()],
            transitions: vec![
                TransitionDef {
                    from: "draft".into(),
                    to: "validated".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: Some("workflow-engine".into()),
                    prompt: Some("Check for security issues.".into()),
                    step_type: StepType::Validate,
                    command: Some("claude-sonnet-4-6".into()),
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "validated".into(),
                    to: "done".into(),
                    trigger: Some("auto".into()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: StepType::default(),
                    command: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        };
        let inst = make_instance("validated", "workflow-engine", "draft");

        let wi = build_work_item(&model, &inst);
        assert_eq!(wi.step_type, StepType::Validate);
        assert_eq!(wi.command.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(wi.prompt, "Check for security issues.");
    }

    /// Test dispatch_pending with in-memory stores and bus — demonstrates
    /// testability enabled by port trait wiring (#254).
    #[tokio::test]
    async fn test_dispatch_pending_with_mock_stores() {
        use crate::infra::memory_bus::InMemoryBus;
        use crate::infra::memory_store::{InMemoryStateMachineStore, InMemoryTaskStore};

        let model = test_model();
        let sm_store = InMemoryStateMachineStore::new();
        let task_store = InMemoryTaskStore::new();
        let (bus_client, bus_server) = InMemoryBus::pair();

        // Create an instance in the "draft" state with an assignee.
        // The test_model has an auto transition draft → review with assignee "agent:reviewer".
        let mut inst = sm_store
            .create(&model, "Test PR", "Please review", "kira")
            .unwrap();
        assert_eq!(inst.state, "draft");

        // Manually set assignee to simulate a pending dispatch.
        inst.assignee = "agent:reviewer".to_string();
        sm_store.save(&inst).unwrap();

        // Run dispatch_pending — should dispatch the instance via the bus.
        dispatch_pending(&bus_client, &[model], &sm_store, &task_store).await;

        // The bus server side should have received at least one message
        // (the dispatched task or event).
        let msg = tokio::time::timeout(std::time::Duration::from_millis(100), bus_server.recv())
            .await
            .expect("expected a message on the bus")
            .expect("bus recv failed");

        // Verify the message targets the assignee.
        assert_eq!(msg.target, "agent:reviewer");
        assert!(msg.payload.get("task").and_then(|t| t.as_str()).is_some());
    }

    /// Test handle_completion with mock stores — verifies transition logic
    /// without any filesystem or socket dependencies.
    #[tokio::test]
    async fn test_handle_completion_applies_transition() {
        use crate::infra::memory_bus::InMemoryBus;
        use crate::infra::memory_store::{InMemoryStateMachineStore, InMemoryTaskStore};

        let model = test_model();
        let sm_store = InMemoryStateMachineStore::new();
        let task_store = InMemoryTaskStore::new();
        let bus = InMemoryBus::loopback();

        // Create an instance and move it to "review" state.
        let inst = sm_store
            .create(&model, "Fix bug", "Details", "kira")
            .unwrap();
        sm_store
            .move_to(
                &mut inst.clone(),
                &model,
                "review",
                "auto",
                None,
                None,
                None,
            )
            .unwrap();

        // Simulate a completion with "LGTM" result — should transition to "approved".
        let result = handle_completion(
            &bus,
            &[model.clone()],
            &sm_store,
            &task_store,
            &inst.id,
            "LGTM looks good",
            None,
        )
        .await;
        assert!(result.is_ok());

        // Verify the instance transitioned to terminal state "approved".
        let updated = sm_store.load(&inst.id).unwrap();
        assert_eq!(updated.state, "approved");
    }
}
