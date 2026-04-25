//! `deskd status` subcommand handler.
//!
//! Renders the unified dashboard (#213) — agents, sub-agent workers, SM
//! instances, and the task queue — in either text or JSON form. The actual
//! aggregation lives in [`super::dashboard`].

use anyhow::Result;

use super::dashboard;

pub async fn handle(config_path: &str, format: &str) -> Result<()> {
    let dash = dashboard::build(config_path).await?;
    match format {
        "json" => {
            println!("{}", dashboard::render_json(&dash)?);
        }
        "text" | "" => {
            print!("{}", dashboard::render_text(&dash));
        }
        other => {
            anyhow::bail!("unknown --format '{other}' (expected 'text' or 'json')");
        }
    }
    Ok(())
}
