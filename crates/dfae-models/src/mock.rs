use async_trait::async_trait;
use dfae_core::*;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// Returns pre-recorded responses in order and records every request.
/// For unit and integration tests.
#[derive(Clone, Default)]
pub struct ScriptedModel {
    responses: Arc<Mutex<VecDeque<Result<String, String>>>>,
    pub requests: Arc<Mutex<Vec<GenerationRequest>>>,
    name: String,
}

impl ScriptedModel {
    pub fn new(name: &str, responses: Vec<&str>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().map(|s| Ok(s.to_string())).collect())),
            requests: Arc::default(),
            name: name.into(),
        }
    }
    pub fn push(&self, r: &str) {
        self.responses.lock().push_back(Ok(r.to_string()));
    }
    pub fn push_err(&self, e: &str) {
        self.responses.lock().push_back(Err(e.to_string()));
    }
    pub fn calls(&self) -> usize {
        self.requests.lock().len()
    }
}

#[async_trait]
impl LanguageModel for ScriptedModel {
    fn name(&self) -> &str {
        &self.name
    }
    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, ModelError> {
        self.requests.lock().push(req);
        match self.responses.lock().pop_front() {
            Some(Ok(text)) => Ok(GenerationResponse { text, usage: Usage::default(), model: self.name.clone() }),
            Some(Err(e)) => Err(ModelError::Request(e)),
            None => Err(ModelError::Request("scripted model exhausted".into())),
        }
    }
}

/// A deterministic stand-in for a real SLM so the whole stack can be smoke
/// tested with no model server. It plans a single reasoning step and answers
/// by echoing the evidence it is given. It does not reason.
#[derive(Default)]
pub struct OfflineStub;

#[async_trait]
impl LanguageModel for OfflineStub {
    fn name(&self) -> &str {
        "offline-stub"
    }
    async fn generate(&self, req: GenerationRequest) -> Result<GenerationResponse, ModelError> {
        let user = req.messages.last().map(|m| m.content.clone()).unwrap_or_default();
        let text = match req.purpose.as_str() {
            p if p.starts_with("plan") || p == "replan" => {
                r#"{"steps":[{"id":"s1","do":"Answer the request from the context","tool":null}]}"#.to_string()
            }
            "answer" => {
                let ev = user.split("EVIDENCE:").nth(1).unwrap_or(&user).trim();
                format!("[offline stub] {}", ev.chars().take(600).collect::<String>())
            }
            _ => format!("[offline stub] {}", user.chars().take(200).collect::<String>()),
        };
        let usage = Usage {
            prompt_tokens: tokens::estimate_tokens(&req.system) as u64 + tokens::estimate_tokens(&user) as u64,
            completion_tokens: tokens::estimate_tokens(&text) as u64,
        };
        Ok(GenerationResponse { text, usage, model: "offline-stub".into() })
    }
}
