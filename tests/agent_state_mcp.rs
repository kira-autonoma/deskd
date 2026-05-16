//! End-to-end test for the `agent_state` MCP tool (#456).
//!
//! Spawns `deskd mcp --agent <name>` as a subprocess, drives the full
//! JSON-RPC handshake (initialize → tools/list → tools/call) and asserts:
//!   1. `agent_state` is advertised in tools/list.
//!   2. `write` then `read` round-trips through the subprocess, with the
//!      `Last updated:` stamp inserted automatically.
//!   3. `append` adds a bullet under a `## Section`.
//!   4. `set_section` replaces a single section while leaving siblings intact.
//!   5. A configured per-agent `state_file:` is honoured (read default is empty
//!      after we wrote elsewhere).

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

fn temp_socket() -> String {
    format!("/tmp/deskd-test-agent-state-{}.sock", uuid::Uuid::new_v4())
}

fn deskd_bin() -> std::path::PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_deskd") {
        return std::path::PathBuf::from(p);
    }
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("target/debug/deskd")
}

fn spawn_bus(socket: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = deskd::app::bus::serve(&socket).await;
    })
}

async fn write_line(stdin: &mut tokio::process::ChildStdin, value: serde_json::Value) {
    let mut s = serde_json::to_string(&value).unwrap();
    s.push('\n');
    stdin.write_all(s.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

async fn read_response(
    stdout: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "no response with id={} within 5s", id);
        let line = match timeout(remaining, stdout.next_line()).await {
            Ok(Ok(Some(l))) => l,
            other => panic!(
                "unexpected EOF/error reading mcp stdout (id={}): {:?}",
                id, other
            ),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Non-JSON output (tracing leaks) — skip.
            continue;
        };
        if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
            return v;
        }
    }
}

/// Extract the embedded `content[0].text` payload as a string.
fn content_text(resp: &serde_json::Value) -> String {
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no content[0].text in response: {}", resp))
        .to_string()
}

#[tokio::test]
async fn agent_state_mcp_full_stack_roundtrip() {
    let bin = deskd_bin();
    assert!(
        bin.exists(),
        "deskd binary not found at {} — run `cargo build` before tests",
        bin.display()
    );

    // Isolated $HOME for the subprocess so the test never touches the user's
    // real state files.
    let home_dir = tempfile::TempDir::new().unwrap();
    let agent = format!("itest-{}", uuid::Uuid::new_v4().simple());

    // Per-agent state_file override via a deskd.yaml. The subprocess loads
    // this via DESKD_AGENT_CONFIG. Use a path under the temp HOME.
    let state_path = home_dir.path().join("state.md");
    let yaml = format!(
        "model: claude-sonnet-4-6\nsystem_prompt: \"\"\nagents:\n  - name: {agent}\n    model: haiku\n    subscribe: []\n    state_file: \"{}\"\n",
        state_path.display(),
        agent = agent
    );
    let config_path = home_dir.path().join("deskd.yaml");
    std::fs::write(&config_path, yaml).unwrap();

    // 1. Spawn a bus on a temp socket (the MCP server requires one to exist).
    let socket = temp_socket();
    let _bus_handle = spawn_bus(socket.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Spawn deskd mcp.
    let mut child = Command::new(&bin)
        .args(["mcp", "--agent", &agent])
        .env("DESKD_BUS_SOCKET", &socket)
        .env("DESKD_AGENT_CONFIG", &config_path)
        .env("HOME", home_dir.path())
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn deskd mcp");

    let mut stdin = child.stdin.take().expect("no stdin");
    let stdout = child.stdout.take().expect("no stdout");
    let mut stdout_lines = BufReader::new(stdout).lines();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // 3. initialize handshake.
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
    let _init = read_response(&mut stdout_lines, 1).await;

    // 4. tools/list must advertise agent_state.
    write_line(
        &mut stdin,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let list = read_response(&mut stdout_lines, 2).await;
    let tools = list["result"]["tools"]
        .as_array()
        .expect("tools/list result must include tools array");
    let state_tool = tools
        .iter()
        .find(|t| t["name"] == "agent_state")
        .expect("agent_state tool must be registered");
    let required = state_tool["inputSchema"]["required"]
        .as_array()
        .expect("inputSchema.required must be an array");
    assert!(required.iter().any(|r| r == "action"));
    assert!(required.iter().any(|r| r == "agent"));

    // 5. write a full document and verify it lands on disk under the
    //    configured state_file path.
    write_line(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "agent_state",
                "arguments": {
                    "action": "write",
                    "agent": agent,
                    "content": "# State\n\n## Open PRs\n\n- existing\n"
                }
            }
        }),
    )
    .await;
    let _ = read_response(&mut stdout_lines, 3).await;

    let on_disk = std::fs::read_to_string(&state_path).expect("state file must exist after write");
    assert!(on_disk.contains("Open PRs"));
    assert!(
        on_disk.contains("Last updated: "),
        "stamp missing in:\n{}",
        on_disk
    );

    // 6. read returns the document we just wrote.
    write_line(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "agent_state",
                "arguments": {"action": "read", "agent": agent}
            }
        }),
    )
    .await;
    let read_resp = read_response(&mut stdout_lines, 4).await;
    let payload: serde_json::Value =
        serde_json::from_str(&content_text(&read_resp)).expect("read payload is JSON");
    let content = payload["content"].as_str().unwrap();
    assert!(content.contains("- existing"));
    assert!(content.contains("Last updated: "));

    // 7. append a new bullet under Open PRs.
    write_line(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "agent_state",
                "arguments": {
                    "action": "append",
                    "agent": agent,
                    "section": "Open PRs",
                    "line": "kgatilin/deskd#456"
                }
            }
        }),
    )
    .await;
    let _ = read_response(&mut stdout_lines, 5).await;
    let on_disk = std::fs::read_to_string(&state_path).unwrap();
    assert!(on_disk.contains("- existing"));
    assert!(on_disk.contains("- kgatilin/deskd#456"));

    // 8. set_section replaces only the named section.
    write_line(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "agent_state",
                "arguments": {
                    "action": "set_section",
                    "agent": agent,
                    "section": "Open PRs",
                    "content": "- only-this-one"
                }
            }
        }),
    )
    .await;
    let _ = read_response(&mut stdout_lines, 6).await;
    let on_disk = std::fs::read_to_string(&state_path).unwrap();
    assert!(on_disk.contains("- only-this-one"));
    assert!(!on_disk.contains("- existing"));
    assert!(!on_disk.contains("- kgatilin/deskd#456"));
    assert!(on_disk.contains("# State"));

    // 9. Sanity: a 'read' for an unknown agent must return an empty body
    //    (no error) because no state file has been written for it. This is
    //    the friendlier behaviour we chose for missing files.
    write_line(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "agent_state",
                "arguments": {"action": "read", "agent": format!("ghost-{}", uuid::Uuid::new_v4().simple())}
            }
        }),
    )
    .await;
    let ghost = read_response(&mut stdout_lines, 7).await;
    let payload: serde_json::Value = serde_json::from_str(&content_text(&ghost)).unwrap();
    assert_eq!(payload["content"], "");

    // Cleanup.
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let _ = std::fs::remove_file(&socket);
}
