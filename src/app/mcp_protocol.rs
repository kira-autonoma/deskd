/// MCP JSON-RPC protocol types and I/O helpers.
///
/// Handles request/response framing, serialization, and bus channel
/// notifications for the MCP server.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::ports::bus_wire::BusMessage;

// --- MCP Protocol types ---

#[derive(Debug, Deserialize)]
pub(crate) struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

impl Response {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(json!({"code": code, "message": message})),
        }
    }
}

// --- Protocol handlers ---

pub(crate) fn handle_initialize(id: Option<Value>) -> Response {
    Response::ok(
        id,
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "experimental": {
                    "claude/channel": {}
                }
            },
            "serverInfo": {
                "name": "deskd",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

// --- I/O helpers ---

pub(crate) async fn write_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &Response,
) -> Result<()> {
    let json = serde_json::to_string(resp)?;
    // Newline-delimited JSON (compatible with Claude Code).
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    tracing::debug!(resp = %json, "sent MCP response");
    Ok(())
}

// --- Bus channel listener ---

/// Connect to the bus as a persistent subscriber for channel notifications.
/// Returns a line reader for incoming bus messages, or None if connection fails
/// (MCP server degrades gracefully -- tool calls still work without push).
pub(crate) async fn connect_bus_listener(
    agent_name: &str,
    bus_socket: &str,
) -> Option<Arc<Mutex<tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>>>> {
    let stream = match UnixStream::connect(bus_socket).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to connect bus listener -- channel notifications disabled");
            return None;
        }
    };

    let (read_half, mut write_half) = stream.into_split();

    // Register as a channel listener with subscriptions matching the agent.
    let reg = json!({
        "type": "register",
        "name": format!("{}-mcp-channel", agent_name),
        "subscriptions": [
            format!("agent:{}", agent_name),
            format!("reply:{}:*", agent_name),
        ]
    });
    let mut line = serde_json::to_string(&reg).unwrap();
    line.push('\n');

    if let Err(e) = write_half.write_all(line.as_bytes()).await {
        warn!(error = %e, "failed to register bus channel listener");
        return None;
    }
    if let Err(e) = write_half.flush().await {
        warn!(error = %e, "failed to flush bus channel registration");
        return None;
    }

    info!(agent = %agent_name, "bus channel listener connected");

    // Keep write_half alive by leaking it into a background task that does nothing
    // but hold the connection open. When the MCP server exits, it will be dropped.
    tokio::spawn(async move {
        // Hold the write half open until the MCP server exits.
        let _keep = write_half;
        std::future::pending::<()>().await;
    });

    let reader = BufReader::new(read_half);
    Some(Arc::new(Mutex::new(reader.lines())))
}

/// Emit a channel notification to stdout for a bus message.
/// Uses the `notifications/claude/channel` method per the MCP channels spec.
pub(crate) async fn emit_channel_notification<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &BusMessage,
) -> Result<()> {
    // Extract the task text from the payload.
    let content = if let Some(task) = msg.payload.get("task").and_then(|t| t.as_str()) {
        task.to_string()
    } else if let Some(result) = msg.payload.get("result").and_then(|r| r.as_str()) {
        result.to_string()
    } else {
        serde_json::to_string(&msg.payload)?
    };

    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": {
                "source": msg.source,
                "target": msg.target,
                "message_id": msg.id,
            }
        }
    });

    let json_str = serde_json::to_string(&notification)?;
    writer.write_all(json_str.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    info!(source = %msg.source, "emitted channel notification");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_ok() {
        let resp = Response::ok(Some(json!(1)), json!({"status": "ok"}));
        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, Some(json!(1)));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[test]
    fn test_response_ok_null_id() {
        let resp = Response::ok(None, json!("done"));
        assert!(resp.id.is_none());
        assert!(resp.result.is_some());
    }

    #[test]
    fn test_response_err() {
        let resp = Response::err(Some(json!(42)), -32601, "Method not found");
        assert_eq!(resp.jsonrpc, "2.0");
        assert_eq!(resp.id, Some(json!(42)));
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err["code"], -32601);
        assert_eq!(err["message"], "Method not found");
    }

    #[test]
    fn test_response_err_null_id() {
        let resp = Response::err(None, -32700, "Parse error");
        assert!(resp.id.is_none());
        assert_eq!(resp.error.unwrap()["code"], -32700);
    }

    #[test]
    fn test_response_ok_serialization() {
        let resp = Response::ok(Some(json!(1)), json!({"key": "value"}));
        let s = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert!(v.get("error").is_none());
        assert_eq!(v["result"]["key"], "value");
    }

    #[test]
    fn test_response_err_serialization() {
        let resp = Response::err(Some(json!(2)), -32603, "Internal error");
        let s = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("result").is_none());
        assert_eq!(v["error"]["code"], -32603);
    }

    #[test]
    fn test_handle_initialize() {
        let resp = handle_initialize(Some(json!(1)));
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(result["serverInfo"]["name"], "deskd");
        assert!(resp.error.is_none());
    }

    #[test]
    fn test_handle_initialize_null_id() {
        let resp = handle_initialize(None);
        assert!(resp.id.is_none());
        assert!(resp.result.is_some());
    }
}
