//! CLI handler for `deskd a2a` subcommands.

use std::sync::Arc;

use anyhow::Result;

use crate::app::cli::A2aAction;
use crate::app::{a2a, a2a_jwt, a2a_server};
use crate::config::WorkspaceConfig;

pub async fn handle(action: A2aAction, config_path: &str) -> Result<()> {
    match action {
        A2aAction::AgentCard { .. } => {
            let workspace = WorkspaceConfig::load(config_path)?;
            let card = a2a::build_agent_card(&workspace)?;
            let json = serde_json::to_string_pretty(&card)?;
            println!("{json}");
            Ok(())
        }
        A2aAction::Keygen {} => {
            let key_path = default_key_path();
            if key_path.exists() {
                anyhow::bail!(
                    "Key already exists at {}. Remove it first to regenerate.",
                    key_path.display()
                );
            }
            let kp = a2a_jwt::KeyPair::generate()?;
            kp.save(&key_path)?;
            println!("Generated Ed25519 key pair:");
            println!("  Private: {}", key_path.display());
            println!("  Public:  {}", key_path.with_extension("pub").display());
            println!("  Base64:  {}", kp.public_key_base64url());
            Ok(())
        }
        A2aAction::Serve { listen, .. } => {
            let workspace = WorkspaceConfig::load(config_path)?;
            let a2a_cfg = workspace
                .a2a
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("workspace.yaml has no `a2a:` section"))?;

            let mut card = a2a::build_agent_card(&workspace)?;

            // Load own key pair for JWKS publication if JWT mode.
            if a2a_cfg.auth == "jwt" {
                let key_path = a2a_cfg
                    .private_key
                    .as_deref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(default_key_path);
                let kp = a2a_jwt::KeyPair::load(&key_path)?;
                card.authentication.jwks =
                    Some(a2a_jwt::Jwks::from_public_key(kp.public_key_bytes()));
            }

            // Decode trusted public keys from config (base64url → raw bytes).
            let trusted_keys: Vec<Vec<u8>> = a2a_cfg
                .trusted_keys
                .iter()
                .filter_map(|b64| {
                    use base64::Engine;
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(b64)
                        .ok()
                })
                .collect();

            if a2a_cfg.auth == "jwt" && trusted_keys.is_empty() {
                tracing::warn!(
                    "JWT auth enabled but no trusted_keys configured — all incoming requests will be rejected"
                );
            }

            let listen_addr = listen.as_deref().unwrap_or(&a2a_cfg.listen);

            // Find a bus socket from serve state or workspace agents.
            let bus_socket = crate::config::ServeState::load()
                .and_then(|s| s.any_bus_socket().map(String::from))
                .or_else(|| workspace.agents.first().map(|a| a.bus_socket()))
                .ok_or_else(|| anyhow::anyhow!("no bus socket found — is deskd serve running?"))?;

            let state = Arc::new(a2a_server::A2aState {
                agent_card: card,
                api_key: a2a_cfg.api_key.clone(),
                bus_socket,
                auth_mode: a2a_cfg.auth.clone(),
                trusted_keys,
                tasks: a2a_server::A2aTaskRegistry::default(),
            });

            a2a_server::serve(listen_addr, state).await
        }
    }
}

fn default_key_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home)
        .join(".deskd")
        .join("a2a_key.pem")
}
