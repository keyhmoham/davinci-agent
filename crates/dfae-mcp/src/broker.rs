//! Who opens remote MCP servers for the engine, and signs the user in to them.
//!
//! The hub never speaks to an authenticated server itself. It asks a [`Broker`] to open one, and gets back the [`Spec`] its own
//! client connects to:
//!
//! * [`crate::mcpc::McpcBroker`] runs [mcpc](https://github.com/apify/mcpc): every remote server is an mcpc session, reached through
//!   the session's local proxy (`mcpc connect … --proxy`), so OAuth (CIMD, DCR, PKCE, token refresh), keepalive and reconnects are
//!   mcpc's, and the engine only ever holds the proxy's bearer token, never the server's credentials.
//! * [`crate::remote::RemoteBroker`] does the same through a broker in another container (`dfae-server mcpc-broker`), so the
//!   credentials live where the agent's own commands cannot read them.
//! * [`DirectBroker`] is what is left without mcpc: the engine connects to the server as it is (no credentials, or a static
//!   header), and servers that want a sign-in are not offered.

use crate::client::Spec;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum BrokerError {
    /// The server wants credentials the broker does not have: the user has to sign in.
    #[error("{0}")]
    NeedsLogin(String),
    /// The broker cannot do this at all (no mcpc, a local server).
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Failed(String),
}

/// A sign-in that waits for the user: the URL to open, and the `state` the provider's answer is matched by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginStart {
    pub state: String,
    pub url: String,
}

#[async_trait]
pub trait Broker: Send + Sync {
    /// `mcpc`, `mcpc-remote` or `direct` (for logs and the API).
    fn kind(&self) -> &'static str;

    /// Can the user be asked to sign in through it?
    fn signs_in(&self) -> bool;

    /// Open the remote server at `url` for the engine's server `server` (or reuse what is open and alive), with `headers` (an API
    /// key). Returns what the engine's client connects to. [`BrokerError::NeedsLogin`]: the server refused for want of a sign-in.
    async fn open(&self, server: &str, url: &str, headers: &BTreeMap<String, String>) -> Result<Spec, BrokerError>;

    /// Close what was opened for `server` (best effort).
    async fn close(&self, server: &str);

    /// The server refused what the broker holds for it (a sign-in that expired and could not be refreshed, or was revoked).
    async fn unauthorized(&self, server: &str) -> bool;

    /// The user signed in to the server at `url` before, and the broker still holds that sign-in.
    async fn signed_in(&self, url: &str) -> bool;

    /// Start signing the user in to the server at `url`.
    async fn begin_login(&self, url: &str) -> Result<LoginStart, BrokerError>;

    /// The provider sent the user's browser back with this query (`code`, `state`, `iss`, or `error`): hand it to the sign-in it
    /// belongs to. Needed when the browser could not reach the broker's own callback (another machine, another container).
    async fn deliver(&self, state: &str, query: &str) -> Result<(), BrokerError>;

    /// Wait until the sign-in `state` is done (the tokens are kept), failed, or `timeout` passed.
    async fn wait_login(&self, state: &str, timeout: Duration) -> Result<(), BrokerError>;

    /// Forget the sign-in to the server at `url`.
    async fn logout(&self, url: &str);
}

/// No broker: the engine connects to remote servers itself, with no credentials but a static header.
pub struct DirectBroker;

const NO_SIGN_IN: &str = "signing in to MCP servers needs mcpc (install @apify/mcpc, or set DFAE_MCPC_BROKER_URL)";

#[async_trait]
impl Broker for DirectBroker {
    fn kind(&self) -> &'static str {
        "direct"
    }

    fn signs_in(&self) -> bool {
        false
    }

    async fn open(&self, _server: &str, url: &str, headers: &BTreeMap<String, String>) -> Result<Spec, BrokerError> {
        Ok(Spec::Http { url: url.to_string(), headers: headers.clone() })
    }

    async fn close(&self, _server: &str) {}

    async fn unauthorized(&self, _server: &str) -> bool {
        false
    }

    async fn signed_in(&self, _url: &str) -> bool {
        false
    }

    async fn begin_login(&self, _url: &str) -> Result<LoginStart, BrokerError> {
        Err(BrokerError::Unsupported(NO_SIGN_IN.into()))
    }

    async fn deliver(&self, _state: &str, _query: &str) -> Result<(), BrokerError> {
        Err(BrokerError::Unsupported(NO_SIGN_IN.into()))
    }

    async fn wait_login(&self, _state: &str, _timeout: Duration) -> Result<(), BrokerError> {
        Err(BrokerError::Unsupported(NO_SIGN_IN.into()))
    }

    async fn logout(&self, _url: &str) {}
}

/// A server's URL as sign-ins are kept by: lowercase scheme and host, no query, fragment or trailing slash.
pub fn canonical_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            u.set_fragment(None);
            u.set_query(None);
            u.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => url.trim_end_matches('/').to_string(),
    }
}

/// Equal strings, in time that does not depend on where they differ.
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A random URL-safe string (two v4 UUIDs: 244 random bits).
pub(crate) fn random_token() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

/// The OAuth parameters of the query the provider sent the browser back with, and nothing else: what is handed on is rebuilt from
/// these, never passed through.
pub fn callback_params(query: &str) -> Vec<(String, String)> {
    const KEPT: &[&str] = &["code", "state", "iss", "error", "error_description", "error_uri"];
    let query = query.trim().trim_start_matches('?');
    // a whole URL pasted from the address bar is fine too
    let query = match query.find('?') {
        Some(i) if query.starts_with("http") => &query[i + 1..],
        _ => query,
    };
    reqwest::Url::parse(&format!("http://x/?{query}"))
        .map(|u| u.query_pairs().into_owned().filter(|(k, _)| KEPT.contains(&k.as_str())).collect())
        .unwrap_or_default()
}

/// The `state` in a callback query, if any.
pub fn callback_state(query: &str) -> Option<String> {
    callback_params(query).into_iter().find(|(k, _)| k == "state").map(|(_, v)| v).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_oauth_parameters_of_a_callback_are_kept() {
        let p = callback_params("http://127.0.0.1:13316/callback?code=abc&state=s1&iss=https%3A%2F%2Fa&evil=1#frag");
        assert_eq!(p, vec![("code".into(), "abc".into()), ("state".into(), "s1".into()), ("iss".into(), "https://a".into())]);
        assert_eq!(callback_state("?state=zz&code=1").as_deref(), Some("zz"));
        assert_eq!(callback_state("code=1"), None);
        assert_eq!(callback_params("error=access_denied&error_description=no").len(), 2);
    }

    #[test]
    fn urls_compare_as_sign_ins_are_kept() {
        assert_eq!(canonical_url("HTTPS://MCP.Linear.app/mcp/?x=1#y"), "https://mcp.linear.app/mcp");
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        assert!(random_token().len() >= 64);
    }
}
