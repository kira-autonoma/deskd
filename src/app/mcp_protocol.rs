/// MCP JSON-RPC protocol types and I/O helpers.
///
/// Handles request/response framing, serialization, and bus channel
/// notifications for the MCP server.
///
/// Channel protocol (#451): the channel mode (`deskd mcp-channel`) emits
/// `notifications/claude/channel` events for Telegram inbox messages. The
/// channel capability and meta-key sanitization rules follow the Claude Code
/// channels reference: `https://code.claude.com/docs/en/channels-reference`.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::io::ErrorKind;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

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

// --- Server mode ---

/// Runtime mode for the MCP server: plain MCP tools, or Claude Code Channels
/// protocol (#451) with channel notifications + `reply` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerMode {
    /// Standard MCP: `deskd` server name, no channel capability, no reply tool.
    Plain,
    /// Channels mode: `deskd-telegram` server name, `experimental.claude/channel`
    /// capability advertised, `reply` tool registered, Telegram-inbox bus
    /// messages emitted as `notifications/claude/channel` frames.
    Channel,
}

impl ServerMode {
    /// Server name surfaced in the initialize response's `serverInfo.name`.
    ///
    /// In channel mode this is what Claude Code uses as the `source` attribute
    /// on the rendered `<channel source="...">` tag, so it must be distinct
    /// from other channel sources (e.g. future `deskd-github`).
    pub fn server_name(self) -> &'static str {
        match self {
            ServerMode::Plain => "deskd",
            ServerMode::Channel => "deskd-telegram",
        }
    }
}

// --- Protocol handlers ---

/// Build the `initialize` response for the given mode.
///
/// In `Channel` mode the response declares the `experimental.claude/channel`
/// capability and an `instructions` string per the channels spec — these are
/// what register the notification listener on the Claude Code side.
pub(crate) fn handle_initialize(id: Option<Value>, mode: ServerMode) -> Response {
    let mut capabilities = json!({ "tools": {} });
    if mode == ServerMode::Channel {
        capabilities["experimental"] = json!({ "claude/channel": {} });
    }

    let mut result = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": capabilities,
        "serverInfo": {
            "name": mode.server_name(),
            "version": env!("CARGO_PKG_VERSION")
        }
    });

    if mode == ServerMode::Channel {
        // Instruction string is added to Claude's system prompt — tells the
        // model how to recognise inbound Telegram events and how to reply.
        result["instructions"] = Value::String(
            "Telegram messages arrive as \
             <channel source=\"deskd-telegram\" chat_id=\"...\" user_id=\"...\">body</channel>. \
             To reply, call the `reply` tool with the chat_id from the tag and the message text."
                .to_string(),
        );
    }

    Response::ok(id, result)
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
/// Returns a line reader for incoming bus messages, or `None` if connection fails
/// (MCP server degrades gracefully — tool calls still work without push).
///
/// In channel mode the listener also subscribes to `telegram.in:*` so Telegram
/// messages routed at the chat (rather than re-targeted to the agent) still
/// surface as channel events.
pub(crate) async fn connect_bus_listener(
    agent_name: &str,
    bus_socket: &str,
    mode: ServerMode,
) -> Option<Arc<Mutex<tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>>>> {
    let stream = match UnixStream::connect(bus_socket).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to connect bus listener -- channel notifications disabled");
            return None;
        }
    };

    let (read_half, mut write_half) = stream.into_split();

    let mut subscriptions = vec![
        format!("agent:{}", agent_name),
        format!("reply:{}:*", agent_name),
    ];
    if mode == ServerMode::Channel {
        // Pick up the raw `telegram.in:<chat_id>` stream so the channel
        // server emits events even if the workspace hasn't wired a
        // `route_to` override that retargets to `agent:<name>`.
        subscriptions.push("telegram.in:*".to_string());
    }

    let reg = json!({
        "type": "register",
        "name": format!("{}-mcp-channel", agent_name),
        "subscriptions": subscriptions,
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

    info!(agent = %agent_name, mode = ?mode, "bus channel listener connected");

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

// --- Channel notification building ---

/// Channels-spec meta-key alphabet: ASCII letter, digit, or underscore (#451).
///
/// Per the channels reference, "Keys must be identifiers: letters, digits, and
/// underscores only. Keys containing hyphens or other characters are silently
/// dropped." This function exposes that predicate so callers can filter their
/// own meta maps consistently.
pub(crate) fn is_valid_meta_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Filter a meta map down to the keys that survive channels-spec sanitization.
///
/// All values are coerced to strings (per the spec `meta` is
/// `Record<string, string>`). Non-conforming keys are dropped silently — they
/// match the docs' behavior, and the meta object is best-effort routing
/// context, not load-bearing.
pub(crate) fn sanitize_meta(input: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in input {
        if !is_valid_meta_key(k) {
            continue;
        }
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Null => continue,
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            // Arrays/objects: serialize to JSON so meta stays string-valued.
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        out.insert(k.clone(), Value::String(s));
    }
    out
}

/// Build the channel notification payload for a Telegram-flavoured bus
/// message. Always returns a frame conforming to the channels spec; the meta
/// keys are restricted to the sanitized identifier set.
///
/// Recognised payload fields (Telegram adapter convention):
/// * `task` / `result` / `text` — first non-empty becomes `content`
/// * `telegram_chat_id` — surfaces as `chat_id`
/// * `telegram_sender.id` — surfaces as `user_id`
/// * `telegram_chat_name` — surfaces as `chat_name` (sanitized if present)
/// * `telegram_message_id` — surfaces as `message_id`
pub(crate) fn build_channel_notification(msg: &BusMessage) -> Value {
    let content = extract_content(msg);
    let mut raw_meta = Map::new();

    if let Some(chat_id) = msg.payload.get("telegram_chat_id")
        && let Some(s) = value_to_string(chat_id)
    {
        raw_meta.insert("chat_id".to_string(), Value::String(s));
    }
    if let Some(sender) = msg.payload.get("telegram_sender")
        && let Some(uid) = sender.get("id")
        && let Some(s) = value_to_string(uid)
    {
        raw_meta.insert("user_id".to_string(), Value::String(s));
    }
    if let Some(chat_name) = msg
        .payload
        .get("telegram_chat_name")
        .and_then(|v| v.as_str())
        && !chat_name.is_empty()
    {
        raw_meta.insert(
            "chat_name".to_string(),
            Value::String(chat_name.to_string()),
        );
    }
    if let Some(mid) = msg.payload.get("telegram_message_id")
        && let Some(s) = value_to_string(mid)
    {
        raw_meta.insert("message_id".to_string(), Value::String(s));
    }
    // Fallback routing context: include source/target so non-Telegram bus
    // messages (e.g. inter-agent reminders) still carry breadcrumbs.
    raw_meta.insert("source".to_string(), Value::String(msg.source.clone()));
    raw_meta.insert("target".to_string(), Value::String(msg.target.clone()));
    raw_meta.insert("bus_id".to_string(), Value::String(msg.id.clone()));

    let meta = sanitize_meta(&raw_meta);

    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": Value::Object(meta),
        }
    })
}

fn extract_content(msg: &BusMessage) -> String {
    for key in ["task", "text", "result", "message"] {
        if let Some(s) = msg.payload.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    serde_json::to_string(&msg.payload).unwrap_or_else(|_| String::new())
}

fn value_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Emit a channel notification to stdout for a bus message.
///
/// **Best-effort delivery**: per the channels spec, notifications are not
/// acknowledged. If the stdout write fails with `BrokenPipe` (the Claude Code
/// session ended), the error is logged at debug level and swallowed — the
/// caller keeps reading from the bus.
pub(crate) async fn emit_channel_notification<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &BusMessage,
) -> Result<()> {
    let notification = build_channel_notification(msg);
    let json_str = serde_json::to_string(&notification)?;

    if let Err(e) = writer.write_all(json_str.as_bytes()).await {
        return handle_write_err(e, "write");
    }
    if let Err(e) = writer.write_all(b"\n").await {
        return handle_write_err(e, "newline");
    }
    if let Err(e) = writer.flush().await {
        return handle_write_err(e, "flush");
    }

    info!(source = %msg.source, "emitted channel notification");
    Ok(())
}

fn handle_write_err(e: tokio::io::Error, stage: &str) -> Result<()> {
    if e.kind() == ErrorKind::BrokenPipe {
        debug!(
            stage = stage,
            "channel notification dropped — stdout pipe broken (no listener)"
        );
        return Ok(());
    }
    Err(anyhow::Error::from(e).context(format!("channel notification {} failed", stage)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::bus_wire::BusMetadata;

    fn bus_msg(payload: Value) -> BusMessage {
        BusMessage {
            id: "msg-1".to_string(),
            source: "telegram-dev".to_string(),
            target: "telegram.in:12345".to_string(),
            payload,
            reply_to: Some("telegram.out:12345".to_string()),
            metadata: BusMetadata::default(),
        }
    }

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
    fn test_response_err() {
        let resp = Response::err(Some(json!(42)), -32601, "Method not found");
        assert_eq!(resp.error.unwrap()["code"], -32601);
    }

    #[test]
    fn test_handle_initialize_plain_mode() {
        let resp = handle_initialize(Some(json!(1)), ServerMode::Plain);
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "deskd");
        // Plain mode must not advertise the channel capability.
        assert!(result["capabilities"]["experimental"].is_null());
        // No instructions in plain mode (avoids leaking channel-only docs).
        assert!(result.get("instructions").is_none());
    }

    #[test]
    fn test_handle_initialize_channel_mode() {
        let resp = handle_initialize(Some(json!(1)), ServerMode::Channel);
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "deskd-telegram");
        // experimental.claude/channel: {} registers the listener — required.
        assert!(result["capabilities"]["experimental"]["claude/channel"].is_object());
        // tools capability is still present so reply is callable.
        assert!(result["capabilities"]["tools"].is_object());
        // Instructions reference both the <channel> tag shape and the reply tool.
        let instr = result["instructions"].as_str().unwrap();
        assert!(instr.contains("<channel"));
        assert!(instr.contains("deskd-telegram"));
        assert!(instr.contains("reply"));
        assert!(instr.contains("chat_id"));
    }

    #[test]
    fn test_is_valid_meta_key() {
        assert!(is_valid_meta_key("chat_id"));
        assert!(is_valid_meta_key("user_id"));
        assert!(is_valid_meta_key("Source42"));
        assert!(is_valid_meta_key("_under"));
        assert!(!is_valid_meta_key(""));
        // Hyphens are silently dropped per the channels reference.
        assert!(!is_valid_meta_key("chat-id"));
        assert!(!is_valid_meta_key("user.id"));
        assert!(!is_valid_meta_key("a b"));
        assert!(!is_valid_meta_key("emoji🙂"));
    }

    #[test]
    fn test_sanitize_meta_drops_invalid_keys() {
        let mut input = Map::new();
        input.insert("chat_id".to_string(), json!("123"));
        input.insert("chat-id".to_string(), json!("123")); // dropped
        input.insert("user.id".to_string(), json!(456));
        input.insert("plain".to_string(), json!(true));
        input.insert("nulled".to_string(), json!(null)); // dropped
        let out = sanitize_meta(&input);
        assert!(out.contains_key("chat_id"));
        assert!(!out.contains_key("chat-id"));
        assert!(!out.contains_key("user.id"));
        assert_eq!(out["chat_id"], json!("123"));
        // numbers and bools coerced to strings.
        assert_eq!(out["plain"], json!("true"));
        assert!(!out.contains_key("nulled"));
    }

    #[test]
    fn test_sanitize_meta_coerces_numbers_to_strings() {
        // Spec: meta is Record<string, string>. Numbers must come out as strings.
        let mut input = Map::new();
        input.insert("chat_id".to_string(), json!(12345));
        input.insert("user_id".to_string(), json!(67890));
        let out = sanitize_meta(&input);
        assert_eq!(out["chat_id"], json!("12345"));
        assert_eq!(out["user_id"], json!("67890"));
    }

    #[test]
    fn test_build_channel_notification_telegram_payload() {
        let msg = bus_msg(json!({
            "task": "hello from telegram",
            "telegram_chat_id": -100123,
            "telegram_sender": {"id": 4242, "username": "alice"},
            "telegram_message_id": 7,
            "telegram_chat_name": "kira channel",
        }));
        let frame = build_channel_notification(&msg);
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["method"], "notifications/claude/channel");
        assert_eq!(frame["params"]["content"], "hello from telegram");
        let meta = &frame["params"]["meta"];
        assert_eq!(meta["chat_id"], json!("-100123"));
        assert_eq!(meta["user_id"], json!("4242"));
        assert_eq!(meta["message_id"], json!("7"));
        assert_eq!(meta["chat_name"], json!("kira channel"));
        // bus_id/source/target stay as routing breadcrumbs.
        assert_eq!(meta["bus_id"], json!("msg-1"));
        assert_eq!(meta["source"], json!("telegram-dev"));
    }

    #[test]
    fn test_build_channel_notification_falls_back_to_payload_json() {
        // Non-Telegram bus message: still produces a valid notification with
        // routing context — channel servers MUST emit syntactically valid
        // frames or Claude Code drops them.
        let msg = bus_msg(json!({"foo": "bar"}));
        let frame = build_channel_notification(&msg);
        let content = frame["params"]["content"].as_str().unwrap();
        assert!(content.contains("foo"));
        let meta = &frame["params"]["meta"];
        assert_eq!(meta["source"], json!("telegram-dev"));
        // No chat_id when there's no telegram_chat_id in the payload.
        assert!(meta.get("chat_id").is_none());
    }

    #[test]
    fn test_emit_channel_notification_swallows_broken_pipe() {
        // Custom AsyncWrite that fails with BrokenPipe on first write.
        struct BrokenWriter;
        impl tokio::io::AsyncWrite for BrokenWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::new(
                    ErrorKind::BrokenPipe,
                    "no listener",
                )))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let msg = bus_msg(json!({"task": "drop me"}));
        let mut w = BrokenWriter;
        // Per channels spec, a broken stdio pipe must NOT propagate as an
        // error — channels are best-effort, no listener means drop silently.
        let res = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(emit_channel_notification(&mut w, &msg));
        assert!(res.is_ok(), "broken-pipe emit must succeed silently");
    }

    #[test]
    fn test_emit_channel_notification_writes_newline_delimited_json() {
        let msg = bus_msg(json!({
            "task": "ping",
            "telegram_chat_id": 1,
            "telegram_sender": {"id": 2},
        }));
        let mut buf: Vec<u8> = Vec::new();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(emit_channel_notification(&mut buf, &msg))
            .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.ends_with('\n'));
        let parsed: Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(parsed["method"], "notifications/claude/channel");
        assert_eq!(parsed["params"]["content"], "ping");
        assert_eq!(parsed["params"]["meta"]["chat_id"], "1");
    }

    #[test]
    fn test_server_mode_names() {
        assert_eq!(ServerMode::Plain.server_name(), "deskd");
        // This name is exposed as the `source` attribute on the <channel> tag;
        // changing it is a breaking change for any agent system prompt
        // referencing `<channel source="deskd-telegram">`.
        assert_eq!(ServerMode::Channel.server_name(), "deskd-telegram");
    }
}
