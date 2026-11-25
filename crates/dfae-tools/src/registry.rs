use dfae_core::*;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Registry of available tools with their metadata. The model only ever sees
/// the subset the decision layer selected for the task.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.metadata().name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn meta(&self, name: &str) -> Option<ToolMetadata> {
        self.tools.get(name).map(|t| t.metadata())
    }

    pub fn metas(&self) -> Vec<ToolMetadata> {
        self.tools.values().map(|t| t.metadata()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
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

    /// Signatures with parameter descriptions (for a self-hosted model).
    pub fn signatures_detailed(&self, names: &[String]) -> String {
        names.iter().filter_map(|n| self.meta(n)).map(|m| m.signature_detailed()).collect::<Vec<_>>().join("\n")
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
