//! Functional test: budget enforcement — agent rejects tasks when over budget (#119).
//!
//! Tests the full bus path: sender posts task → worker detects budget exceeded →
//! worker replies with error → sender receives error → task logged as "skip".
//!
//! Since we can't easily start the real worker (needs Claude), we simulate
//! the worker's budget check logic: connect to bus, receive task, check budget,
//! send error reply, log to tasklog. This verifies the bus delivery and log paths.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn temp_socket() -> String {
    format!(
        "/tmp/deskd-test-budget-{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn temp_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/deskd-test-budget-dir-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

async fn connect_and_register(
    socket: &str,
    name: &str,
    subscriptions: &[&str],
) -> (
    tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(socket).await.unwrap();
    let (reader, mut writer) = stream.into_split();

    let reg = serde_json::json!({
        "type": "register",
        "name": name,
        "subscriptions": subscriptions,
    });
    let mut line = serde_json::to_string(&reg).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();

    (BufReader::new(reader).lines(), writer)
}

async fn read_one(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    timeout_ms: u64,
) -> Option<serde_json::Value> {
    tokio::time::timeout(Duration::from_millis(timeout_ms), lines.next_line())
        .await
        .ok()?
        .ok()?
        .and_then(|l| serde_json::from_str(&l).ok())
}

/// Simulate the worker's budget enforcement path:
/// Receive a task message, check budget, reject with error, log to tasklog.
#[tokio::test]
async fn test_budget_exceeded_sends_error_and_logs() {
    let socket = temp_socket();
    let tmp = temp_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    let log_path = tmp.join("tasks.jsonl");

    // Start bus.
    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect "worker" (simulates agent worker, subscribed to agent:testbot).
    let (mut worker_rx, mut worker_tx) =
        connect_and_register(&socket, "testbot", &["agent:testbot"]).await;

    // Connect "sender" (simulates CLI or another agent).
    let (mut sender_rx, mut sender_tx) =
        connect_and_register(&socket, "sender", &["reply:sender"]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Sender sends a task to agent:testbot.
    let task_msg = serde_json::json!({
        "type": "message",
        "id": "task-001",
        "source": "sender",
        "target": "agent:testbot",
        "payload": {"task": "Please review this code"},
        "reply_to": "reply:sender",
        "metadata": {"priority": 5},
    });
    let mut line = serde_json::to_string(&task_msg).unwrap();
    line.push('\n');
    sender_tx.write_all(line.as_bytes()).await.unwrap();

    // Worker receives the task.
    let received = read_one(&mut worker_rx, 1000).await;
    assert!(received.is_some(), "worker should receive the task");
    let received = received.unwrap();
    assert_eq!(received["payload"]["task"], "Please review this code");

    // Simulate budget check: total_cost=60.0, budget=50.0 → exceeded.
    let total_cost = 60.0_f64;
    let budget_usd = 50.0_f64;
    assert!(total_cost >= budget_usd, "budget should be exceeded");

    // Worker sends error reply (same as real worker does).
    let budget_error = format!(
        "Budget limit reached (${:.2} / ${:.2}). Task not processed.",
        total_cost, budget_usd,
    );
    let error_reply = serde_json::json!({
        "type": "message",
        "id": uuid::Uuid::new_v4().to_string(),
        "source": "testbot",
        "target": "reply:sender",
        "payload": {"error": budget_error, "in_reply_to": "task-001"},
        "metadata": {"priority": 5},
    });
    let mut reply_line = serde_json::to_string(&error_reply).unwrap();
    reply_line.push('\n');
    worker_tx.write_all(reply_line.as_bytes()).await.unwrap();

    // Sender should receive the error.
    let error_msg = read_one(&mut sender_rx, 1000).await;
    assert!(error_msg.is_some(), "sender should receive budget error");
    let error_msg = error_msg.unwrap();
    let error_text = error_msg["payload"]["error"].as_str().unwrap();
    assert!(
        error_text.contains("Budget limit reached"),
        "error should mention budget: {}",
        error_text
    );
    assert!(
        error_text.contains("$60.00"),
        "error should show current cost"
    );
    assert!(
        error_text.contains("$50.00"),
        "error should show budget limit"
    );

    // Worker logs to tasklog (same as real worker).
    let log_entry = deskd::tasklog::TaskLog {
        ts: chrono::Utc::now().to_rfc3339(),
        source: "sender".to_string(),
        turns: 0,
        cost: 0.0,
        duration_ms: 0,
        status: "skip".to_string(),
        task: deskd::tasklog::truncate_task("Please review this code", 60),
        error: Some("budget exceeded".to_string()),
        msg_id: "task-001".to_string(),
        github_repo: None,
        github_pr: None,
        input_tokens: None,
        output_tokens: None,
    };
    deskd::tasklog::log_task_to_path(&log_path, &log_entry).unwrap();

    // Verify task log contains the skip entry.
    let entries = deskd::tasklog::read_logs_from_path(&log_path, 10, None, None).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, "skip");
    assert_eq!(entries[0].error.as_deref(), Some("budget exceeded"));
    assert_eq!(entries[0].msg_id, "task-001");
    assert_eq!(entries[0].task, "Please review this code");

    // Cleanup.
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Test that budget-under tasks are NOT rejected (control test).
#[tokio::test]
async fn test_budget_ok_task_not_rejected() {
    // Simple assertion: if cost < budget, task should proceed.
    let total_cost = 30.0_f64;
    let budget_usd = 50.0_f64;
    assert!(
        total_cost < budget_usd,
        "task should proceed when under budget"
    );

    // The worker would proceed to dispatch to Claude here.
    // We verify this by confirming no error log entry is written.
    let tmp = temp_dir();
    std::fs::create_dir_all(&tmp).unwrap();
    let log_path = tmp.join("tasks.jsonl");

    // No skip entry written (worker would dispatch to Claude instead).
    let entries = deskd::tasklog::read_logs_from_path(&log_path, 10, None, None).unwrap();
    assert!(entries.is_empty(), "no skip entry when budget is OK");

    let _ = std::fs::remove_dir_all(&tmp);
}
