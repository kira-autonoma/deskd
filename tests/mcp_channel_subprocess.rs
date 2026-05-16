//! Integration test for the Claude Code Channels MCP server (#451).
//!
//! Spawns `deskd mcp-channel --agent <name>` as a subprocess and exercises the
//! wire protocol over real stdio + a real Unix bus socket:
//!   1. Initialize handshake — verify capability + serverInfo.name + instructions
//!   2. tools/list — verify `reply` tool is advertised
//!   3. Simulate a Telegram inbound on the bus — verify the subprocess emits
//!      `notifications/claude/channel` with sanitized meta on stdout
//!
//! This is the "spawn deskd as MCP subprocess, simulate Telegram inbound,
//! assert notification frame on stdout" acceptance criterion from #451.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::timeout;

fn temp_socket() -> String {
    format!("/tmp/deskd-test-mcp-chan-{}.sock", uuid::Uuid::new_v4())
}

/// Locate the freshly built `deskd` binary. Cargo sets `CARGO_BIN_EXE_deskd`
/// for integration tests in the same package; fall back to a debug path.
fn deskd_bin() -> std::path::PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_deskd") {
        return std::path::PathBuf::from(p);
    }
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("target/debug/deskd")
}

/// Spawn the bus server on the given socket. Returns the join handle so the
/// caller can drop it at end of test.
fn spawn_bus(socket: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = deskd::app::bus::serve(&socket).await;
    })
}

/// Write a single JSON-RPC line to the subprocess's stdin.
async fn write_line(stdin: &mut tokio::process::ChildStdin, value: serde_json::Value) {
    let mut s = serde_json::to_string(&value).unwrap();
    s.push('\n');
    stdin.write_all(s.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

/// Read JSON-RPC lines off the subprocess until `pred` returns Some(value),
/// or the timeout expires. Skips non-JSON lines (tracing breakage etc.) and
/// JSON that doesn't match the predicate.
async fn read_until<P>(
    stdout: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    timeout_ms: u64,
    mut pred: P,
) -> Option<serde_json::Value>
where
    P: FnMut(&serde_json::Value) -> bool,
{
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let line = match timeout(remaining, stdout.next_line()).await {
            Ok(Ok(Some(l))) => l,
            _ => return None,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Non-JSON output (e.g. tracing leaked to stdout) — skip.
            continue;
        };
        if pred(&v) {
            return Some(v);
        }
    }
}

#[tokio::test]
async fn test_mcp_channel_subprocess_emits_notification_for_telegram_inbound() {
    let bin = deskd_bin();
    if !bin.exists() {
        // CI must have built deskd before invoking the test; if not, surface
        // a clear panic instead of a confusing "no such file".
        panic!(
            "deskd binary not found at {} — run `cargo build` before tests",
            bin.display()
        );
    }

    // 1. Spawn a bus on a temp socket.
    let socket = temp_socket();
    let _bus_handle = spawn_bus(socket.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Spawn deskd mcp-channel as subprocess pointing at the bus.
    let mut child = Command::new(&bin)
        .args(["mcp-channel", "--agent", "testagent"])
        .env("DESKD_BUS_SOCKET", &socket)
        // Silence subprocess tracing to keep stdout JSON-pure.
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn deskd mcp-channel");

    let mut stdin = child.stdin.take().expect("no stdin");
    let stdout = child.stdout.take().expect("no stdout");
    let mut stdout_lines = BufReader::new(stdout).lines();

    // Give the subprocess a moment to connect its bus listener.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 3. Drive the initialize handshake.
    write_line(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }),
    )
    .await;

    let init_resp = read_until(&mut stdout_lines, 3000, |v| {
        v.get("id").and_then(|i| i.as_u64()) == Some(1)
    })
    .await
    .expect("no initialize response within 3s");

    // Channel capability must be present — without it Claude Code wouldn't
    // register a notification listener.
    assert!(
        init_resp["result"]["capabilities"]["experimental"]["claude/channel"].is_object(),
        "initialize must advertise experimental.claude/channel: {:?}",
        init_resp
    );
    // Server name surfaces as the <channel source="..."> attribute.
    assert_eq!(
        init_resp["result"]["serverInfo"]["name"], "deskd-telegram",
        "server name drives <channel source>; must be deskd-telegram"
    );
    // Instructions string parameterises the agent's system prompt.
    let instr = init_resp["result"]["instructions"]
        .as_str()
        .expect("instructions string required in channel mode");
    assert!(instr.contains("<channel"));
    assert!(instr.contains("reply"));

    // 4. Drive tools/list and assert the reply tool is exposed.
    write_line(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )
    .await;

    let list_resp = read_until(&mut stdout_lines, 3000, |v| {
        v.get("id").and_then(|i| i.as_u64()) == Some(2)
    })
    .await
    .expect("no tools/list response within 3s");

    let tools = list_resp["result"]["tools"]
        .as_array()
        .expect("tools must be an array");
    let reply = tools
        .iter()
        .find(|t| t["name"] == "reply")
        .expect("reply tool must be registered in channel mode");
    let schema = &reply["inputSchema"];
    let required = schema["required"].as_array().expect("required array");
    assert!(required.iter().any(|r| r == "chat_id"));
    assert!(required.iter().any(|r| r == "text"));

    // 5. Simulate a Telegram inbound message arriving on the bus targeted at
    //    `telegram.in:<chat_id>`. The subprocess subscribed to telegram.in:*
    //    so it should emit a notifications/claude/channel frame on stdout.
    let stream = UnixStream::connect(&socket).await.unwrap();
    let (_r, mut w) = stream.into_split();

    let reg = serde_json::json!({
        "type": "register",
        "name": "telegram-test-publisher",
        "subscriptions": [],
    });
    let mut reg_line = serde_json::to_string(&reg).unwrap();
    reg_line.push('\n');
    w.write_all(reg_line.as_bytes()).await.unwrap();
    w.flush().await.unwrap();

    let msg = serde_json::json!({
        "type": "message",
        "id": "test-msg-1",
        "source": "telegram-testagent",
        "target": "telegram.in:42",
        "payload": {
            "task": "hello from telegram",
            "telegram_chat_id": 42,
            "telegram_sender": {"id": 7, "username": "alice"},
            "telegram_message_id": 99,
        },
        "metadata": {"priority": 5},
    });
    let mut msg_line = serde_json::to_string(&msg).unwrap();
    msg_line.push('\n');
    w.write_all(msg_line.as_bytes()).await.unwrap();
    w.flush().await.unwrap();

    // 6. Read until the channel notification arrives on stdout.
    let notif = read_until(&mut stdout_lines, 5000, |v| {
        v.get("method").and_then(|m| m.as_str()) == Some("notifications/claude/channel")
    })
    .await
    .expect("no channel notification emitted within 5s");

    // Notification shape — params.content + params.meta.{chat_id,user_id}.
    assert_eq!(notif["params"]["content"], "hello from telegram");
    let meta = &notif["params"]["meta"];
    assert_eq!(
        meta["chat_id"], "42",
        "chat_id must surface as a string per the channels spec"
    );
    assert_eq!(
        meta["user_id"], "7",
        "user_id must surface as a string per the channels spec"
    );
    assert_eq!(meta["message_id"], "99");

    // No notification id (notifications have no id per JSON-RPC).
    assert!(notif.get("id").is_none(), "notifications must not have id");

    // Cleanup.
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let _ = std::fs::remove_file(&socket);
}
