//! A2A HTTP server — serves Agent Card and handles incoming A2A JSON-RPC requests.
//!
//! Phase 2 of A2A protocol support (deskd#350).
//! Endpoints:
//! - `GET /.well-known/agent-card.json` — discovery
//! - `POST /a2a` — JSON-RPC 2.0 (tasks/send, tasks/get, tasks/cancel)

use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::app::a2a::AgentCard;

/// Shared state for the A2A HTTP server.
pub struct A2aState {
    /// Pre-built Agent Card JSON.
    pub agent_card: AgentCard,
    /// API key for authenticating incoming requests. None = no auth.
    pub api_key: Option<String>,
    /// Bus socket path for routing tasks to local agents.
    pub bus_socket: String,
}

/// Start the A2A HTTP server.
pub async fn serve(listen: &str, state: Arc<A2aState>) -> Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("A2A server listening on {}", listen);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the axum router (public for testing).
pub fn router(state: Arc<A2aState>) -> Router {
    Router::new()
        .route("/.well-known/agent-card.json", get(handle_agent_card))
        .route("/a2a", post(handle_a2a_rpc))
        .with_state(state)
}

/// GET /.well-known/agent-card.json — return the Agent Card.
async fn handle_agent_card(State(state): State<Arc<A2aState>>) -> Json<AgentCard> {
    Json(state.agent_card.clone())
}

// ─── JSON-RPC types ─────────────────────────────────────────────────────────

/// A2A JSON-RPC 2.0 request envelope.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A2A JSON-RPC 2.0 response envelope.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<serde_json::Value>, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message }),
        }
    }
}

/// POST /a2a — JSON-RPC 2.0 dispatcher.
async fn handle_a2a_rpc(
    State(state): State<Arc<A2aState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> (StatusCode, Json<JsonRpcResponse>) {
    // API key authentication.
    if let Some(expected_key) = &state.api_key {
        let provided = headers.get("x-api-key").and_then(|v| v.to_str().ok());
        if provided != Some(expected_key.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(JsonRpcResponse::error(
                    req.id,
                    -32000,
                    "unauthorized: invalid or missing API key".into(),
                )),
            );
        }
    }

    match req.method.as_str() {
        "tasks/send" => handle_tasks_send(req.id, &req.params, &state).await,
        "tasks/get" => handle_tasks_get(req.id, &req.params),
        "tasks/cancel" => handle_tasks_cancel(req.id, &req.params),
        _ => (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                req.id,
                -32601,
                format!("method not found: {}", req.method),
            )),
        ),
    }
}

/// tasks/send — create a task and route to a local agent via bus.
async fn handle_tasks_send(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    state: &A2aState,
) -> (StatusCode, Json<JsonRpcResponse>) {
    let skill_id = params.get("skill").and_then(|v| v.as_str()).unwrap_or("");
    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");

    if skill_id.is_empty() || message.is_empty() {
        return (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                id,
                -32602,
                "missing required params: skill, message".into(),
            )),
        );
    }

    // Extract agent name from skill_id (format: "agent_name/skill_name").
    let agent_name = skill_id.split('/').next().unwrap_or("");
    if agent_name.is_empty() {
        return (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                id,
                -32602,
                format!("invalid skill id format: {skill_id} (expected agent/skill)"),
            )),
        );
    }

    // Route to agent via bus.
    let target = format!("agent:{agent_name}");
    let task_id = uuid::Uuid::new_v4().to_string();

    match crate::app::bus::send_message(&state.bus_socket, "a2a", &target, message).await {
        Ok(_) => (
            StatusCode::OK,
            Json(JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "taskId": task_id,
                    "status": "working",
                    "skill": skill_id,
                    "agent": agent_name,
                }),
            )),
        ),
        Err(e) => (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                id,
                -32000,
                format!("failed to route task to agent: {e}"),
            )),
        ),
    }
}

/// tasks/get — check task status (stub for Phase 2).
fn handle_tasks_get(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
) -> (StatusCode, Json<JsonRpcResponse>) {
    let task_id = params.get("taskId").and_then(|v| v.as_str()).unwrap_or("");

    if task_id.is_empty() {
        return (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                id,
                -32602,
                "missing required param: taskId".into(),
            )),
        );
    }

    // Phase 2 stub — task tracking will be added in a later phase.
    (
        StatusCode::OK,
        Json(JsonRpcResponse::error(
            id,
            -32601,
            "tasks/get not yet implemented (Phase 3)".into(),
        )),
    )
}

/// tasks/cancel — cancel a running task (stub for Phase 2).
fn handle_tasks_cancel(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
) -> (StatusCode, Json<JsonRpcResponse>) {
    let task_id = params.get("taskId").and_then(|v| v.as_str()).unwrap_or("");

    if task_id.is_empty() {
        return (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                id,
                -32602,
                "missing required param: taskId".into(),
            )),
        );
    }

    (
        StatusCode::OK,
        Json(JsonRpcResponse::error(
            id,
            -32601,
            "tasks/cancel not yet implemented (Phase 3)".into(),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn make_state(api_key: Option<&str>) -> Arc<A2aState> {
        Arc::new(A2aState {
            agent_card: crate::app::a2a::AgentCard {
                name: "test".into(),
                description: Some("Test instance".into()),
                url: "https://test.example.com".into(),
                version: "0.1.0".into(),
                capabilities: crate::app::a2a::AgentCapabilities {
                    streaming: true,
                    push_notifications: false,
                },
                skills: vec![crate::app::a2a::AgentSkill {
                    id: "dev/review".into(),
                    name: "Review".into(),
                    description: "Code review".into(),
                    tags: vec!["rust".into()],
                }],
                needs: vec![],
                authentication: crate::app::a2a::AgentAuthentication {
                    schemes: if api_key.is_some() {
                        vec!["apiKey".into()]
                    } else {
                        vec![]
                    },
                },
            },
            api_key: api_key.map(String::from),
            bus_socket: "/tmp/test.sock".into(),
        })
    }

    #[tokio::test]
    async fn agent_card_endpoint() {
        let app = router(make_state(None));
        let req = Request::get("/.well-known/agent-card.json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let card: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(card["name"], "test");
        assert_eq!(card["url"], "https://test.example.com");
        assert_eq!(card["skills"][0]["id"], "dev/review");
    }

    #[tokio::test]
    async fn a2a_rpc_auth_required() {
        let app = router(make_state(Some("secret-key")));
        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/send","params":{"skill":"dev/review","message":"hi"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a2a_rpc_auth_valid() {
        let app = router(make_state(Some("secret-key")));
        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .header("x-api-key", "secret-key")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"taskId":"abc"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Should pass auth (even though tasks/get is a stub, it returns 200 with error)
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a2a_rpc_unknown_method() {
        let app = router(make_state(None));
        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"unknown/method","params":{}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rpc["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn a2a_rpc_missing_params() {
        let app = router(make_state(None));
        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/send","params":{}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rpc["error"]["code"], -32602);
        assert!(
            rpc["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing")
        );
    }
}
