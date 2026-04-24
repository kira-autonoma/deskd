//! A2A HTTP server — serves Agent Card and handles incoming A2A JSON-RPC requests.
//!
//! Phase 2 of A2A protocol support (deskd#350).
//! Endpoints:
//! - `GET /.well-known/agent-card.json` — discovery
//! - `POST /a2a` — JSON-RPC 2.0 (tasks/send, tasks/get, tasks/cancel)

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::app::a2a::AgentCard;

/// Default capacity for the in-memory A2A task registry.
/// Older tasks are evicted in FIFO order once the cap is reached.
const DEFAULT_TASK_REGISTRY_CAPACITY: usize = 1024;

/// One row in the A2A task registry, tracking a task accepted via `tasks/send`.
///
/// Task completion is not observable from `tasks/send` today (bus routing is
/// fire-and-forget), so newly-created tasks stay in `working` until explicitly
/// cancelled via `tasks/cancel`. A later phase will wire task completion into
/// this registry.
#[derive(Debug, Clone, Serialize)]
pub struct A2aTaskRecord {
    pub task_id: String,
    pub skill: String,
    pub agent: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Bounded in-memory store of recent A2A tasks.
///
/// Lookups go through `HashMap` (O(1)); eviction is FIFO via `VecDeque`. Once
/// `capacity` is reached, the oldest entry is dropped. Shared state is guarded
/// by a single `Mutex` — contention is low since the registry is only touched
/// on the task RPC hot path.
pub struct A2aTaskRegistry {
    inner: Mutex<RegistryInner>,
    capacity: usize,
}

struct RegistryInner {
    tasks: HashMap<String, A2aTaskRecord>,
    order: VecDeque<String>,
}

impl A2aTaskRegistry {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                tasks: HashMap::new(),
                order: VecDeque::new(),
            }),
            capacity: capacity.max(1),
        }
    }

    /// Insert a freshly-created task. Evicts the oldest entry when full.
    pub fn insert(&self, record: A2aTaskRecord) {
        let mut inner = self.inner.lock().expect("A2A registry mutex poisoned");
        if inner.tasks.len() >= self.capacity
            && let Some(oldest) = inner.order.pop_front()
        {
            inner.tasks.remove(&oldest);
        }
        inner.order.push_back(record.task_id.clone());
        inner.tasks.insert(record.task_id.clone(), record);
    }

    /// Return a clone of the task record, if present.
    pub fn get(&self, task_id: &str) -> Option<A2aTaskRecord> {
        let inner = self.inner.lock().expect("A2A registry mutex poisoned");
        inner.tasks.get(task_id).cloned()
    }

    /// Transition an existing task to the given status, updating `updated_at`.
    /// Returns the updated record, or `None` if the task is unknown.
    pub fn set_status(&self, task_id: &str, status: &str) -> Option<A2aTaskRecord> {
        let mut inner = self.inner.lock().expect("A2A registry mutex poisoned");
        let record = inner.tasks.get_mut(task_id)?;
        record.status = status.to_string();
        record.updated_at = Utc::now();
        Some(record.clone())
    }
}

impl Default for A2aTaskRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_TASK_REGISTRY_CAPACITY)
    }
}

/// Shared state for the A2A HTTP server.
pub struct A2aState {
    /// Pre-built Agent Card JSON.
    pub agent_card: AgentCard,
    /// API key for authenticating incoming requests. None = no auth.
    pub api_key: Option<String>,
    /// Bus socket path for routing tasks to local agents.
    pub bus_socket: String,
    /// Authentication mode: "api_key", "jwt", or "none".
    pub auth_mode: String,
    /// Trusted public keys for JWT verification (raw Ed25519 bytes).
    /// Incoming JWTs are verified against each key until one matches.
    pub trusted_keys: Vec<Vec<u8>>,
    /// In-memory registry of A2A tasks created via `tasks/send`.
    pub tasks: A2aTaskRegistry,
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
    // Authentication.
    if let Some(auth_error) = check_auth(&state, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(JsonRpcResponse::error(req.id, -32000, auth_error)),
        );
    }

    match req.method.as_str() {
        "tasks/send" => handle_tasks_send(req.id, &req.params, &state).await,
        "tasks/get" => handle_tasks_get(req.id, &req.params, &state),
        "tasks/cancel" => handle_tasks_cancel(req.id, &req.params, &state),
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
        Ok(_) => {
            let now = Utc::now();
            state.tasks.insert(A2aTaskRecord {
                task_id: task_id.clone(),
                skill: skill_id.to_string(),
                agent: agent_name.to_string(),
                status: "working".to_string(),
                created_at: now,
                updated_at: now,
            });
            (
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
            )
        }
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

/// tasks/get — look up a task in the in-memory registry.
///
/// Returns the current record for tasks that were accepted by this server via
/// `tasks/send`. Tasks remain in `working` until explicitly cancelled or
/// evicted by the registry's FIFO cap. Unknown task IDs return a -32002
/// "task not found" error, per the A2A convention.
fn handle_tasks_get(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    state: &A2aState,
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

    match state.tasks.get(task_id) {
        Some(record) => (
            StatusCode::OK,
            Json(JsonRpcResponse::success(
                id,
                serde_json::to_value(&record).unwrap_or_else(|_| serde_json::json!({})),
            )),
        ),
        None => (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                id,
                -32002,
                format!("task not found: {task_id}"),
            )),
        ),
    }
}

/// tasks/cancel — mark a registered task as cancelled.
///
/// The bus layer is fire-and-forget so there is nothing to physically stop,
/// but the registry entry transitions to `cancelled` so subsequent `tasks/get`
/// calls report the new state. Unknown task IDs return -32002.
fn handle_tasks_cancel(
    id: Option<serde_json::Value>,
    params: &serde_json::Value,
    state: &A2aState,
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

    match state.tasks.set_status(task_id, "cancelled") {
        Some(record) => (
            StatusCode::OK,
            Json(JsonRpcResponse::success(
                id,
                serde_json::to_value(&record).unwrap_or_else(|_| serde_json::json!({})),
            )),
        ),
        None => (
            StatusCode::OK,
            Json(JsonRpcResponse::error(
                id,
                -32002,
                format!("task not found: {task_id}"),
            )),
        ),
    }
}

/// Check authentication based on the configured auth mode.
/// Returns None if auth passes, Some(error_message) if auth fails.
fn check_auth(state: &A2aState, headers: &axum::http::HeaderMap) -> Option<String> {
    match state.auth_mode.as_str() {
        "none" => None,
        "jwt" => {
            if state.trusted_keys.is_empty() {
                return Some("server misconfigured: no trusted keys for JWT verification".into());
            }
            let token = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "));

            match token {
                None => Some("unauthorized: missing Bearer token".into()),
                Some(t) => {
                    // Try each trusted key — accept if any matches.
                    for key in &state.trusted_keys {
                        if crate::app::a2a_jwt::verify_jwt(t, key).is_ok() {
                            return None;
                        }
                    }
                    Some("unauthorized: JWT signature does not match any trusted key".into())
                }
            }
        }
        _ => {
            // Default: api_key mode.
            if let Some(expected_key) = &state.api_key {
                let provided = headers.get("x-api-key").and_then(|v| v.to_str().ok());
                if provided != Some(expected_key.as_str()) {
                    return Some("unauthorized: invalid or missing API key".into());
                }
            }
            None
        }
    }
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
                    jwks: None,
                },
            },
            api_key: api_key.map(String::from),
            bus_socket: "/tmp/test.sock".into(),
            auth_mode: if api_key.is_some() {
                "api_key".into()
            } else {
                "none".into()
            },
            trusted_keys: vec![],
            tasks: A2aTaskRegistry::default(),
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

    /// Create a JWT-authenticated server state with a *sender* key pair.
    /// The server trusts the sender's public key. The sender signs with its private key.
    /// This mirrors real usage: sender and server have *different* identities.
    fn make_jwt_state() -> (Arc<A2aState>, crate::app::a2a_jwt::KeyPair) {
        let sender_kp = crate::app::a2a_jwt::KeyPair::generate().unwrap();
        let state = Arc::new(A2aState {
            agent_card: crate::app::a2a::AgentCard {
                name: "test".into(),
                description: None,
                url: "https://test.example.com".into(),
                version: "0.1.0".into(),
                capabilities: crate::app::a2a::AgentCapabilities {
                    streaming: true,
                    push_notifications: false,
                },
                skills: vec![],
                needs: vec![],
                authentication: crate::app::a2a::AgentAuthentication {
                    schemes: vec!["jwt".into()],
                    jwks: None,
                },
            },
            api_key: None,
            bus_socket: "/tmp/test.sock".into(),
            auth_mode: "jwt".into(),
            trusted_keys: vec![sender_kp.public_key_bytes().to_vec()],
            tasks: A2aTaskRegistry::default(),
        });
        (state, sender_kp)
    }

    #[tokio::test]
    async fn a2a_jwt_auth_valid() {
        let (state, kp) = make_jwt_state();
        let app = router(state);
        let token = kp.sign_jwt("https://sender.example.com", 60).unwrap();

        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", token))
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"taskId":"abc"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a2a_jwt_auth_missing_token() {
        let (state, _kp) = make_jwt_state();
        let app = router(state);

        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"taskId":"abc"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ─── Task registry tests ────────────────────────────────────────────────

    fn make_record(id: &str) -> A2aTaskRecord {
        let now = Utc::now();
        A2aTaskRecord {
            task_id: id.into(),
            skill: "dev/review".into(),
            agent: "dev".into(),
            status: "working".into(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn task_registry_insert_and_get() {
        let reg = A2aTaskRegistry::new(8);
        reg.insert(make_record("task-1"));
        let got = reg.get("task-1").expect("task-1 should exist");
        assert_eq!(got.status, "working");
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn task_registry_fifo_eviction() {
        let reg = A2aTaskRegistry::new(2);
        reg.insert(make_record("a"));
        reg.insert(make_record("b"));
        reg.insert(make_record("c"));
        // "a" is the oldest and must have been evicted.
        assert!(reg.get("a").is_none());
        assert!(reg.get("b").is_some());
        assert!(reg.get("c").is_some());
    }

    #[test]
    fn task_registry_set_status() {
        let reg = A2aTaskRegistry::new(4);
        reg.insert(make_record("task-1"));
        let before = reg.get("task-1").unwrap();
        // Ensure updated_at actually advances.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let updated = reg
            .set_status("task-1", "cancelled")
            .expect("known task is returned");
        assert_eq!(updated.status, "cancelled");
        assert!(updated.updated_at >= before.updated_at);
        assert!(reg.set_status("unknown", "cancelled").is_none());
    }

    #[tokio::test]
    async fn a2a_tasks_get_unknown_returns_not_found() {
        let app = router(make_state(None));
        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"taskId":"does-not-exist"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rpc["error"]["code"], -32002);
        assert!(
            rpc["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
    }

    #[tokio::test]
    async fn a2a_tasks_get_returns_registered_task() {
        let state = make_state(None);
        state.tasks.insert(make_record("t-known"));
        let app = router(state);

        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"taskId":"t-known"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rpc["result"]["task_id"], "t-known");
        assert_eq!(rpc["result"]["status"], "working");
        assert_eq!(rpc["result"]["skill"], "dev/review");
    }

    #[tokio::test]
    async fn a2a_tasks_cancel_transitions_status() {
        let state = make_state(None);
        state.tasks.insert(make_record("t-cancel"));
        let app = router(Arc::clone(&state));

        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/cancel","params":{"taskId":"t-cancel"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rpc["result"]["status"], "cancelled");

        // Subsequent get must reflect the cancelled status.
        assert_eq!(state.tasks.get("t-cancel").unwrap().status, "cancelled");
    }

    #[tokio::test]
    async fn a2a_tasks_cancel_unknown_returns_not_found() {
        let app = router(make_state(None));
        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/cancel","params":{"taskId":"nope"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rpc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(rpc["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn a2a_jwt_auth_wrong_key() {
        let (state, _kp) = make_jwt_state();
        let app = router(state);
        // Sign with a different key.
        let other_kp = crate::app::a2a_jwt::KeyPair::generate().unwrap();
        let token = other_kp.sign_jwt("https://evil.example.com", 60).unwrap();

        let req = Request::post("/a2a")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", token))
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{"taskId":"abc"}}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
