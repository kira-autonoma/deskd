//! Functional test: bus serve() with real UnixStream connections (#118).
//!
//! Tests the full path: start bus → connect clients via real sockets →
//! send messages → verify delivery through actual UnixStream I/O.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Create a temp socket path that won't collide with other tests.
fn temp_socket() -> String {
    format!(
        "/tmp/deskd-test-bus-{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// Helper: connect to bus, register with a name, return (reader, writer).
async fn connect_and_register(
    socket: &str,
    name: &str,
    subscriptions: &[&str],
) -> (
    tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(socket)
        .await
        .unwrap_or_else(|e| panic!("failed to connect to {}: {}", socket, e));
    let (reader, mut writer) = stream.into_split();

    let reg = serde_json::json!({
        "type": "register",
        "name": name,
        "subscriptions": subscriptions,
    });
    let mut line = serde_json::to_string(&reg).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();

    let lines = BufReader::new(reader).lines();
    (lines, writer)
}

/// Helper: send a message through the bus.
async fn send_message(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    source: &str,
    target: &str,
    payload: serde_json::Value,
) {
    let msg = serde_json::json!({
        "type": "message",
        "id": format!("test-{}", uuid::Uuid::new_v4()),
        "source": source,
        "target": target,
        "payload": payload,
        "metadata": {"priority": 5},
    });
    let mut line = serde_json::to_string(&msg).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
}

/// Helper: read one message with timeout.
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

#[tokio::test]
async fn test_bus_direct_agent_routing() {
    let socket = temp_socket();

    // Start bus in background.
    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::app::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect alice and bob.
    let (mut alice_rx, mut alice_tx) = connect_and_register(&socket, "alice", &[]).await;
    let (mut bob_rx, _bob_tx) = connect_and_register(&socket, "bob", &[]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Alice sends to agent:bob.
    send_message(
        &mut alice_tx,
        "alice",
        "agent:bob",
        serde_json::json!({"task": "hello bob"}),
    )
    .await;

    // Bob should receive it.
    let msg = read_one(&mut bob_rx, 1000).await;
    assert!(msg.is_some(), "bob should receive message from alice");
    let msg = msg.unwrap();
    assert_eq!(msg["source"], "alice");
    assert_eq!(msg["target"], "agent:bob");
    assert_eq!(msg["payload"]["task"], "hello bob");

    // Alice should NOT receive it (not targeted).
    let alice_msg = read_one(&mut alice_rx, 200).await;
    assert!(
        alice_msg.is_none(),
        "alice should not receive her own message"
    );

    // Cleanup socket.
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn test_bus_subscription_routing() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::app::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Bob subscribes to "queue:tasks".
    let (mut bob_rx, _bob_tx) = connect_and_register(&socket, "bob", &["queue:tasks"]).await;
    // Charlie has no subscriptions.
    let (mut charlie_rx, _charlie_tx) = connect_and_register(&socket, "charlie", &[]).await;
    let (_alice_rx, mut alice_tx) = connect_and_register(&socket, "alice", &[]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Alice posts to queue:tasks.
    send_message(
        &mut alice_tx,
        "alice",
        "queue:tasks",
        serde_json::json!({"task": "do work"}),
    )
    .await;

    // Bob should receive (subscribed).
    let msg = read_one(&mut bob_rx, 1000).await;
    assert!(msg.is_some(), "bob should receive queue:tasks message");

    // Charlie should NOT receive (not subscribed).
    let charlie_msg = read_one(&mut charlie_rx, 200).await;
    assert!(
        charlie_msg.is_none(),
        "charlie should not receive queue:tasks"
    );

    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn test_bus_broadcast() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::app::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (_alice_rx, mut alice_tx) = connect_and_register(&socket, "alice", &[]).await;
    let (mut bob_rx, _bob_tx) = connect_and_register(&socket, "bob", &[]).await;
    let (mut charlie_rx, _charlie_tx) = connect_and_register(&socket, "charlie", &[]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_message(
        &mut alice_tx,
        "alice",
        "broadcast",
        serde_json::json!({"announcement": "hello all"}),
    )
    .await;

    // Bob and charlie should both receive.
    let bob_msg = read_one(&mut bob_rx, 1000).await;
    assert!(bob_msg.is_some(), "bob should receive broadcast");

    let charlie_msg = read_one(&mut charlie_rx, 1000).await;
    assert!(charlie_msg.is_some(), "charlie should receive broadcast");

    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn test_bus_list_clients() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::app::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (mut alice_rx, mut alice_tx) = connect_and_register(&socket, "alice", &[]).await;
    let (_bob_rx, _bob_tx) = connect_and_register(&socket, "bob", &[]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send list request.
    let list_req = serde_json::json!({"type": "list"});
    let mut line = serde_json::to_string(&list_req).unwrap();
    line.push('\n');
    alice_tx.write_all(line.as_bytes()).await.unwrap();

    // Alice should receive list_response with both clients.
    let resp = read_one(&mut alice_rx, 1000).await;
    assert!(resp.is_some(), "should receive list_response");
    let resp = resp.unwrap();
    let clients = resp["payload"]["clients"]
        .as_array()
        .expect("clients should be array");
    let names: Vec<&str> = clients.iter().filter_map(|c| c.as_str()).collect();
    assert!(names.contains(&"alice"), "list should contain alice");
    assert!(names.contains(&"bob"), "list should contain bob");

    let _ = std::fs::remove_file(&socket);
}

/// Integration test: two agents produce events, memory agent receives both via glob subscription.
/// Verifies the full bus routing path that memory agents rely on for event injection.
#[tokio::test]
async fn test_memory_agent_receives_events_from_two_agents() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::app::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Memory agent subscribes to agent:* (glob) — receives all agent-targeted messages.
    let (mut memory_rx, _memory_tx) =
        connect_and_register(&socket, "memory-all", &["agent:*"]).await;

    // Two source agents.
    let (_dev_rx, mut dev_tx) = connect_and_register(&socket, "dev", &[]).await;
    let (_worker_rx, mut worker_tx) = connect_and_register(&socket, "worker", &[]).await;
    // Allow registrations to be fully processed by the bus server.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Dev sends a task result to agent:kira.
    send_message(
        &mut dev_tx,
        "dev",
        "agent:kira",
        serde_json::json!({"result": "deployed v1.2.3"}),
    )
    .await;

    // Worker sends an error to agent:collab.
    send_message(
        &mut worker_tx,
        "worker",
        "agent:collab",
        serde_json::json!({"error": "build failed: missing dep"}),
    )
    .await;

    // Small delay to ensure messages are routed.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Memory agent should receive BOTH messages via agent:* glob.
    let msg1 = read_one(&mut memory_rx, 2000).await;
    assert!(
        msg1.is_some(),
        "memory agent should receive first event (from dev)"
    );
    let msg1 = msg1.unwrap();
    assert_eq!(msg1["source"], "dev");

    let msg2 = read_one(&mut memory_rx, 2000).await;
    assert!(
        msg2.is_some(),
        "memory agent should receive second event (from worker)"
    );
    let msg2 = msg2.unwrap();
    assert_eq!(msg2["source"], "worker");

    // Verify format_memory_event works on the received messages.
    use deskd::domain::message::{Message, Metadata};
    let m1 = Message {
        id: msg1["id"].as_str().unwrap_or("").into(),
        source: msg1["source"].as_str().unwrap_or("").into(),
        target: msg1["target"].as_str().unwrap_or("").into(),
        payload: msg1["payload"].clone(),
        reply_to: None,
        metadata: Metadata::default(),
    };
    let m2 = Message {
        id: msg2["id"].as_str().unwrap_or("").into(),
        source: msg2["source"].as_str().unwrap_or("").into(),
        target: msg2["target"].as_str().unwrap_or("").into(),
        payload: msg2["payload"].clone(),
        reply_to: None,
        metadata: Metadata::default(),
    };

    let event1 = deskd::app::worker::format_memory_event(&m1);
    let event2 = deskd::app::worker::format_memory_event(&m2);

    assert!(
        event1.contains("[dev @"),
        "event1 should attribute source dev: {event1}"
    );
    assert!(
        event1.contains("deployed v1.2.3"),
        "event1 should contain result"
    );
    assert!(
        event2.contains("[worker @"),
        "event2 should attribute source worker: {event2}"
    );
    assert!(
        event2.contains("build failed"),
        "event2 should contain error"
    );

    let _ = std::fs::remove_file(&socket);
}
