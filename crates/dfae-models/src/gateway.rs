use chrono::Utc;
use dfae_core::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Slm,
    Strong,
}

/// One completed model call, ready to be recorded in the ledger.
#[derive(Debug, Clone)]
pub struct Generated {
    pub text: String,
    pub entry: LedgerEntry,
}

/// Routes generation requests to the configured tiers and does the token
/// accounting. The gateway never decides *which* tier — the runtime does,
/// based on the decision layer.
#[derive(Clone)]
pub struct ModelGateway {
    slm: Option<Arc<dyn LanguageModel>>,
    strong: Option<Arc<dyn LanguageModel>>,
}

impl ModelGateway {
    pub fn new(slm: Option<Arc<dyn LanguageModel>>, strong: Option<Arc<dyn LanguageModel>>) -> Self {
        Self { slm, strong }
    }

    pub fn has(&self, tier: Tier) -> bool {
        match tier {
            Tier::Slm => self.slm.is_some(),
            Tier::Strong => self.strong.is_some(),
        }
    }

    pub fn model_name(&self, tier: Tier) -> Option<String> {
        match tier {
            Tier::Slm => self.slm.as_ref().map(|m| m.name().to_string()),
            Tier::Strong => self.strong.as_ref().map(|m| m.name().to_string()),
        }
    }

    pub async fn health(&self, tier: Tier) -> Option<bool> {
        match tier {
            Tier::Slm => match &self.slm {
                Some(m) => Some(m.health().await),
                None => None,
            },
            Tier::Strong => match &self.strong {
                Some(m) => Some(m.health().await),
                None => None,
            },
        }
    }

    pub async fn generate(&self, tier: Tier, req: GenerationRequest) -> Result<Generated, ModelError> {
        let model = match tier {
            Tier::Slm => self.slm.as_ref(),
            Tier::Strong => self.strong.as_ref(),
        }
        .ok_or_else(|| ModelError::NotConfigured(format!("{tier:?} tier")))?;
        let est_prompt = tokens::estimate_tokens(&req.system) as u64 + req.messages.iter().map(|m| tokens::estimate_tokens(&m.content) as u64).sum::<u64>();
        let purpose = req.purpose.clone();
        let started = Instant::now();
        let resp = model.generate(req).await?;
        let latency_ms = started.elapsed().as_millis() as u64;
        let (p, c) = if resp.usage.prompt_tokens + resp.usage.completion_tokens > 0 {
            (resp.usage.prompt_tokens, resp.usage.completion_tokens)
        } else {
            (est_prompt, tokens::estimate_tokens(&resp.text) as u64)
        };
        Ok(Generated {
            entry: LedgerEntry {
                ts: Utc::now(),
                component: if tier == Tier::Slm { Component::Slm } else { Component::Strong },
                model: resp.model,
                purpose,
                prompt_tokens: p,
                completion_tokens: c,
                latency_ms,
                cached: false,
            },
            text: resp.text,
        })
    }

    /// Generate and parse a JSON object; on parse failure retry once with a
    /// terse correction. Returns every call made so the caller can account them.
    pub async fn generate_json(&self, tier: Tier, mut req: GenerationRequest) -> Result<(Value, Vec<Generated>), ModelError> {
        req.json = true;
        let first = self.generate(tier, req.clone()).await?;
        if let Some(v) = extract_json(&first.text) {
            return Ok((v, vec![first]));
        }
        let mut retry = req;
        retry.messages.push(Message::assistant(first.text.chars().take(400).collect::<String>()));
        retry.messages.push(Message::user("That was not valid JSON. Reply with ONLY the JSON object, no prose, no code fences."));
        let second = self.generate(tier, retry).await?;
        match extract_json(&second.text) {
            Some(v) => Ok((v, vec![first, second])),
            None => Err(ModelError::InvalidOutput(format!("no JSON object in output: {}", second.text.chars().take(120).collect::<String>()))),
        }
    }
}

/// Extract the first balanced JSON object from model output (tolerates
/// code fences and surrounding prose).
pub fn extract_json(text: &str) -> Option<Value> {
    let t = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t)
        && v.is_object()
    {
        return Some(v);
    }
    let bytes: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '{' {
            let (mut depth, mut in_str, mut esc) = (0i32, false, false);
            for j in i..bytes.len() {
                let c = bytes[j];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        in_str = false;
                    }
                    continue;
                }
                match c {
                    '"' => in_str = true,
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            let s: String = bytes[i..=j].iter().collect();
                            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                                return Some(v);
                            }
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedModel;

    #[test]
    fn extracts_json_from_noise() {
        let v = extract_json("Sure! ```json\n{\"a\": {\"b\": \"}\"}, \"c\": 1}\n``` hope it helps").unwrap();
        assert_eq!(v["c"], 1);
        assert!(extract_json("no json here").is_none());
        assert!(extract_json("[1,2,3]").is_none());
    }

    fn req() -> GenerationRequest {
        GenerationRequest { system: "s".into(), messages: vec![Message::user("u")], max_tokens: 100, temperature: 0.0, json: true, purpose: "plan".into() }
    }

    #[tokio::test]
    async fn repairs_invalid_json_once() {
        let m = ScriptedModel::new("m", vec!["oops not json", "{\"ok\": true}"]);
        let gw = ModelGateway::new(Some(Arc::new(m.clone())), None);
        let (v, calls) = gw.generate_json(Tier::Slm, req()).await.unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(calls.len(), 2);
        assert_eq!(m.calls(), 2);
        // estimated tokens are recorded when the provider reports none
        assert!(calls[0].entry.prompt_tokens > 0);
    }

    #[tokio::test]
    async fn fails_after_one_repair() {
        let gw = ModelGateway::new(Some(Arc::new(ScriptedModel::new("m", vec!["x", "y"]))), None);
        assert!(gw.generate_json(Tier::Slm, req()).await.is_err());
    }

    #[tokio::test]
    async fn missing_tier_is_an_error() {
        let gw = ModelGateway::new(None, None);
        assert!(matches!(gw.generate(Tier::Strong, req()).await, Err(ModelError::NotConfigured(_))));
    }
}
