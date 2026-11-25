//! Typed client for the DFAE HTTP API (plus SSE).

use dfae_core::{Event, TaskRecord, TaskRequest};
use futures_util::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    pub base: String,
    token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskRow {
    pub task_id: String,
    pub goal: String,
    pub status: dfae_core::TaskStatus,
    pub mode: Option<dfae_core::ExecMode>,
    #[allow(dead_code)]
    pub verification: Option<dfae_core::VerificationStatus>,
    pub pending_approvals: usize,
    pub ledger: dfae_core::LedgerSummary,
    #[allow(dead_code)]
    pub session_id: String,
}

impl Api {
    pub fn new(base: &str, token: Option<String>) -> Self {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build().expect("http client");
        Self { http, base: base.trim_end_matches('/').to_string(), token }
    }

    fn rb(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut b = self.http.request(m, format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            b = b.bearer_auth(t);
        }
        b
    }

    async fn send<T: DeserializeOwned>(&self, b: reqwest::RequestBuilder) -> Result<T, String> {
        let r = b.timeout(Duration::from_secs(10)).send().await.map_err(|e| e.to_string())?;
        let status = r.status();
        if !status.is_success() && status != reqwest::StatusCode::SERVICE_UNAVAILABLE {
            let body: Value = r.json().await.unwrap_or(Value::Null);
            return Err(body["error"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {status}")));
        }
        r.json().await.map_err(|e| e.to_string())
    }

    pub async fn health(&self) -> Result<Value, String> {
        self.send(self.rb(reqwest::Method::GET, "/v1/health")).await
    }
    pub async fn info(&self) -> Result<Value, String> {
        self.send(self.rb(reqwest::Method::GET, "/v1/info")).await
    }
    pub async fn stats(&self) -> Result<Value, String> {
        self.send(self.rb(reqwest::Method::GET, "/v1/stats")).await
    }
    pub async fn tasks(&self) -> Result<Vec<TaskRow>, String> {
        self.send(self.rb(reqwest::Method::GET, "/v1/tasks?limit=60")).await
    }
    pub async fn task(&self, id: &str) -> Result<TaskRecord, String> {
        self.send(self.rb(reqwest::Method::GET, &format!("/v1/tasks/{id}"))).await
    }
    pub async fn create(&self, req: &TaskRequest) -> Result<String, String> {
        let v: Value = self.send(self.rb(reqwest::Method::POST, "/v1/tasks").json(req)).await?;
        v["task_id"].as_str().map(String::from).ok_or_else(|| "no task id in response".into())
    }
    pub async fn cancel(&self, id: &str) -> Result<(), String> {
        let _: Value = self.send(self.rb(reqwest::Method::POST, &format!("/v1/tasks/{id}/cancel"))).await?;
        Ok(())
    }
    pub async fn decide(&self, task: &str, approval: &str, approve: bool) -> Result<(), String> {
        let _: Value = self
            .send(self.rb(reqwest::Method::POST, &format!("/v1/tasks/{task}/approvals/{approval}")).json(&json!({"approve": approve, "by": "tui"})))
            .await?;
        Ok(())
    }
    pub async fn memory_search(&self, q: &str) -> Result<Vec<Value>, String> {
        self.send(self.rb(reqwest::Method::GET, "/v1/memory/search").query(&[("q", q), ("limit", "8")])).await
    }
    pub async fn remember(&self, text: &str) -> Result<(), String> {
        let _: Value = self.send(self.rb(reqwest::Method::POST, "/v1/memory").json(&json!({"text": text}))).await?;
        Ok(())
    }

    /// Stream a task's events (backlog replay + live) until the connection drops.
    pub async fn stream_events(&self, id: &str, mut on_event: impl FnMut(Event)) -> Result<(), String> {
        let r = self.rb(reqwest::Method::GET, &format!("/v1/tasks/{id}/events")).send().await.map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("HTTP {}", r.status()));
        }
        let mut buf = String::new();
        let mut s = r.bytes_stream();
        while let Some(chunk) = s.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            buf.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
            while let Some(pos) = buf.find("\n\n") {
                let frame: String = buf.drain(..pos + 2).collect();
                let data: String = frame.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.trim_start()).collect::<Vec<_>>().join("\n");
                if data.is_empty() {
                    continue;
                }
                if let Ok(e) = serde_json::from_str::<Event>(&data) {
                    on_event(e);
                }
            }
        }
        Ok(())
    }
}
