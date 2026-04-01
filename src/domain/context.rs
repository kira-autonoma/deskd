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
