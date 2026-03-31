//! Integration tests for the MessageBus trait implementations.
//!
//! Tests UnixBus against the real bus server and InMemoryBus standalone.

use std::sync::Arc;
use std::time::Duration;

use deskd::domain::message::{Message, Metadata};
use deskd::infra::unix_bus::UnixBus;
use deskd::ports::bus::MessageBus;

fn temp_socket() -> String {
    format!("/tmp/deskd-test-mbtrait-{}.sock", uuid::Uuid::new_v4())
}

fn test_message(source: &str, target: &str, task: &str) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        source: source.to_string(),
        target: target.to_string(),
        payload: serde_json::json!({"task": task}),
        reply_to: None,
        metadata: Metadata::default(),
    }
}

/// UnixBus: register + send + recv through the real bus server.
#[tokio::test]
async fn test_unix_bus_send_recv() {
    let socket = temp_socket();

    // Start bus server.
    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Connect two clients via UnixBus trait.
    let alice: Arc<dyn MessageBus> = Arc::new(UnixBus::connect(&socket).await.unwrap());
    let bob: Arc<dyn MessageBus> = Arc::new(UnixBus::connect(&socket).await.unwrap());

    alice
        .register("alice", &["agent:alice".to_string()])
        .await
        .unwrap();
    bob.register("bob", &["agent:bob".to_string()])
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Alice sends to bob via trait.
    let msg = test_message("alice", "agent:bob", "hello via trait");
    alice.send(&msg).await.unwrap();

    // Bob receives via trait.
    let received = tokio::time::timeout(Duration::from_secs(2), bob.recv())
        .await
        .expect("timeout waiting for message")
        .expect("recv failed");

    assert_eq!(received.source, "alice");
    assert_eq!(received.target, "agent:bob");
    assert_eq!(received.payload["task"], "hello via trait");

    let _ = std::fs::remove_file(&socket);
}

/// UnixBus: subscription-based routing through trait.
#[tokio::test]
async fn test_unix_bus_subscription_routing() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let sender: Arc<dyn MessageBus> = Arc::new(UnixBus::connect(&socket).await.unwrap());
    let subscriber: Arc<dyn MessageBus> = Arc::new(UnixBus::connect(&socket).await.unwrap());

    sender.register("sender", &[]).await.unwrap();
    subscriber
        .register("subscriber", &["queue:work".to_string()])
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let msg = test_message("sender", "queue:work", "queued task");
    sender.send(&msg).await.unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("timeout")
        .expect("recv failed");

    assert_eq!(received.payload["task"], "queued task");

    let _ = std::fs::remove_file(&socket);
}

/// UnixBus used as Arc<dyn MessageBus> — proves object safety.
#[tokio::test]
async fn test_unix_bus_as_dyn_trait() {
    let socket = temp_socket();

    let sock = socket.clone();
    tokio::spawn(async move {
        deskd::bus::serve(&sock).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // This function accepts the trait object — proves it's object-safe.
    async fn use_bus(bus: &dyn MessageBus, name: &str) {
        bus.register(name, &[format!("agent:{}", name)])
            .await
            .unwrap();
    }

    let bus = UnixBus::connect(&socket).await.unwrap();
    use_bus(&bus, "trait-test").await;

    let _ = std::fs::remove_file(&socket);
}
