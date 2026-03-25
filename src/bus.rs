use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, RwLock};

use crate::message::{Envelope, Message};

type Tx = mpsc::UnboundedSender<Message>;

struct Client {
    name: String,
    tx: Tx,
    subscriptions: HashSet<String>,
}

struct BusState {
    clients: HashMap<String, Client>,
}

impl BusState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    fn route(&self, msg: &Message) {
        let target = &msg.target;

        if target == "broadcast" {
            for client in self.clients.values() {
                if client.name != msg.source {
                    let _ = client.tx.send(msg.clone());
                }
            }
        } else if let Some(name) = target.strip_prefix("agent:") {
            if let Some(client) = self.clients.get(name) {
                let _ = client.tx.send(msg.clone());
            } else {
                eprintln!("[bus] no such agent: {}", name);
            }
        } else if target.starts_with("queue:") {
            for client in self.clients.values() {
                if client.subscriptions.contains(target) && client.name != msg.source {
                    let _ = client.tx.send(msg.clone());
                }
            }
        } else {
            eprintln!("[bus] unknown target format: {}", target);
        }
    }
}

pub async fn serve(socket_path: &str) -> Result<()> {
    let path = Path::new(socket_path);

    // Clean up stale socket
    if path.exists() {
        std::fs::remove_file(path).ok();
    }

    let listener = UnixListener::bind(path).context("Failed to bind Unix socket")?;

    // Make socket world-accessible so container users (non-root) can connect
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))?;
    }

    eprintln!("[bus] listening on {}", socket_path);

    let state = Arc::new(RwLock::new(BusState::new()));

    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                eprintln!("[bus] connection error: {}", e);
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, state: Arc<RwLock<BusState>>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // First message must be REGISTER
    let first_line = lines
        .next_line()
        .await?
        .context("connection closed before register")?;

    let envelope: Envelope =
        serde_json::from_str(&first_line).context("invalid register message")?;

    let (name, subscriptions) = match envelope {
        Envelope::Register(reg) => (reg.name, reg.subscriptions.into_iter().collect()),
        Envelope::Message(_) => {
            anyhow::bail!("first message must be register");
        }
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    eprintln!("[bus] client registered: {}", name);

    {
        let mut bus = state.write().await;
        bus.clients.insert(
            name.clone(),
            Client {
                name: name.clone(),
                tx,
                subscriptions,
            },
        );
    }

    // Writer task — sends messages from channel to the socket
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut line = serde_json::to_string(&msg).unwrap_or_default();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Reader loop — reads messages from client and routes them
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }

        let envelope: Envelope = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[bus] invalid message from {}: {}", name, e);
                continue;
            }
        };

        match envelope {
            Envelope::Message(msg) => {
                let bus = state.read().await;
                bus.route(&msg);
            }
            Envelope::Register(_) => {
                eprintln!("[bus] ignoring duplicate register from {}", name);
            }
        }
    }

    // Client disconnected
    eprintln!("[bus] client disconnected: {}", name);
    {
        let mut bus = state.write().await;
        bus.clients.remove(&name);
    }
    writer_handle.abort();

    Ok(())
}
