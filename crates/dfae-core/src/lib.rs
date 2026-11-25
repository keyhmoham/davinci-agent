//! Shared types, traits and utilities for the Decision-First Agentic Engine.
//!
//! Everything the other crates agree on lives here: the task state model,
//! plan schema, events, token accounting, decision contract and the provider
//! traits (`DecisionProvider`, `EntityExtractor`, `LanguageModel`, `Tool`,
//! `MemoryStore`).

pub mod config;
pub mod decision;
pub mod error;
pub mod events;
pub mod ledger;
pub mod memory;
pub mod providers;
pub mod safety;
pub mod text;
pub mod tokens;
pub mod tool;
pub mod types;

pub use config::*;
pub use decision::*;
pub use error::*;
pub use events::*;
pub use ledger::*;
pub use memory::*;
pub use providers::*;
pub use tool::*;
pub use types::*;

/// A new random identifier (UUID v4, string form).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// First 8 characters of an id, for display.
pub fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Stable SHA-256 hex digest of a string.
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Canonical (key-sorted) JSON serialisation, used for hashing tool arguments.
pub fn canonical_json(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap(), canonical_json(&m[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}
