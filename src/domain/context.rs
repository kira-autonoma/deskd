//! Context domain types — pure data, no I/O.
//! Serde lives on infra DTOs (infra::dto), not here.

/// Node kinds in the context graph
#[derive(Debug, Clone)]
pub enum NodeKind {
    /// Injected as-is into the session
    Static {
        role: String, // "system", "user", "assistant"
        content: String,
    },
    /// Executed at fork time, result injected
    Live {
        command: String, // shell command to execute
        args: Vec<String>,
        max_age_secs: Option<u64>, // cache result for N seconds
        inject_as: String,         // role to inject result as
        cached_result: Option<CachedResult>,
    },
}

#[derive(Debug, Clone)]
pub struct CachedResult {
    pub content: String,
    pub fetched_at: String, // RFC3339
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub label: String,        // human-readable description
    pub tokens_estimate: u32, // approximate token count
    pub tags: Vec<String>,    // topic tags for partitioning (#299)
}

/// The main branch — persistent context for an agent
#[derive(Debug, Clone)]
pub struct MainBranch {
    pub agent: String,
    pub budget_tokens: u32, // target size for main
    pub nodes: Vec<Node>,   // ordered: stable first, dynamic last
}

/// Configuration for context system
#[derive(Debug, Clone, Default)]
pub struct ContextConfig {
    pub enabled: bool,
    pub main_budget_tokens: Option<u32>,       // default 10000
    pub compact_threshold_tokens: Option<u32>, // trigger compaction at this session size, default 80000
    pub main_path: Option<String>,             // path to main branch file
}

#[derive(Debug, Clone)]
pub struct MaterializedMessage {
    pub role: String,
    pub content: String,
}

impl MainBranch {
    pub fn new(agent: &str, budget: u32) -> Self {
        Self {
            agent: agent.to_string(),
            budget_tokens: budget,
            nodes: Vec::new(),
        }
    }

    /// Total estimated tokens across all nodes
    pub fn total_tokens(&self) -> u32 {
        self.nodes.iter().map(|n| n.tokens_estimate).sum()
    }

    /// Add a static content node to the branch.
    pub fn add_static(&mut self, id: &str, label: &str, role: &str, content: &str) {
        let tokens_estimate = (content.len() as u32) / 4;
        self.nodes.push(Node {
            id: id.to_string(),
            kind: NodeKind::Static {
                role: role.to_string(),
                content: content.to_string(),
            },
            label: label.to_string(),
            tokens_estimate,
            tags: Vec::new(),
        });
    }

    /// Add a static content node with tags.
    pub fn add_static_tagged(
        &mut self,
        id: &str,
        label: &str,
        role: &str,
        content: &str,
        tags: Vec<String>,
    ) {
        let tokens_estimate = (content.len() as u32) / 4;
        self.nodes.push(Node {
            id: id.to_string(),
            kind: NodeKind::Static {
                role: role.to_string(),
                content: content.to_string(),
            },
            label: label.to_string(),
            tokens_estimate,
            tags,
        });
    }

    /// Serialize all static nodes into a single system prompt string.
    ///
    /// Used for context transfer: when spawning a child agent, this
    /// produces the prompt that carries the parent's context subset.
    pub fn to_system_prompt(&self) -> String {
        let mut sections = Vec::new();
        for node in &self.nodes {
            if let NodeKind::Static { content, .. } = &node.kind {
                sections.push(format!("## {}\n{}", node.label, content));
            }
        }
        sections.join("\n\n")
    }

    /// Partition nodes by tag groups, returning a new MainBranch per group.
    ///
    /// Each group is a set of tags. A node matches a group if it has any
    /// overlapping tag. Untagged nodes are included in all partitions.
    pub fn partition_by_tags(&self, groups: &[Vec<String>]) -> Vec<MainBranch> {
        groups
            .iter()
            .enumerate()
            .map(|(i, group_tags)| {
                let matching_nodes: Vec<Node> = self
                    .nodes
                    .iter()
                    .filter(|n| n.tags.is_empty() || n.tags.iter().any(|t| group_tags.contains(t)))
                    .cloned()
                    .collect();
                let mut branch =
                    MainBranch::new(&format!("{}-split-{}", self.agent, i), self.budget_tokens);
                branch.nodes = matching_nodes;
                branch
            })
            .collect()
    }
}

/// Check if session should be compacted based on cumulative token usage
pub fn should_compact(total_tokens_used: u64, threshold: u64) -> bool {
    total_tokens_used >= threshold
}

/// Default path for the main branch file relative to an agent's work_dir.
pub fn default_main_path(work_dir: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(work_dir)
        .join(".deskd")
        .join("context")
        .join("main.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_static_node() {
        let mut branch = MainBranch::new("test-agent", 10_000);
        branch.add_static("n1", "System Role", "system", "You are a helpful agent.");
        assert_eq!(branch.nodes.len(), 1);
        assert_eq!(branch.nodes[0].id, "n1");
        assert!(branch.nodes[0].tokens_estimate > 0);
        assert!(branch.nodes[0].tags.is_empty());
    }

    #[test]
    fn test_add_static_tagged() {
        let mut branch = MainBranch::new("test-agent", 10_000);
        branch.add_static_tagged(
            "n1",
            "Rust Context",
            "system",
            "Rust codebase info",
            vec!["rust".into(), "code".into()],
        );
        assert_eq!(branch.nodes[0].tags, vec!["rust", "code"]);
    }

    #[test]
    fn test_to_system_prompt() {
        let mut branch = MainBranch::new("test-agent", 10_000);
        branch.add_static("n1", "Role", "system", "You are helpful.");
        branch.add_static("n2", "Context", "user", "Work on deskd.");
        let prompt = branch.to_system_prompt();
        assert!(prompt.contains("## Role\nYou are helpful."));
        assert!(prompt.contains("## Context\nWork on deskd."));
    }

    #[test]
    fn test_to_system_prompt_skips_live_nodes() {
        let mut branch = MainBranch::new("test-agent", 10_000);
        branch.add_static("n1", "Role", "system", "Be helpful.");
        branch.nodes.push(Node {
            id: "live1".into(),
            kind: NodeKind::Live {
                command: "echo".into(),
                args: vec!["hello".into()],
                max_age_secs: None,
                inject_as: "user".into(),
                cached_result: None,
            },
            label: "Live Node".into(),
            tokens_estimate: 10,
            tags: Vec::new(),
        });
        let prompt = branch.to_system_prompt();
        assert!(prompt.contains("Be helpful."));
        assert!(!prompt.contains("Live Node"));
    }

    #[test]
    fn test_partition_by_tags() {
        let mut branch = MainBranch::new("parent", 10_000);
        branch.add_static_tagged("n1", "Rust", "system", "Rust code", vec!["rust".into()]);
        branch.add_static_tagged("n2", "Go", "system", "Go code", vec!["go".into()]);
        branch.add_static("n3", "Shared", "system", "Shared context"); // untagged

        let partitions = branch.partition_by_tags(&[vec!["rust".into()], vec!["go".into()]]);

        assert_eq!(partitions.len(), 2);

        // Rust partition: n1 (rust) + n3 (untagged)
        assert_eq!(partitions[0].nodes.len(), 2);
        assert_eq!(partitions[0].agent, "parent-split-0");

        // Go partition: n2 (go) + n3 (untagged)
        assert_eq!(partitions[1].nodes.len(), 2);
        assert_eq!(partitions[1].agent, "parent-split-1");
    }

    #[test]
    fn test_partition_node_in_multiple_groups() {
        let mut branch = MainBranch::new("agent", 5_000);
        branch.add_static_tagged(
            "n1",
            "Both",
            "system",
            "Relevant to both",
            vec!["rust".into(), "go".into()],
        );

        let partitions = branch.partition_by_tags(&[vec!["rust".into()], vec!["go".into()]]);

        // Node with both tags appears in both partitions.
        assert_eq!(partitions[0].nodes.len(), 1);
        assert_eq!(partitions[1].nodes.len(), 1);
    }

    #[test]
    fn test_token_estimate_from_content_length() {
        let mut branch = MainBranch::new("agent", 10_000);
        branch.add_static("n1", "Test", "system", "a]bc"); // 4 chars → 1 token
        assert_eq!(branch.nodes[0].tokens_estimate, 1);
    }
}
