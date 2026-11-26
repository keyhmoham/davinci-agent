use dfae_core::*;
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Registry of available tools with their metadata. The model only ever sees
/// the subset the decision layer selected for the task.
///
/// Clones share their tools: MCP servers connect while the engine runs, and what they bring is registered for everyone.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, tool: Arc<dyn Tool>) {
        self.tools.write().insert(tool.metadata().name, tool);
    }

    /// Remove every tool whose name starts with `prefix` (a disconnected server's tools).
    pub fn unregister_prefix(&self, prefix: &str) -> usize {
        let mut t = self.tools.write();
        let before = t.len();
        t.retain(|n, _| !n.starts_with(prefix));
        before - t.len()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().get(name).cloned()
    }

    pub fn meta(&self, name: &str) -> Option<ToolMetadata> {
        self.get(name).map(|t| t.metadata())
    }

    pub fn metas(&self) -> Vec<ToolMetadata> {
        let tools: Vec<Arc<dyn Tool>> = self.tools.read().values().cloned().collect();
        tools.iter().map(|t| t.metadata()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.read().keys().cloned().collect()
    }

    pub fn briefs(&self) -> Vec<ToolBrief> {
        self.metas()
            .into_iter()
            .map(|m| ToolBrief {
                name: m.name,
                description: m.description,
                category: m.category,
                risk: m.risk,
                side_effects: m.side_effects,
                keywords: m.keywords,
            })
            .collect()
    }

    /// Token cost of presenting *every* tool schema (what a naive agent pays per call).
    pub fn full_schema_tokens(&self) -> usize {
        self.metas()
            .iter()
            .map(|m| tokens::estimate_tokens(&serde_json::to_string(&m.params).unwrap_or_default()) + tokens::estimate_tokens(&m.description) + 12)
            .sum()
    }

    /// Compact signatures for a chosen subset.
    pub fn signatures(&self, names: &[String]) -> String {
        names.iter().filter_map(|n| self.meta(n)).map(|m| m.signature()).collect::<Vec<_>>().join("\n")
    }

    /// One line per tool for a planner that only picks tools.
    pub fn brief_lines(&self, names: &[String]) -> String {
        names.iter().filter_map(|n| self.meta(n)).map(|m| m.brief_line()).collect::<Vec<_>>().join("\n")
    }

    /// The fuller line per tool for a cached prompt prefix.
    pub fn catalog_lines(&self, names: &[String]) -> String {
        names.iter().filter_map(|n| self.meta(n)).map(|m| m.catalog_line()).collect::<Vec<_>>().join("\n")
    }

    /// Signatures with parameter descriptions (for a self-hosted model).
    pub fn signatures_detailed(&self, names: &[String]) -> String {
        names.iter().filter_map(|n| self.meta(n)).map(|m| m.signature_detailed()).collect::<Vec<_>>().join("\n")
    }

    pub fn len(&self) -> usize {
        self.tools.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.read().is_empty()
    }
}
