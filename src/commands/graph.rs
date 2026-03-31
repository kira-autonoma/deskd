//! `deskd graph` subcommand handlers.

use anyhow::{Context, Result};

use crate::cli::GraphAction;
use crate::graph;

pub async fn handle(action: GraphAction) -> Result<()> {
    match action {
        GraphAction::Run {
            file,
            work_dir,
            vars,
        } => {
            let path = std::path::Path::new(&file);
            let abs_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            };

            let work = if let Some(ref wd) = work_dir {
                std::path::PathBuf::from(wd)
            } else {
                abs_path
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            };

            let yaml = std::fs::read_to_string(&abs_path)
                .with_context(|| format!("failed to read graph file: {}", abs_path.display()))?;
            let graph_def: graph::GraphDef = serde_yaml::from_str(&yaml)
                .with_context(|| format!("failed to parse graph YAML: {}", abs_path.display()))?;

            println!(
                "Running graph: {} ({} steps)",
                graph_def.graph,
                graph_def.steps.len()
            );

            let progress: graph::ProgressFn = Box::new(|result| {
                let status = if result.skipped { "SKIP" } else { "DONE" };
                println!("  [{status}] {} ({}ms)", result.id, result.duration_ms);
            });

            let inputs: Option<std::collections::HashMap<String, String>> = if vars.is_empty() {
                None
            } else {
                let mut map = std::collections::HashMap::new();
                for kv in &vars {
                    if let Some((k, v)) = kv.split_once('=') {
                        map.insert(k.to_string(), v.to_string());
                    } else {
                        anyhow::bail!("invalid --var format '{}', expected KEY=VALUE", kv);
                    }
                }
                Some(map)
            };

            let ctx = graph::execute(&graph_def, &work, Some(&progress), inputs).await?;

            println!("\nGraph completed: {} steps executed", ctx.results.len());
            if !ctx.variables.is_empty() {
                println!("Variables:");
                for (k, v) in &ctx.variables {
                    let display = if v.len() > 80 {
                        format!("{}...", &v[..80])
                    } else {
                        v.clone()
                    };
                    println!("  {}: {}", k, display);
                }
            }
        }
        GraphAction::Validate { file } => {
            let yaml = std::fs::read_to_string(&file)
                .with_context(|| format!("failed to read graph file: {}", file))?;
            let graph_def: graph::GraphDef = serde_yaml::from_str(&yaml)
                .with_context(|| format!("failed to parse graph YAML: {}", file))?;

            graph::topo_sort(&graph_def)?;

            println!(
                "Graph '{}' is valid: {} steps, DAG OK",
                graph_def.graph,
                graph_def.steps.len()
            );
        }
    }
    Ok(())
}
