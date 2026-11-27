//! Memory: embeddings, durable stores, ranking and context assembly.
//!
//! Four memory layers share one store abstraction:
//! working state (persisted task records), episodic (task outcomes),
//! semantic (facts/documents) and procedural (skills). PostgreSQL is the
//! durable backend (pgvector when the extension exists); an in-memory store
//! backs tests and database-less runs.

mod context;
mod embed;
mod inmem;
mod pg;
mod remote;

pub use context::*;
pub use embed::{HashEmbedder, cosine};
pub use inmem::InMemoryStore;
pub use pg::PgStore;
pub use remote::RemoteEmbedder;

use dfae_core::*;
use sha2::{Digest, Sha256};

/// Hash-chain link for audit records.
pub(crate) fn chain_hash(prev: &str, kind: &str, payload: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(kind.as_bytes());
    h.update(canonical_json(payload).as_bytes());
    hex::encode(h.finalize())
}

pub(crate) const GENESIS: &str = "genesis";

/// Verify an audit chain end to end (tamper evidence).
pub fn verify_audit_chain(records: &[AuditRecord]) -> bool {
    let mut prev = GENESIS.to_string();
    for r in records {
        let expect = chain_hash(&prev, &r.kind, &r.payload);
        if expect != r.hash {
            return false;
        }
        prev = r.hash.clone();
    }
    true
}
