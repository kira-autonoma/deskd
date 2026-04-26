pub mod discord;
pub mod telegram;

use crate::config::{AgentDef, UserConfig};
use std::future::Future;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;

pub type BoxFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'static>>;

/// Trait implemented by all message adapters (Telegram, Discord, etc.).
/// Each adapter bridges an external messaging platform to/from the agent's bus.
pub trait Adapter: Send + 'static {
    /// Short name for logging, e.g. "telegram", "discord".
    fn name(&self) -> &str;
    /// Run the adapter until it exits or errors.
    fn run(self: Box<Self>, bus_socket: String, agent_name: String) -> BoxFuture;
}

/// Build all configured adapters for an agent from workspace + user config.
/// Returns each adapter paired with its `CancellationToken` so callers can
/// cancel individual adapters cooperatively before aborting their task handles.
pub fn build_adapters(
    def: &AgentDef,
    user_cfg: Option<&UserConfig>,
    admin_telegram_ids: &[i64],
) -> Vec<(Box<dyn Adapter>, CancellationToken)> {
    let mut adapters: Vec<(Box<dyn Adapter>, CancellationToken)> = Vec::new();

    if let Some(tg) = &def.telegram {
        let routes = user_cfg
            .and_then(|c| c.telegram.as_ref())
            .map(|t| t.routes.clone())
            .unwrap_or_default();
        let cancel = CancellationToken::new();
        adapters.push((
            Box::new(telegram::TelegramAdapter::new(
                tg.token.clone(),
                routes,
                admin_telegram_ids.to_vec(),
                def.name.clone(),
                cancel.clone(),
            )),
            cancel,
        ));
    }

    if let Some(dc) = &def.discord {
        let routes = user_cfg
            .and_then(|c| c.discord.as_ref())
            .map(|d| d.routes.clone())
            .unwrap_or_default();
        // Discord adapter does not yet support cooperative cancellation;
        // provide a token that is never triggered so the API is uniform.
        let cancel = CancellationToken::new();
        adapters.push((
            Box::new(discord::DiscordAdapter::new(dc.token.clone(), routes)),
            cancel,
        ));
    }

    adapters
}
