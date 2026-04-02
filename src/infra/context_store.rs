//! File-based context persistence — implements ContextRepository.

use std::path::Path;

use anyhow::Result;

use crate::domain::context::MainBranch;
use crate::infra::dto::StoredMainBranch;
use crate::ports::store::ContextRepository;

/// File-based context store backed by YAML files on disk.
pub struct FileContextStore;

impl FileContextStore {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileContextStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextRepository for FileContextStore {
    fn load(&self, path: &Path) -> Result<MainBranch> {
        let content = std::fs::read_to_string(path)?;
        let dto: StoredMainBranch = serde_yaml::from_str(&content)?;
        Ok(dto.into())
    }

    fn save(&self, branch: &MainBranch, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let dto: StoredMainBranch = branch.into();
        let content = serde_yaml::to_string(&dto)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::context::{Node, NodeKind};

    #[test]
    fn test_file_context_store_roundtrip() {
        let store = FileContextStore::new();
        let dir =
            std::env::temp_dir().join(format!("deskd-ctx-store-test-{}", uuid::Uuid::new_v4()));
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

        store.save(&branch, &path).expect("save failed");
        let loaded = store.load(&path).expect("load failed");

        assert_eq!(loaded.agent, "test-agent");
        assert_eq!(loaded.budget_tokens, 8000);
        assert_eq!(loaded.nodes.len(), 1);
        assert_eq!(loaded.nodes[0].id, "s1");

        std::fs::remove_dir_all(&dir).ok();
    }
}
