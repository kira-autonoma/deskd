/// Discord adapter — connects to Discord Gateway and bridges messages to/from the agent bus.
///
/// Channel naming (consistent with Telegram adapter):
///   Incoming:  adapter publishes `discord.in:<channel_id>`
///   Outgoing:  adapter subscribes to `discord.out:<channel_id>` (glob: `discord.out:*`)
///
/// The adapter ignores messages from bots to prevent reply loops.
use anyhow::{Context, Result};
use serenity::async_trait;
use serenity::http::Http;
use serenity::model::channel::Message;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::DiscordRoute;

pub struct DiscordAdapter {
    token: String,
    routes: Vec<DiscordRoute>,
}

impl DiscordAdapter {
    pub fn new(token: String, routes: Vec<DiscordRoute>) -> Self {
        Self { token, routes }
    }
}

impl super::Adapter for DiscordAdapter {
    fn name(&self) -> &str {
        "discord"
    }

    fn run(self: Box<Self>, bus_socket: String, agent_name: String) -> super::BoxFuture {
        Box::pin(run(*self, bus_socket, agent_name))
    }
}

pub async fn run(adapter: DiscordAdapter, bus_socket: String, agent_name: String) -> Result<()> {
    info!(agent = %agent_name, "starting Discord adapter");

    let allowed_channels: HashSet<u64> = adapter.routes.iter().map(|r| r.channel_id).collect();
    let channel_names: HashMap<u64, String> = adapter
        .routes
        .iter()
        .filter_map(|r| r.name.as_ref().map(|n| (r.channel_id, n.clone())))
        .collect();

    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<(u64, String)>();

    let handler = DiscordHandler {
        inbound_tx,
        allowed_channels,
    };

    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&adapter.token, intents)
        .event_handler(handler)
        .await
        .context("failed to create Discord client")?;

    let http = client.http.clone();
    let adapter_name = format!("discord-{}", agent_name);

    // Task 1: Discord gateway (handles incoming messages via EventHandler)
    let gateway_task = {
        let agent = agent_name.clone();
        tokio::spawn(async move {
            if let Err(e) = client.start().await {
                tracing::error!(agent = %agent, error = %e, "Discord gateway failed");
            }
        })
    };

    // Task 2: Bus → Discord (outbound)
    let outbound_task = {
        let bus = bus_socket.clone();
        let name = adapter_name.clone();
        let http = http.clone();
        tokio::spawn(async move {
            if let Err(e) = bus_loop(&bus, &name, http).await {
                tracing::error!(error = %e, "discord bus loop failed");
            }
        })
    };

    // Task 3: Inbound messages from Discord → Bus
    let inbound_task = {
        let bus = bus_socket.clone();
        let agent = agent_name.clone();
        tokio::spawn(async move {
            inbound_loop(inbound_rx, &bus, &agent, &channel_names).await;
        })
    };

    info!(agent = %agent_name, "Discord adapter started");

    tokio::select! {
        _ = gateway_task => warn!(agent = %agent_name, "discord gateway task exited"),
        _ = outbound_task => warn!(agent = %agent_name, "discord outbound task exited"),
        _ = inbound_task => warn!(agent = %agent_name, "discord inbound task exited"),
    }

    Ok(())
}

struct DiscordHandler {
    inbound_tx: mpsc::UnboundedSender<(u64, String)>,
    allowed_channels: HashSet<u64>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(bot = %ready.user.name, "Discord bot connected");
    }

    async fn message(&self, _ctx: Context, msg: Message) {
        // Ignore bot messages to prevent reply loops.
        if msg.author.bot {
            return;
        }

        let channel_id = msg.channel_id.get();

        // Whitelist check.
        if !self.allowed_channels.is_empty() && !self.allowed_channels.contains(&channel_id) {
            debug!(channel_id = channel_id, "ignoring discord message — channel not in whitelist");
            return;
        }

        let text = msg.content.clone();
        if text.is_empty() {
            return;
        }

        debug!(channel_id = channel_id, "received Discord message");
        if self.inbound_tx.send((channel_id, text)).is_err() {
            warn!("discord inbound channel closed");
        }
    }
}

/// Subscribe to `discord.out:*` on the bus and forward messages to Discord.
async fn bus_loop(socket_path: &str, adapter_name: &str, http: Arc<Http>) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "discord adapter: failed to connect to bus at {}",
            socket_path
        )
    })?;

    let reg = serde_json::json!({
        "type": "register",
        "name": adapter_name,
        "subscriptions": ["discord.out:*"],
    });
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    info!(adapter = %adapter_name, "discord adapter registered on bus");

    let (reader, _writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "discord adapter: invalid message from bus");
                continue;
            }
        };

        let target = msg.get("target").and_then(|t| t.as_str()).unwrap_or("");

        if let Some(channel_id_str) = target.strip_prefix("discord.out:") {
            let channel_id: u64 = match channel_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    warn!(target = %target, "discord adapter: invalid channel_id in target");
                    continue;
                }
            };

            let text = msg
                .get("payload")
                .and_then(|p| {
                    p.get("result")
                        .or_else(|| p.get("task"))
                        .or_else(|| p.get("error"))
                })
                .and_then(|t| t.as_str())
                .unwrap_or("(no content)");

            debug!(channel_id = channel_id, "forwarding bus message to Discord");

            let channel = serenity::model::id::ChannelId::new(channel_id);
            if let Err(e) = channel.say(&http, text).await {
                warn!(channel_id = channel_id, error = %e, "failed to send Discord message");
            }
        }
    }

    Ok(())
}

/// Receive inbound Discord messages and publish them to the bus as `discord.in:<channel_id>`.
async fn inbound_loop(
    mut rx: mpsc::UnboundedReceiver<(u64, String)>,
    bus_socket: &str,
    agent_name: &str,
    channel_names: &HashMap<u64, String>,
) {
    while let Some((channel_id, text)) = rx.recv().await {
        let target = format!("discord.in:{}", channel_id);
        let reply_to = format!("discord.out:{}", channel_id);
        let channel_name = channel_names.get(&channel_id).cloned();

        if let Err(e) = publish_to_bus(
            bus_socket,
            agent_name,
            &text,
            &target,
            &reply_to,
            channel_id,
            channel_name,
        )
        .await
        {
            warn!(channel_id = channel_id, error = %e, "failed to publish Discord message to bus");
        }
    }
}

/// Publish an incoming Discord message to the bus as `discord.in:<channel_id>`.
async fn publish_to_bus(
    socket_path: &str,
    agent_name: &str,
    text: &str,
    target: &str,
    reply_to: &str,
    channel_id: u64,
    channel_name: Option<String>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("discord: failed to connect to bus at {}", socket_path))?;

    let reg = serde_json::json!({
        "type": "register",
        "name": format!("discord-in-{}", Uuid::new_v4()),
        "subscriptions": [],
    });
    let mut reg_line = serde_json::to_string(&reg)?;
    reg_line.push('\n');
    stream.write_all(reg_line.as_bytes()).await?;

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": format!("discord-{}", agent_name),
        "target": target,
        "payload": {
            "task": text,
            "discord_channel_id": channel_id,
            "discord_channel_name": channel_name,
        },
        "reply_to": reply_to,
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;

    Ok(())
}
