//! A broker in another process or container (`dfae-server mcpc-broker`): mcpc runs there, under its own user, with its own
//! volume, so the user's sign-ins are out of reach of the agent's commands (which run as the engine's user, in the engine's
//! container). The engine reaches the broker's API with a shared token, and the sessions' proxies with one bearer token each.
//!
//! The protocol is one `POST /v1/<op>` with a JSON body per [`Broker`] method; [`dispatch`] is its server side.

use crate::broker::{Broker, BrokerError, LoginStart};
use crate::client::Spec;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

pub struct RemoteBroker {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl RemoteBroker {
    pub fn new(base: &str, token: &str) -> Self {
        // the broker is on the engine's own network: never through a proxy
        let http = reqwest::Client::builder().no_proxy().build().expect("http");
        Self { http, base: base.trim_end_matches('/').to_string(), token: token.to_string() }
    }

    async fn call(&self, op: &str, body: Value, timeout: Duration) -> Result<Value, BrokerError> {
        let r = self
            .http
            .post(format!("{}/v1/{op}", self.base))
            .bearer_auth(&self.token)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| BrokerError::Failed(format!("the mcpc broker at {} cannot be reached: {e}", self.base)))?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(v);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(BrokerError::Failed("the mcpc broker refused the engine's token (DFAE_MCPC_BROKER_TOKEN differs)".into()));
        }
        Err(serde_json::from_value::<BrokerError>(v.clone()).unwrap_or_else(|_| BrokerError::Failed(format!("the mcpc broker answered HTTP {status}: {v}"))))
    }

    async fn flag(&self, op: &str, body: Value) -> bool {
        self.call(op, body, Duration::from_secs(30)).await.ok().and_then(|v| v["value"].as_bool()).unwrap_or(false)
    }
}

const LONG: Duration = Duration::from_secs(120);

#[async_trait]
impl Broker for RemoteBroker {
    fn kind(&self) -> &'static str {
        "mcpc-remote"
    }

    fn signs_in(&self) -> bool {
        true
    }

    async fn open(&self, server: &str, url: &str, headers: &BTreeMap<String, String>) -> Result<Spec, BrokerError> {
        let v = self.call("open", json!({"server": server, "url": url, "headers": headers}), LONG).await?;
        #[derive(Deserialize)]
        struct Opened {
            url: String,
            #[serde(default)]
            headers: BTreeMap<String, String>,
        }
        let o: Opened = serde_json::from_value(v).map_err(|e| BrokerError::Failed(format!("the mcpc broker's answer: {e}")))?;
        Ok(Spec::Http { url: o.url, headers: o.headers })
    }

    async fn close(&self, server: &str) {
        let _ = self.call("close", json!({"server": server}), LONG).await;
    }

    async fn unauthorized(&self, server: &str) -> bool {
        self.flag("unauthorized", json!({"server": server})).await
    }

    async fn signed_in(&self, url: &str) -> bool {
        self.flag("signed-in", json!({"url": url})).await
    }

    async fn begin_login(&self, url: &str) -> Result<LoginStart, BrokerError> {
        let v = self.call("login", json!({"url": url}), LONG).await?;
        serde_json::from_value(v).map_err(|e| BrokerError::Failed(format!("the mcpc broker's answer: {e}")))
    }

    async fn deliver(&self, state: &str, query: &str) -> Result<(), BrokerError> {
        self.call("deliver", json!({"state": state, "query": query}), LONG).await.map(|_| ())
    }

    async fn wait_login(&self, state: &str, timeout: Duration) -> Result<(), BrokerError> {
        self.call("wait", json!({"state": state, "timeout_secs": timeout.as_secs()}), timeout + Duration::from_secs(15)).await.map(|_| ())
    }

    async fn logout(&self, url: &str) {
        let _ = self.call("logout", json!({"url": url}), LONG).await;
    }
}

/// The broker's side of the protocol: one operation on `broker`. `Err` is sent back as the [`BrokerError`]'s JSON.
pub async fn dispatch(broker: &dyn Broker, op: &str, body: &Value) -> Result<Value, BrokerError> {
    let s = |k: &str| body.get(k).and_then(|v| v.as_str()).map(String::from).ok_or_else(|| BrokerError::Unsupported(format!("`{k}` is missing")));
    match op {
        "open" => {
            let headers: BTreeMap<String, String> = serde_json::from_value(body.get("headers").cloned().unwrap_or(json!({}))).unwrap_or_default();
            match broker.open(&s("server")?, &s("url")?, &headers).await? {
                Spec::Http { url, headers } => Ok(json!({"url": url, "headers": headers})),
                Spec::Stdio { .. } => Err(BrokerError::Unsupported("the broker opens remote servers only".into())),
            }
        }
        "close" => {
            broker.close(&s("server")?).await;
            Ok(json!({}))
        }
        "unauthorized" => Ok(json!({"value": broker.unauthorized(&s("server")?).await})),
        "signed-in" => Ok(json!({"value": broker.signed_in(&s("url")?).await})),
        "login" => Ok(serde_json::to_value(broker.begin_login(&s("url")?).await?).unwrap_or_default()),
        "deliver" => broker.deliver(&s("state")?, &s("query")?).await.map(|_| json!({})),
        "wait" => {
            let secs = body.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(600).clamp(1, crate::mcpc::LOGIN_TTL.as_secs());
            broker.wait_login(&s("state")?, Duration::from_secs(secs)).await.map(|_| json!({}))
        }
        "logout" => {
            broker.logout(&s("url")?).await;
            Ok(json!({}))
        }
        other => Err(BrokerError::Unsupported(format!("no operation `{other}`"))),
    }
}
