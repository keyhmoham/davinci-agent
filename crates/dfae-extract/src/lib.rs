//! Entity extraction.
//!
//! * [`GlinerClient`] — typed client for the GLiNER Python service
//!   (`POST /extract`, dynamic labels).
//! * [`RuleExtractor`] — regex/lexicon extractor for hard-structured spans
//!   (URLs, paths, versions, expressions, constraints). Deterministic and free.
//! * [`HybridExtractor`] — runs both, merges spans (structured rules win for
//!   structured labels, GLiNER wins for semantic ones), caches results, and
//!   degrades to rules-only when the service is down.
//!
//! Extraction yields *candidates*. Nothing here decides permissions, task
//! completion or what enters long-term memory.

mod gliner;
mod hybrid;
mod normalize;
mod rules;

pub use gliner::GlinerClient;
pub use hybrid::HybridExtractor;
pub use normalize::{merge_entities, normalize_entities};
pub use rules::RuleExtractor;

/// Labels worth asking GLiNER for, per task type (dynamic label sets rather
/// than one global list keep both latency and false positives down).
pub fn labels_for(task_type: Option<&str>) -> Vec<String> {
    let base: &[&str] = match task_type {
        Some("file_operations") => &["file path", "directory", "document", "action", "constraint"],
        Some("system_diagnostics") => &["server", "software", "service", "metric", "environment", "action", "constraint"],
        Some("maintenance") => &["software", "version", "server", "environment", "location", "deadline", "constraint", "action", "risk"],
        Some("research") => &["software", "version", "organization", "technology", "document", "deadline", "constraint"],
        Some("coding") => &["programming language", "repository", "file path", "software", "version", "action", "constraint"],
        Some("calculation") => &["number", "unit", "action"],
        _ => dfae_core::DEFAULT_LABELS,
    };
    base.iter().map(|s| s.to_string()).collect()
}
