//! A hosted decision model behind an OpenAI-compatible **Decisions API** (`POST /v1/decisions`): Vercel AI Gateway serves
//! `openai/gpt-6-luna-decisions` this way, and OpenAI's own API at `api.openai.com/v1/decisions`.
//!
//! The API answers exactly the three kinds of typed question the engine asks — `predicate` (our `noul`: a probability), `choice`
//! (probabilities per option and a confidence) and `score` (a probability-weighted level index, lowest = 0) — many at once over
//! one shared input. So a whole decision (`route_task`'s dozen questions) is **one request**, the shared state is cached by the
//! provider, and only input tokens are billed. Answers are calibrated probabilities like the self-hosted model's.

use async_trait::async_trait;
use dfae_core::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// What a hosted model is not told by the request itself: what the agent around it can do. Without it, "which files mention TODO?"
/// reads as a general-knowledge question with no tools needed. (A self-hosted model gets the same facts from its own prompt.)
const CONTEXT: &str = "CONTEXT: you route requests for an agent that has a workspace of the user's files and code, a sandboxed shell on this \
machine, a calculator, web search and page fetching, and a memory. A request about files, code, this machine, a URL, arithmetic or \
information that changes over time needs those tools; small talk, explanations and general knowledge do not.";

pub struct HostedDecisions {
    http: reqwest::Client,
    url: String,
    model: String,
    key: Option<String>,
    paused_until: Mutex<Option<Instant>>,
    pause: Duration,
}

impl HostedDecisions {
    /// `base` is the API root (`https://ai-gateway.vercel.sh/v1`); `/decisions` is appended.
    pub fn new(base: &str, model: &str, key: Option<String>, timeout: Duration) -> Option<Self> {
        let http = reqwest::Client::builder().timeout(timeout).build().ok()?;
        Some(Self {
            http,
            url: format!("{}/decisions", base.trim_end_matches('/')),
            model: model.into(),
            key,
            paused_until: Mutex::new(None),
            pause: Duration::from_secs(30),
        })
    }

    pub fn with_pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }
}

/// The wire form of one question.
pub(crate) fn wire_question(name: &str, q: &Question) -> Value {
    match q {
        Question::Noul { instructions, criteria } => {
            let mut text = instructions.clone();
            if let Some(c) = criteria {
                if let Some(y) = &c.yes {
                    text.push_str(&format!("\nYes means: {y}"));
                }
                if let Some(n) = &c.no {
                    text.push_str(&format!("\nNo means: {n}"));
                }
            }
            json!({"type": "predicate", "name": name, "instructions": text})
        }
        Question::Choice { instructions, criteria } => json!({
            "type": "choice",
            "name": name,
            "instructions": instructions,
            "choices": criteria.iter().map(|(k, d)| json!({"value": k, "description": d})).collect::<Vec<_>>(),
        }),
        Question::Score { instructions, criteria } => json!({
            "type": "score",
            "name": name,
            "instructions": instructions,
            "levels": criteria
                .iter()
                .enumerate()
                .map(|(i, d)| json!({"label": format!("{}", i + 1), "description": d}))
                .collect::<Vec<_>>(),
        }),
    }
}

/// One wire answer → our typed answer, checked against the question it answers.
pub(crate) fn read_answer(q: &Question, a: &Value) -> Result<Answer, DecisionError> {
    let bad = |why: &str| DecisionError::Invalid(format!("decisions API: {why}"));
    match (q, a["type"].as_str()) {
        (_, Some("refusal")) => Err(bad("the question was refused")),
        (Question::Noul { .. }, Some("predicate")) => a["probability"].as_f64().map(Answer::noul).ok_or_else(|| bad("no probability")),
        (Question::Choice { criteria, .. }, Some("choice")) => {
            let choice = a["choice"].as_str().ok_or_else(|| bad("no choice"))?;
            if !criteria.contains_key(choice) {
                return Err(bad(&format!("'{choice}' is not one of the options")));
            }
            let probs: BTreeMap<String, f64> = a["probabilities"]
                .as_array()
                .map(|ps| ps.iter().filter_map(|p| Some((p["value"].as_str()?.to_string(), p["probability"].as_f64()?))).collect())
                .unwrap_or_default();
            let conf = a["confidence"].as_f64().or_else(|| probs.get(choice).copied()).unwrap_or(0.5);
            Ok(Answer::Choice { choice: choice.into(), confidence: conf.clamp(0.0, 1.0), probabilities: probs })
        }
        (Question::Score { criteria, .. }, Some("score")) => {
            let score = a["score"].as_f64().ok_or_else(|| bad("no score"))?;
            let probs: BTreeMap<String, f64> = a["probabilities"]
                .as_array()
                .map(|ps| ps.iter().filter_map(|p| Some(((p["value"].as_u64()? + 1).to_string(), p["probability"].as_f64()?))).collect())
                .unwrap_or_default();
            let conf = a["confidence"].as_f64().unwrap_or(0.5);
            Ok(Answer::Score {
                score: score.clamp(0.0, criteria.len().saturating_sub(1) as f64),
                confidence: conf.clamp(0.0, 1.0),
                probabilities: probs,
                legend: Some(json!(criteria.iter().enumerate().map(|(i, d)| ((i + 1).to_string(), d.clone())).collect::<BTreeMap<_, _>>())),
            })
        }
        (_, t) => Err(bad(&format!("an answer of type {t:?} does not fit the question"))),
    }
}

#[async_trait]
impl DecisionProvider for HostedDecisions {
    fn name(&self) -> &str {
        "decisions-api"
    }

    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        if self.paused_until.lock().is_some_and(|t| Instant::now() < t) {
            return Err(DecisionError::Unavailable("the decisions API is paused after an error".into()));
        }
        let started = Instant::now();
        let questions: Vec<Value> = request.questions.iter().map(|(k, q)| wire_question(k, q)).collect();
        let input = format!("{CONTEXT}\n{}", crate::open::render_state(&request.state));
        let body = json!({"model": self.model, "input": input, "questions": questions});
        let mut rb = self.http.post(&self.url).json(&body);
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        let out = async {
            let resp = rb.send().await.map_err(|e| DecisionError::Unavailable(e.to_string()))?;
            let status = resp.status();
            if !status.is_success() {
                let text: String = resp.text().await.unwrap_or_default().chars().take(300).collect();
                return Err(DecisionError::Unavailable(format!("HTTP {status}: {text}")));
            }
            resp.json::<Value>().await.map_err(|e| DecisionError::Invalid(e.to_string()))
        }
        .await;
        let v = match out {
            Ok(v) => v,
            Err(e) => {
                *self.paused_until.lock() = Some(Instant::now() + self.pause);
                return Err(e);
            }
        };
        let by_name: BTreeMap<&str, &Value> =
            v["answers"].as_array().map(|a| a.iter().filter_map(|x| Some((x["name"].as_str()?, x))).collect()).unwrap_or_default();
        let mut answers = BTreeMap::new();
        for (k, q) in &request.questions {
            let a = by_name.get(k.as_str()).ok_or_else(|| DecisionError::Invalid(format!("decisions API: no answer for '{k}'")))?;
            answers.insert(k.clone(), read_answer(q, a)?);
        }
        let usage = &v["usage"];
        let cost = v["provider_metadata"]["gateway"]["cost"].as_str().and_then(|c| c.parse::<f64>().ok());
        Ok(DecisionResponse {
            answers,
            provider: "decisions-api".into(),
            model: v["model"].as_str().unwrap_or(&self.model).to_string(),
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
            latency_ms: started.elapsed().as_millis() as u64,
            cost_usd: cost,
        })
    }

    async fn health(&self) -> Option<bool> {
        Some(!self.paused_until.lock().is_some_and(|t| Instant::now() < t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_question_kinds_go_out_and_come_back_typed() {
        let n = Question::noul("Is it?");
        assert_eq!(wire_question("x", &n)["type"], "predicate");
        assert!(matches!(read_answer(&n, &json!({"type": "predicate", "probability": 0.8})).unwrap(), Answer::Noul { noul } if (noul - 0.8).abs() < 1e-9));

        let c = Question::choice("Which?", [("a", "first"), ("b", "second")]);
        let w = wire_question("c", &c);
        assert_eq!(w["choices"][1]["value"], "b");
        let a = read_answer(
            &c,
            &json!({"type": "choice", "choice": "b", "confidence": 0.7, "probabilities": [{"value": "a", "probability": 0.2}, {"value": "b", "probability": 0.8}]}),
        )
        .unwrap();
        assert!(matches!(a, Answer::Choice { ref choice, .. } if choice == "b"));
        assert!(read_answer(&c, &json!({"type": "choice", "choice": "z"})).is_err(), "an option that was not offered is refused");

        let s = Question::score("How hard?", ["easy", "medium", "hard"]);
        assert_eq!(wire_question("s", &s)["levels"].as_array().unwrap().len(), 3);
        let a = read_answer(&s, &json!({"type": "score", "score": 1.7, "confidence": 0.6, "probabilities": [{"value": 2, "probability": 0.7}]})).unwrap();
        assert!(matches!(a, Answer::Score { score, .. } if (score - 1.7).abs() < 1e-9), "lowest level is 0, as the self-hosted model reports it");
        assert!(read_answer(&s, &json!({"type": "refusal"})).is_err());
    }
}
