//! Adapter for the Jev typed decision model.
//!
//! Contract (from the AI/ML API reference): `POST {url}` with
//! `Authorization: Bearer <key>` and body
//! `{ "model", "state", "questions": { key: {type, instructions, criteria} } }`;
//! the response is `{ "model", "answers": { key: typed answer }, "usage": {input_tokens, output_tokens} }`.

use async_trait::async_trait;
use dfae_core::*;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub struct JevProvider {
    http: reqwest::Client,
    url: String,
    model: String,
    key: String,
}

#[derive(Deserialize)]
struct JevUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct JevResponse {
    answers: BTreeMap<String, Answer>,
    #[serde(default)]
    usage: Option<JevUsage>,
}

impl JevProvider {
    pub fn new(cfg: &JevConfig) -> Option<Self> {
        let key = cfg.api_key.clone()?;
        let http = reqwest::Client::builder().timeout(Duration::from_millis(cfg.timeout_ms)).build().ok()?;
        Some(Self { http, url: cfg.url.clone(), model: cfg.model.clone(), key })
    }

    /// Reject answers that do not match the question that was asked.
    fn validate(req: &DecisionRequest, answers: &BTreeMap<String, Answer>) -> Result<(), DecisionError> {
        for (k, q) in &req.questions {
            let a = answers.get(k).ok_or_else(|| DecisionError::Invalid(format!("missing answer for '{k}'")))?;
            match (q, a) {
                (Question::Noul { .. }, Answer::Noul { noul }) if (0.0..=1.0).contains(noul) => {}
                (Question::Choice { criteria, .. }, Answer::Choice { choice, .. }) if criteria.contains_key(choice) => {}
                (Question::Score { criteria, .. }, Answer::Score { score, .. }) if *score >= -0.01 && *score <= criteria.len() as f64 => {}
                _ => return Err(DecisionError::Invalid(format!("answer for '{k}' does not match its question"))),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl DecisionProvider for JevProvider {
    fn name(&self) -> &str {
        "jev"
    }

    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let started = Instant::now();
        let body = json!({
            "model": self.model,
            "state": request.state,
            "questions": request.questions,
        });
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(|e| DecisionError::Unavailable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(DecisionError::Failed(format!("HTTP {status}: {}", txt.chars().take(200).collect::<String>())));
        }
        let parsed: JevResponse = resp.json().await.map_err(|e| DecisionError::Invalid(e.to_string()))?;
        Self::validate(&request, &parsed.answers)?;
        let (i, o) = parsed.usage.map(|u| (u.input_tokens, u.output_tokens)).unwrap_or((0, 0));
        Ok(DecisionResponse {
            answers: parsed.answers,
            provider: "jev".into(),
            input_tokens: i,
            output_tokens: o,
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }
}
