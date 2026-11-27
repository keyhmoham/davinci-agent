//! A decision model you host yourself: any open-weight instruct model behind an
//! OpenAI-compatible server that returns token log-probabilities (Ollama, vLLM,
//! llama.cpp server, …).
//!
//! The model never writes an answer. Each typed question is turned into a
//! single-token multiple choice and the **probability mass of the answer tokens**
//! is read from the first generated token's log-probabilities:
//!
//! * `noul`   — P(yes) / (P(yes) + P(no))
//! * `choice` — options are lettered `A, B, …`; probabilities are the normalised
//!   masses of the letters, the choice is the argmax
//! * `score`  — levels are numbered `1..n`; the score is the expected level index
//!
//! So the probabilities are real model beliefs (not self-reported numbers), the
//! output is always schema-valid, and a decision costs one prompt and one
//! generated token. The shared state comes first and the question last, so servers
//! with prompt caching reuse the state across the questions of a request.

use async_trait::async_trait;
use dfae_core::*;
use futures_util::stream::{self, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SYSTEM: &str = "You are the decision component of an agent runtime. You judge requests and situations. \
Reply with exactly one token from the allowed answers: no explanation, no punctuation.";

/// Below this much probability on valid answer tokens the reply is not trusted.
const MIN_VALID_MASS: f64 = 0.3;

/// A reqwest error with its causes ("error sending request: operation timed out").
fn describe(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        out.push_str(&format!(": {s}"));
        src = s.source();
    }
    out
}

/// While the model is still loading, or right after it failed, decisions go straight to the local
/// rules instead of each waiting out a timeout (a task makes several decisions).
struct Gate {
    ready: bool,
    paused_until: Option<Instant>,
}

pub struct OpenModelProvider {
    http: reqwest::Client,
    /// no overall timeout beyond a generous one: the first request loads the model
    warm_http: reqwest::Client,
    base: String,
    model: String,
    key: Option<String>,
    parallel: usize,
    gate: Mutex<Gate>,
    pause: Duration,
    warm_retry: Duration,
}

impl OpenModelProvider {
    pub fn new(cfg: &DecisionConfig) -> Option<Self> {
        let base = cfg.base_url.clone()?.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder().timeout(Duration::from_millis(cfg.timeout_ms)).build().ok()?;
        let warm_http = reqwest::Client::builder().timeout(Duration::from_secs(300)).build().ok()?;
        Some(Self {
            http,
            warm_http,
            base,
            model: cfg.model.clone(),
            key: cfg.api_key.clone(),
            parallel: cfg.parallel.max(1),
            gate: Mutex::new(Gate { ready: true, paused_until: None }),
            pause: Duration::from_secs(30),
            warm_retry: Duration::from_secs(5),
        })
    }

    /// Test hook: how long a failure pauses the model, and how often warm-up retries.
    pub fn with_timing(mut self, pause: Duration, warm_retry: Duration) -> Self {
        self.pause = pause;
        self.warm_retry = warm_retry;
        self
    }

    /// Load the model in the background. Until the first answer arrives, decisions use the local
    /// rules, so a cold start (or a model that is still being pulled) never stalls a task.
    pub fn start_warmup(self: &Arc<Self>) {
        self.gate.lock().ready = false;
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let prompt = "REQUEST: ready\n\nQUESTION: Is this a test?\nAllowed answers: yes, no".to_string();
                match me.first_token(&me.warm_http, prompt).await {
                    Ok(_) => {
                        let mut g = me.gate.lock();
                        g.ready = true;
                        g.paused_until = None;
                        tracing::info!(model = %me.model, "decision model is ready");
                        return;
                    }
                    Err(e) => {
                        tracing::info!(model = %me.model, error = %e, "decision model not ready yet; decisions use the local rules");
                        tokio::time::sleep(me.warm_retry).await;
                    }
                }
            }
        });
    }

    /// One single-token completion: the first token's top alternatives as `(token, probability)`.
    async fn first_token(&self, client: &reqwest::Client, prompt: String) -> Result<(Vec<(String, f64)>, u64, u64), DecisionError> {
        let body = json!({
            "model": self.model,
            "messages": [{"role": "system", "content": SYSTEM}, {"role": "user", "content": prompt}],
            "max_tokens": 1,
            "temperature": 0,
            "logprobs": true,
            "top_logprobs": 20,
            "stream": false,
        });
        let mut rb = client.post(format!("{}/chat/completions", self.base)).json(&body);
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await.map_err(|e| DecisionError::Unavailable(describe(&e)))?;
        let status = resp.status();
        if !status.is_success() {
            let txt = resp.text().await.unwrap_or_default();
            return Err(DecisionError::Failed(format!("HTTP {status}: {}", txt.chars().take(200).collect::<String>())));
        }
        let v: Value = resp.json().await.map_err(|e| DecisionError::Invalid(e.to_string()))?;
        let top = v["choices"][0]["logprobs"]["content"][0]["top_logprobs"]
            .as_array()
            .ok_or_else(|| DecisionError::Invalid("the server returned no log-probabilities (use Ollama 0.12.11+, vLLM or llama.cpp server)".into()))?;
        let toks: Vec<(String, f64)> = top.iter().filter_map(|t| Some((t["token"].as_str()?.to_string(), t["logprob"].as_f64()?.exp()))).collect();
        if toks.is_empty() {
            return Err(DecisionError::Invalid("empty log-probabilities".into()));
        }
        Ok((toks, v["usage"]["prompt_tokens"].as_u64().unwrap_or(0), v["usage"]["completion_tokens"].as_u64().unwrap_or(0)))
    }

    async fn answer(&self, state: &str, q: &Question) -> Result<(Answer, u64, u64), DecisionError> {
        let (prompt, labels) = render_question(state, q);
        let (toks, i, o) = self.first_token(&self.http, prompt).await?;
        Ok((interpret(q, &labels, &toks)?, i, o))
    }
}

#[async_trait]
impl DecisionProvider for OpenModelProvider {
    fn name(&self) -> &str {
        "open-model"
    }

    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        {
            let g = self.gate.lock();
            if !g.ready {
                return Err(DecisionError::Unavailable("the decision model is still loading".into()));
            }
            if g.paused_until.map(|t| Instant::now() < t).unwrap_or(false) {
                return Err(DecisionError::Unavailable("the decision model is paused after an error".into()));
            }
        }
        let out = self.decide_inner(request).await;
        let mut g = self.gate.lock();
        match &out {
            Ok(_) => g.paused_until = None,
            // a malformed answer is the model's fault for that question, not a reason to stop asking
            Err(DecisionError::Invalid(_)) => {}
            Err(_) => g.paused_until = Some(Instant::now() + self.pause),
        }
        out
    }

    async fn health(&self) -> Option<bool> {
        self.check_health().await
    }
}

impl OpenModelProvider {
    async fn decide_inner(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        let started = Instant::now();
        let state = render_state(&request.state);
        let jobs: Vec<(String, Question)> = request.questions.iter().map(|(k, q)| (k.clone(), q.clone())).collect();
        let results: Vec<Result<(String, Answer, u64, u64), DecisionError>> = stream::iter(jobs)
            .map(|(k, q)| {
                let state = &state;
                async move { self.answer(state, &q).await.map(|(a, i, o)| (k, a, i, o)) }
            })
            .buffer_unordered(self.parallel)
            .collect()
            .await;
        let mut answers = BTreeMap::new();
        let (mut tin, mut tout) = (0, 0);
        for r in results {
            let (k, a, i, o) = r?;
            answers.insert(k, a);
            tin += i;
            tout += o;
        }
        Ok(DecisionResponse {
            answers,
            provider: "open-model".into(),
            model: self.model.clone(),
            input_tokens: tin,
            output_tokens: tout,
            latency_ms: started.elapsed().as_millis() as u64,
            cost_usd: None,
        })
    }
}

impl OpenModelProvider {
    async fn check_health(&self) -> Option<bool> {
        let mut rb = self.http.get(format!("{}/models", self.base)).timeout(Duration::from_secs(3));
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        let Ok(resp) = rb.send().await else { return Some(false) };
        if !resp.status().is_success() {
            return Some(false);
        }
        let norm = |m: &str| m.strip_suffix(":latest").unwrap_or(m).to_string();
        let want = norm(&self.model);
        Some(match resp.json::<Value>().await.ok().and_then(|v| v["data"].as_array().cloned()) {
            Some(models) => models.iter().filter_map(|m| m["id"].as_str()).any(|id| norm(id) == want),
            None => true,
        })
    }
}

// ------------------------------------------------------------- prompts ----

fn clip(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n { format!("{t}…") } else { t }
}

/// The situation, as compact lines (shared by every question of a request).
pub(crate) fn render_state(s: &DecisionState) -> String {
    // the decision model is self-hosted, so its context is generous: a decision is only as good as what it can see
    let mut out = Vec::new();
    if !s.recent.is_empty() {
        out.push(format!("RECENT CONVERSATION (oldest first):\n{}", s.recent.iter().map(|t| format!("- {}", clip(t, 600))).collect::<Vec<_>>().join("\n")));
    }
    if !s.known.is_empty() {
        out.push(format!("KNOWN FROM THE CONVERSATION: {}", s.known.join("; ")));
    }
    out.push(format!("REQUEST: {}", clip(&s.request, 6000)));
    if !s.entities.is_empty() {
        out.push(format!("ENTITIES: {}", s.entities.iter().map(|e| format!("{}={}", e.label, clip(&e.text, 300))).collect::<Vec<_>>().join(" | ")));
    }
    if let Some(m) = &s.mode_facts {
        out.push(format!(
            "FACTS: deterministic_binding={} skill_match={} complexity={:.2} risk={} task_type={} small_model={} planner_model={}",
            m.direct_bindable,
            m.skill_score.map(|x| format!("{x:.2}")).unwrap_or_else(|| "none".into()),
            m.complexity,
            m.risk.map(|r| format!("{r:?}").to_lowercase()).unwrap_or_else(|| "unknown".into()),
            m.task_type,
            m.worker_available,
            m.planner_available
        ));
        if m.plan_min_complexity > 0.0 {
            out.push(format!(
                "POLICY: planning is always done by the planner model (when one exists) and carried out by the local system. Choose escalated for any request that needs a plan (several dependent steps, building or serving something, results that decide the next step; complexity {:.2} or more is a hint). Choose assisted only for one quick action or a reply that needs no plan.",
                m.plan_min_complexity
            ));
        }
    }
    if let Some(p) = &s.plan_facts {
        out.push(format!(
            "PLAN: {} steps ({} tool, {} reasoning, {} change something), highest risk {}. {}",
            p.steps,
            p.tool_steps,
            p.reasoning_steps,
            p.side_effect_steps,
            p.max_risk.map(|r| format!("{r:?}").to_lowercase()).unwrap_or_else(|| "none".into()),
            clip(&p.summary, 2000)
        ));
    }
    if let Some(f) = &s.step_facts {
        out.push(format!(
            "FAILED STEP: tool={} error_kind={} error=\"{}\" attempt {}/{} replans {}/{} optional={} planner_model={}",
            f.tool.as_deref().unwrap_or("reasoning"),
            f.error_kind,
            clip(&f.error, 1000),
            f.attempts,
            f.max_attempts,
            f.replans_used,
            f.max_replans,
            f.optional,
            f.planner_available
        ));
    }
    if let Some(c) = &s.completion_facts {
        out.push(format!(
            "RESULT: {}/{} steps ok, {} failed; checks {} passed, {} failed\nEVIDENCE: {}",
            c.steps_ok,
            c.steps_total,
            c.steps_failed,
            c.checks_passed,
            c.checks_failed,
            clip(&c.evidence, 6000)
        ));
    }
    if let Some(t) = &s.team_facts {
        out.push(format!(
            "SUBTASKS: {} | deterministic={} dependent={} independent_model_tasks={} changes_state={} needs_synthesis={} est_tokens one_plan={} team={}",
            t.subgoals.iter().map(|g| format!("\"{}\"", clip(g, 300))).collect::<Vec<_>>().join(", "),
            t.direct,
            t.dependent,
            t.parallel_model,
            t.mutating,
            t.has_synthesis,
            t.est_flat_tokens,
            t.est_team_tokens
        ));
    }
    out.join("\n")
}

const LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// The prompt for one question and the answer labels it expects, in order.
/// Labels for `choice` are letters in key order; the caller maps them back.
pub(crate) fn render_question(state: &str, q: &Question) -> (String, Vec<String>) {
    match q {
        Question::Noul { instructions, criteria } => {
            let mut p = format!("{state}\n\nQUESTION: {instructions}\n");
            if let Some(c) = criteria {
                if let Some(y) = &c.yes {
                    p.push_str(&format!("yes means: {y}\n"));
                }
                if let Some(n) = &c.no {
                    p.push_str(&format!("no means: {n}\n"));
                }
            }
            p.push_str("Allowed answers: yes, no");
            (p, vec!["yes".into(), "no".into()])
        }
        Question::Choice { instructions, criteria } => {
            let mut p = format!("{state}\n\nQUESTION: {instructions}\n");
            let mut labels = Vec::new();
            for (i, (k, d)) in criteria.iter().enumerate().take(26) {
                let l = &LETTERS[i..=i];
                p.push_str(&format!("{l}) {k} — {d}\n"));
                labels.push(l.to_lowercase());
            }
            p.push_str(&format!("Allowed answers: {}", labels.iter().map(|l| l.to_uppercase()).collect::<Vec<_>>().join(", ")));
            (p, labels)
        }
        Question::Score { instructions, criteria } => {
            let mut p = format!("{state}\n\nQUESTION: {instructions}\n");
            let mut labels = Vec::new();
            for (i, d) in criteria.iter().enumerate().take(9) {
                p.push_str(&format!("{}) {d}\n", i + 1));
                labels.push((i + 1).to_string());
            }
            p.push_str(&format!("Allowed answers: {}", labels.join(", ")));
            (p, labels)
        }
    }
}

// ---------------------------------------------------------- probabilities ----

fn norm_token(t: &str) -> String {
    t.trim_start_matches(['▁', 'Ġ', ' ']).trim().trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Probability mass of each label, over every token that spells it (`Yes`, ` yes`, `YES`, `y` …).
fn masses(labels: &[String], toks: &[(String, f64)], aliases: &dyn Fn(&str) -> Option<usize>) -> Vec<f64> {
    let mut m = vec![0.0; labels.len()];
    for (t, p) in toks {
        let n = norm_token(t);
        if let Some(i) = labels.iter().position(|l| *l == n).or_else(|| aliases(&n)) {
            m[i] += p;
        }
    }
    m
}

pub(crate) fn interpret(q: &Question, labels: &[String], toks: &[(String, f64)]) -> Result<Answer, DecisionError> {
    let bad = || DecisionError::Invalid("the model did not answer with one of the allowed tokens".into());
    match q {
        Question::Noul { .. } => {
            let m = masses(labels, toks, &|t| match t {
                "y" => Some(0),
                "n" => Some(1),
                "true" => Some(0),
                "false" => Some(1),
                _ => None,
            });
            let total = m[0] + m[1];
            if total < MIN_VALID_MASS {
                return Err(bad());
            }
            Ok(Answer::noul(m[0] / total))
        }
        Question::Choice { criteria, .. } => {
            let m = masses(labels, toks, &|_| None);
            let total: f64 = m.iter().sum();
            if total < MIN_VALID_MASS {
                return Err(bad());
            }
            let keys: Vec<&String> = criteria.keys().collect();
            let probs: BTreeMap<String, f64> = keys.iter().zip(&m).map(|(k, p)| ((*k).clone(), p / total)).collect();
            let (best, conf) = probs.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).map(|(k, p)| (k.clone(), *p)).ok_or_else(bad)?;
            Ok(Answer::Choice { choice: best, confidence: conf, probabilities: probs })
        }
        Question::Score { criteria, .. } => {
            let m = masses(labels, toks, &|_| None);
            let total: f64 = m.iter().sum();
            if total < MIN_VALID_MASS {
                return Err(bad());
            }
            let probs: Vec<f64> = m.iter().map(|p| p / total).collect();
            let score: f64 = probs.iter().enumerate().map(|(i, p)| i as f64 * p).sum();
            let conf = probs.iter().cloned().fold(0.0, f64::max);
            let legend = json!(criteria.iter().enumerate().map(|(i, d)| ((i + 1).to_string(), d.clone())).collect::<BTreeMap<_, _>>());
            Ok(Answer::Score {
                score,
                confidence: conf,
                probabilities: probs.iter().enumerate().map(|(i, p)| ((i + 1).to_string(), *p)).collect(),
                legend: Some(legend),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(v: &[(&str, f64)]) -> Vec<(String, f64)> {
        v.iter().map(|(t, p)| (t.to_string(), *p)).collect()
    }

    #[test]
    fn yes_no_mass_is_read_from_every_spelling() {
        let q = Question::noul("Is it?");
        let (_, labels) = render_question("S", &q);
        let a = interpret(&q, &labels, &toks(&[("Yes", 0.6), (" yes", 0.1), ("No", 0.2), ("The", 0.1)])).unwrap();
        match a {
            Answer::Noul { noul } => assert!((noul - 0.7 / 0.9).abs() < 1e-9, "{noul}"),
            _ => panic!(),
        }
        assert!(interpret(&q, &labels, &toks(&[("The", 0.9), ("Yes", 0.05)])).is_err(), "an off-format reply is not trusted");
    }

    #[test]
    fn choices_are_lettered_in_key_order_and_mapped_back() {
        let q = Question::choice("Which?", [("research", "look up"), ("coding", "code"), ("other", "none")]);
        let (p, labels) = render_question("STATE", &q);
        // BTreeMap order: coding, other, research
        assert!(p.contains("A) coding") && p.contains("B) other") && p.contains("C) research"), "{p}");
        assert!(p.starts_with("STATE\n\nQUESTION: Which?") && p.ends_with("Allowed answers: A, B, C"));
        let a = interpret(&q, &labels, &toks(&[("C", 0.7), ("A", 0.2), ("B", 0.05)])).unwrap();
        match a {
            Answer::Choice { choice, confidence, probabilities } => {
                assert_eq!(choice, "research");
                assert!((confidence - 0.7 / 0.95).abs() < 1e-9);
                assert!(probabilities["coding"] > probabilities["other"]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn a_score_is_the_expected_level() {
        let q = Question::score("How complex?", ["simple", "intermediate", "complex"]);
        let (p, labels) = render_question("S", &q);
        assert!(p.contains("1) simple") && p.contains("3) complex"));
        let a = interpret(&q, &labels, &toks(&[("1", 0.5), ("2", 0.3), ("3", 0.2)])).unwrap();
        match a {
            Answer::Score { score, confidence, .. } => {
                assert!((score - (0.3 + 0.4)).abs() < 1e-9, "0-based expectation: {score}");
                assert!((confidence - 0.5).abs() < 1e-9);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn the_state_is_compact_and_the_question_comes_last() {
        let s = DecisionState {
            request: "read a.txt".into(),
            entities: vec![EntityBrief { label: "file path".into(), text: "a.txt".into() }],
            mode_facts: Some(ModeFacts {
                direct_bindable: true,
                complexity: 0.2,
                risk: Some(Risk::Low),
                task_type: "file_operations".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let st = render_state(&s);
        assert!(
            st.contains("REQUEST: read a.txt")
                && st.contains("ENTITIES: file path=a.txt")
                && st.contains("deterministic_binding=true")
                && st.contains("risk=low"),
            "{st}"
        );
    }
}
