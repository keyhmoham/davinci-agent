//! The Glama MCP directory (<https://glama.ai/mcp/reference>): search for remote MCP servers ("connectors") by what they do.
//!
//! The directory's licence asks that data from it be credited to Glama wherever it is shown, and that each server link to its own
//! listing: [`Connector::listing`] is that link, and [`CREDIT`] the credit.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const CREDIT: &str = "MCP data from Glama (https://glama.ai)";

#[derive(Debug, Clone, thiserror::Error)]
pub enum GlamaError {
    #[error("the Glama API key was refused (HTTP {0}): check GLAMA_API_KEY")]
    Unauthorized(u16),
    #[error("Glama rate limit reached: try again in {0}s")]
    RateLimited(u64),
    #[error("Glama request failed: {0}")]
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Glama {
    http: reqwest::Client,
    base: String,
    key: String,
}

/// How the connector wants to be called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    None,
    ApiKey,
    Basic,
    Oauth2,
}

#[derive(Debug, Clone, Deserialize)]
struct RawConnection {
    #[serde(rename = "authType")]
    auth_type: AuthType,
    transport: String,
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawConnector {
    id: String,
    name: String,
    namespace: String,
    slug: String,
    #[serde(default)]
    description: Option<String>,
    connection: RawConnection,
    #[serde(default)]
    healthy: Option<bool>,
    #[serde(default, rename = "qualityScore")]
    quality_score: Option<f64>,
    #[serde(default, rename = "toolCount")]
    tool_count: Option<u32>,
    #[serde(default, rename = "deprecatedAt")]
    deprecated_at: Option<String>,
    #[serde(default, rename = "isBoosted")]
    boosted: bool,
    #[serde(default)]
    url: Option<String>,
}

/// A remote MCP server from the directory.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Connector {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub slug: String,
    pub description: Option<String>,
    /// The MCP endpoint.
    pub endpoint: String,
    pub transport: String,
    pub auth: AuthType,
    /// `None`: never tested.
    pub healthy: Option<bool>,
    pub quality: Option<f64>,
    pub tool_count: u32,
    pub deprecated: bool,
    /// A paid placement: shown as such, never trusted more for it.
    pub promoted: bool,
    /// The connector's own page on Glama.
    pub listing: String,
}

impl Connector {
    fn from_raw(r: RawConnector) -> Self {
        let listing = r.url.unwrap_or_else(|| format!("https://glama.ai/mcp/connectors/{}/{}", r.namespace, r.slug));
        Self {
            id: r.id,
            name: r.name,
            namespace: r.namespace,
            slug: r.slug,
            description: r.description,
            endpoint: r.connection.url,
            transport: r.connection.transport,
            auth: r.connection.auth_type,
            healthy: r.healthy,
            quality: r.quality_score,
            tool_count: r.tool_count.unwrap_or(0),
            deprecated: r.deprecated_at.is_some(),
            promoted: r.boosted,
            listing,
        }
    }

    /// Does the listing say the service is paid (a per-query price, a marketplace of paid data)? Such a server answers a free call with a
    /// preview or a payment request, never with the data: it is not worth connecting on a user's behalf.
    pub fn paid(&self) -> bool {
        static PAID: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r"(?i)\bx402\b|\busdc\b|\bpay[- ]per\b|\bper[- ](?:query|call|request)\b|\bpaid\b|\bcredits? required\b|\bpremium\b|\bmicropayments?\b",
            )
            .unwrap()
        });
        PAID.is_match(&self.name) || self.description.as_deref().is_some_and(|d| PAID.is_match(d))
    }

    /// Can this client connect without anything from the user? Streamable HTTP, no credentials, not deprecated, not known to be down.
    pub fn open(&self) -> bool {
        self.auth == AuthType::None && self.transport == "streamable_http" && !self.deprecated && self.healthy != Some(false) && !self.paid()
    }
}

impl Glama {
    pub fn new(base_url: &str, key: &str) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("http client");
        Self { http, base: base_url.trim_end_matches('/').to_string(), key: key.to_string() }
    }

    /// Remote servers that match `query`, most relevant first.
    pub async fn search(&self, query: &str, first: u32) -> Result<Vec<Connector>, GlamaError> {
        let rb = self.http.get(format!("{}/v1/connectors", self.base)).bearer_auth(&self.key).query(&[
            ("query", query),
            ("first", &first.clamp(1, 100).to_string()),
            ("sort", "search-relevance:desc"),
        ]);
        let resp = rb.send().await.map_err(|e| GlamaError::Failed(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(GlamaError::Unauthorized(status.as_u16()));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let wait = resp.headers().get("ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(5);
            return Err(GlamaError::RateLimited(wait));
        }
        if !status.is_success() {
            let body: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
            return Err(GlamaError::Failed(format!("HTTP {status}: {body}")));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| GlamaError::Failed(format!("not JSON: {e}")))?;
        let raw: Vec<RawConnector> = serde_json::from_value(v.get("connectors").cloned().unwrap_or(serde_json::json!([])))
            .map_err(|e| GlamaError::Failed(format!("unexpected answer: {e}")))?;
        Ok(raw.into_iter().map(Connector::from_raw).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connector_as_the_directory_documents_it() {
        let raw: RawConnector = serde_json::from_value(serde_json::json!({
            "attributes": ["auth:oauth2"],
            "connection": {"authType": "oauth2", "transport": "streamable_http", "url": "https://mcp.sentry.dev/mcp"},
            "deprecatedAt": null, "description": null, "healthy": null, "id": "wybarb9lq5", "isBoosted": true, "name": "Sentry",
            "namespace": "dev.sentry.mcp", "qualityScore": null, "repository": null, "slug": "sentry", "thumbnailUrl": null, "toolCount": 1,
            "url": "https://glama.ai/mcp/connectors/dev.sentry.mcp/sentry"
        }))
        .unwrap();
        let c = Connector::from_raw(raw);
        assert_eq!((c.auth, c.tool_count, c.promoted), (AuthType::Oauth2, 1, true));
        assert!(!c.open(), "a connector that needs OAuth cannot be used on its own");
        assert_eq!(c.listing, "https://glama.ai/mcp/connectors/dev.sentry.mcp/sentry");
        let mut paid = c.clone();
        paid.auth = AuthType::None;
        paid.name = "Hacker News top stories (x402)".into();
        assert!(paid.paid() && !paid.open(), "a pay-per-call server is not connected for free");
        paid.name = "Hacker News".into();
        paid.description = Some("Live front page. 0.01 USDC per query.".into());
        assert!(paid.paid());
        paid.description = Some("Live front page of Hacker News".into());
        assert!(!paid.paid() && paid.open());
    }
}
