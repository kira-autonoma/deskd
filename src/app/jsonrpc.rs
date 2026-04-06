//! JSON-RPC 2.0 protocol types and helpers.
//!
//! Generic JSON-RPC 2.0 framing used by ACP and other protocols.
//! No application-specific logic — just wire types and parsing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A JSON-RPC 2.0 request (client -> server).
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        }
    }

    /// Serialize to a newline-delimited JSON string.
    pub fn to_line(&self) -> Result<String> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// A JSON-RPC 2.0 response (server -> client).
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<u64>,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
    /// For notifications — the method name.
    pub method: Option<String>,
    /// For notifications — the params.
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// Parse a line of newline-delimited JSON into a JsonRpcResponse.
pub fn parse_response(line: &str) -> Result<JsonRpcResponse> {
    serde_json::from_str(line).context("failed to parse JSON-RPC response")
}

/// Classify incoming JSON-RPC messages into three categories.
#[derive(Debug, PartialEq)]
pub enum MessageKind {
    /// Server request: has both `id` and `method` (e.g. `session/request_permission`).
    ServerRequest,
    /// Notification: has `method` but no `id` (e.g. `session/update`).
    Notification,
    /// Response to a client request: has `id` but no `method`.
    Response,
}

pub fn classify_message(resp: &JsonRpcResponse) -> MessageKind {
    match (resp.id.is_some(), resp.method.is_some()) {
        (true, true) => MessageKind::ServerRequest,
        (false, true) => MessageKind::Notification,
        _ => MessageKind::Response,
    }
}

/// Check if a parsed response is a notification (no id, has method).
pub fn is_notification(resp: &JsonRpcResponse) -> bool {
    resp.id.is_none() && resp.method.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonrpc_request_serialization() {
        let req = JsonRpcRequest::new(1, "initialize", Some(serde_json::json!({"key": "val"})));
        let line = req.to_line().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "initialize");
        assert_eq!(parsed["params"]["key"], "val");
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn test_jsonrpc_request_no_params() {
        let req = JsonRpcRequest::new(42, "test/method", None);
        let line = req.to_line().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "test/method");
        assert!(parsed.get("params").is_none());
    }

    #[test]
    fn test_parse_response_success() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"abc-123"}}"#;
        let resp = parse_response(json).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
        assert!(resp.method.is_none());
    }

    #[test]
    fn test_parse_response_error() {
        let json =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let resp = parse_response(json).unwrap();
        assert_eq!(resp.id, Some(2));
        assert!(resp.error.is_some());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "Invalid request");
    }

    #[test]
    fn test_parse_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"session/update","params":{"status":"working","messages":[]}}"#;
        let resp = parse_response(json).unwrap();
        assert!(is_notification(&resp));
        assert_eq!(resp.method.as_deref(), Some("session/update"));
        assert!(resp.id.is_none());
    }

    #[test]
    fn test_is_notification() {
        let notification = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            result: None,
            error: None,
            method: Some("session/update".to_string()),
            params: Some(serde_json::json!({})),
        };
        assert!(is_notification(&notification));

        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            result: Some(serde_json::json!({})),
            error: None,
            method: None,
            params: None,
        };
        assert!(!is_notification(&response));
    }

    #[test]
    fn test_classify_message_server_request() {
        let server_req = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(42),
            result: None,
            error: None,
            method: Some("session/request_permission".to_string()),
            params: Some(serde_json::json!({"tool": "bash"})),
        };
        assert_eq!(classify_message(&server_req), MessageKind::ServerRequest);
    }

    #[test]
    fn test_classify_message_notification() {
        let notification = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            result: None,
            error: None,
            method: Some("session/update".to_string()),
            params: Some(serde_json::json!({})),
        };
        assert_eq!(classify_message(&notification), MessageKind::Notification);
    }

    #[test]
    fn test_classify_message_response() {
        let response = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(1),
            result: Some(serde_json::json!({})),
            error: None,
            method: None,
            params: None,
        };
        assert_eq!(classify_message(&response), MessageKind::Response);
    }

    #[test]
    fn test_jsonrpc_error_display() {
        let err = JsonRpcError {
            code: -32600,
            message: "Invalid request".to_string(),
            data: None,
        };
        assert_eq!(format!("{}", err), "JSON-RPC error -32600: Invalid request");
    }
}
