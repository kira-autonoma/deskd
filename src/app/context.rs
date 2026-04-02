//! Context application layer — I/O operations on context types.
//!
//! Pure domain types live in `domain::context`. This module adds
//! persistence (load/save via ContextRepository) and materialization
//! (execute live nodes).

use crate::infra::context_store::FileContextStore;
use crate::ports::store::ContextRepository;

// Re-export all domain types for backward compatibility.
pub use crate::domain::context::*;

impl MainBranch {
    /// Load from YAML file (convenience wrapper over ContextRepository).
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        FileContextStore::new().load(path)
    }

    /// Save to YAML file (convenience wrapper over ContextRepository).
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        FileContextStore::new().save(self, path)
    }

    /// Materialize: execute live nodes and produce message list for session injection
    pub async fn materialize(&mut self) -> anyhow::Result<Vec<MaterializedMessage>> {
        let mut messages = Vec::new();
        for node in &mut self.nodes {
            match &mut node.kind {
                NodeKind::Static { role, content } => {
                    messages.push(MaterializedMessage {
                        role: role.clone(),
                        content: content.clone(),
                    });
                }
                NodeKind::Live {
                    command,
                    args,
                    max_age_secs,
                    inject_as,
                    cached_result,
                } => {
                    // Check cache
                    let use_cache =
                        if let (Some(cached), Some(max_age)) = (&cached_result, max_age_secs) {
                            if let Ok(fetched) =
                                chrono::DateTime::parse_from_rfc3339(&cached.fetched_at)
                            {
                                let age = chrono::Utc::now().signed_duration_since(fetched);
                                age.num_seconds() < *max_age as i64
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                    let content = if use_cache {
                        cached_result.as_ref().unwrap().content.clone()
                    } else {
                        // Execute command
                        let output = tokio::process::Command::new(&*command)
                            .args(args.iter())
                            .output()
                            .await?;
                        let result = String::from_utf8_lossy(&output.stdout).to_string();
                        *cached_result = Some(CachedResult {
                            content: result.clone(),
                            fetched_at: chrono::Utc::now().to_rfc3339(),
                        });
                        result
                    };

                    if !content.trim().is_empty() {
                        messages.push(MaterializedMessage {
                            role: inject_as.clone(),
                            content: format!("[{}] {}", node.label, content.trim()),
                        });
                    }
                }
            }
        }
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_main_branch_new() {
        let branch = MainBranch::new("kira", 10000);
        assert_eq!(branch.agent, "kira");
        assert_eq!(branch.budget_tokens, 10000);
        assert!(branch.nodes.is_empty());
    }

    #[test]
    fn test_total_tokens_empty() {
        let branch = MainBranch::new("kira", 10000);
        assert_eq!(branch.total_tokens(), 0);
    }

    #[test]
    fn test_total_tokens_with_nodes() {
        let mut branch = MainBranch::new("kira", 10000);
        branch.nodes.push(Node {
            id: "n1".into(),
            kind: NodeKind::Static {
                role: "system".into(),
                content: "You are Kira.".into(),
            },
            label: "System prompt".into(),
            tokens_estimate: 500,
        });
        branch.nodes.push(Node {
            id: "n2".into(),
            kind: NodeKind::Static {
                role: "user".into(),
                content: "Governance rules...".into(),
            },
            label: "Governance".into(),
            tokens_estimate: 1200,
        });
        assert_eq!(branch.total_tokens(), 1700);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("deskd_test_context_roundtrip");
        let path = dir.join("main.yaml");

        let mut branch = MainBranch::new("test-agent", 8000);
        branch.nodes.push(Node {
            id: "s1".into(),
            kind: NodeKind::Static {
                role: "system".into(),
                content: "Hello world".into(),
            },
            label: "Greeting".into(),
            tokens_estimate: 100,
        });
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["test".into()],
                max_age_secs: Some(300),
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Echo test".into(),
            tokens_estimate: 50,
        });

        branch.save(&path).expect("save failed");
        let loaded = MainBranch::load(&path).expect("load failed");

        assert_eq!(loaded.agent, "test-agent");
        assert_eq!(loaded.budget_tokens, 8000);
        assert_eq!(loaded.nodes.len(), 2);
        assert_eq!(loaded.nodes[0].id, "s1");
        assert_eq!(loaded.nodes[0].label, "Greeting");
        assert_eq!(loaded.total_tokens(), 150);

        // Verify static node content
        match &loaded.nodes[0].kind {
            NodeKind::Static { role, content } => {
                assert_eq!(role, "system");
                assert_eq!(content, "Hello world");
            }
            _ => panic!("Expected Static node"),
        }

        // Verify live node content
        match &loaded.nodes[1].kind {
            NodeKind::Live {
                command,
                args,
                max_age_secs,
                inject_as,
                cached_result,
            } => {
                assert_eq!(command, "echo");
                assert_eq!(args, &["test"]);
                assert_eq!(*max_age_secs, Some(300));
                assert_eq!(inject_as, "user");
                // cached_result is #[serde(skip)] so should be None after load
                assert!(cached_result.is_none());
            }
            _ => panic!("Expected Live node"),
        }

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_materialize_static_nodes() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "s1".into(),
            kind: NodeKind::Static {
                role: "system".into(),
                content: "You are a test agent.".into(),
            },
            label: "System".into(),
            tokens_estimate: 100,
        });
        branch.nodes.push(Node {
            id: "s2".into(),
            kind: NodeKind::Static {
                role: "user".into(),
                content: "Some context".into(),
            },
            label: "Context".into(),
            tokens_estimate: 50,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "You are a test agent.");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "Some context");
    }

    #[tokio::test]
    async fn test_materialize_live_node_echo() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["hello from live node".into()],
                max_age_secs: None,
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Echo test".into(),
            tokens_estimate: 50,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "[Echo test] hello from live node");
    }

    #[tokio::test]
    async fn test_materialize_live_node_caches_result() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["cached".into()],
                max_age_secs: Some(3600), // 1 hour cache
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Cached echo".into(),
            tokens_estimate: 50,
        });

        // First materialize — executes command
        let messages = branch
            .materialize()
            .await
            .expect("first materialize failed");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "[Cached echo] cached");

        // Verify cache was populated
        match &branch.nodes[0].kind {
            NodeKind::Live { cached_result, .. } => {
                assert!(cached_result.is_some());
                let cached = cached_result.as_ref().unwrap();
                assert_eq!(cached.content, "cached\n");
            }
            _ => panic!("Expected Live node"),
        }

        // Second materialize — should use cache (max_age_secs=3600, just ran)
        let messages2 = branch
            .materialize()
            .await
            .expect("second materialize failed");
        assert_eq!(messages2.len(), 1);
        assert_eq!(messages2[0].content, "[Cached echo] cached");
    }

    #[tokio::test]
    async fn test_materialize_live_node_expired_cache() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["fresh".into()],
                max_age_secs: Some(60),
                inject_as: "user".into(),
                // Pre-populate with an expired cache entry
                cached_result: Some(CachedResult {
                    content: "stale data".into(),
                    fetched_at: "2020-01-01T00:00:00Z".into(), // long expired
                }),
            },
            label: "Expiry test".into(),
            tokens_estimate: 50,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert_eq!(messages.len(), 1);
        // Should have re-executed, getting "fresh" from echo
        assert_eq!(messages[0].content, "[Expiry test] fresh");
    }

    #[tokio::test]
    async fn test_materialize_empty_output_skipped() {
        let mut branch = MainBranch::new("agent", 10000);
        branch.nodes.push(Node {
            id: "l1".into(),
            kind: NodeKind::Live {
                command: "true".into(), // produces no output
                args: vec![],
                max_age_secs: None,
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Silent".into(),
            tokens_estimate: 10,
        });

        let messages = branch.materialize().await.expect("materialize failed");
        assert!(messages.is_empty(), "Empty output should be skipped");
    }

    #[test]
    fn test_should_compact() {
        assert!(!should_compact(50000, 80000));
        assert!(should_compact(80000, 80000));
        assert!(should_compact(100000, 80000));
        assert!(!should_compact(0, 80000));
    }

    #[test]
    fn test_default_main_path() {
        let path = default_main_path("/home/kira");
        assert_eq!(path, PathBuf::from("/home/kira/.deskd/context/main.yaml"));
    }

    #[test]
    fn test_context_config_defaults() {
        let cfg = ContextConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.main_budget_tokens.is_none());
        assert!(cfg.compact_threshold_tokens.is_none());
        assert!(cfg.main_path.is_none());
    }

    #[test]
    fn test_context_config_serde() {
        use crate::infra::dto::ConfigContextConfig;
        let yaml = r#"
enabled: true
main_budget_tokens: 12000
compact_threshold_tokens: 90000
main_path: /home/kira/.deskd/context/main.yaml
"#;
        let dto: ConfigContextConfig = serde_yaml::from_str(yaml).unwrap();
        let cfg: ContextConfig = dto.into();
        assert!(cfg.enabled);
        assert_eq!(cfg.main_budget_tokens, Some(12000));
        assert_eq!(cfg.compact_threshold_tokens, Some(90000));
        assert_eq!(
            cfg.main_path.as_deref(),
            Some("/home/kira/.deskd/context/main.yaml")
        );
    }
}
