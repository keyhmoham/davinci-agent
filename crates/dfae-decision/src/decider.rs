use crate::LocalDecider;
use crate::calibrator::Calibrator;
use crate::questions::*;
use chrono::Utc;
use dfae_core::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

pub struct Decided<T> {
    pub value: T,
    pub record: DecisionRecord,
    /// Ledger entry when a hosted decision model was actually called.
    pub entry: Option<LedgerEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Retry,
    Replan,
    Skip,
    Escalate,
    Fail,
}

impl RecoveryAction {
    fn parse(s: &str) -> Self {
        match s {
            "retry" => Self::Retry,
            "replan" => Self::Replan,
            "skip" => Self::Skip,
            "escalate" => Self::Escalate,
            _ => Self::Fail,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModeChoice {
    pub mode: ExecMode,
    pub confidence: f32,
    pub reason: String,
    pub raw_choice: String,
}

/// Facade over the decision providers. Never trusts a provider blindly:
/// answers are validated, hard safety rules are applied afterwards and every
/// decision is recorded.
pub struct Decider {
    primary: Option<Arc<dyn DecisionProvider>>,
    local: LocalDecider,
    pub calibrator: Arc<Calibrator>,
    thresholds: Thresholds,
    cache: Mutex<HashMap<String, DecisionResponse>>,
    /// The decisions the primary provider makes; the others use the local rules.
    model_kinds: std::collections::HashSet<DecisionKind>,
}

const CACHE_CAP: usize = 512;

impl Decider {
    pub fn new(primary: Option<Arc<dyn DecisionProvider>>, calibrator: Arc<Calibrator>, thresholds: Thresholds) -> Self {
        Self {
            primary,
            local: LocalDecider::new(thresholds.clone()),
            calibrator,
            thresholds,
            cache: Mutex::new(HashMap::new()),
            model_kinds: DecisionKind::ALL.into_iter().collect(),
        }
    }

    /// Restrict the primary provider to these decisions (`["all"]` or an empty list = every kind).
    pub fn with_model_kinds(mut self, kinds: &[String]) -> Self {
        if !kinds.is_empty() && !kinds.iter().any(|k| k == "all") {
            self.model_kinds = kinds.iter().filter_map(|k| DecisionKind::parse(k)).collect();
        }
        self
    }

    pub fn provider_name(&self) -> &str {
        self.primary.as_ref().map(|p| p.name()).unwrap_or("local")
    }

    /// Whether the decision model is reachable (`None` when only the local rules are in use).
    pub async fn health(&self) -> Option<bool> {
        self.primary.as_ref()?.health().await
    }

    /// Run a decision request with cache + fallback and produce its record.
    pub async fn decide(&self, req: DecisionRequest) -> (DecisionResponse, DecisionRecord, Option<LedgerEntry>) {
        let key = sha256_hex(&format!("{}|{}", req.kind.as_str(), serde_json::to_string(&(&req.state, &req.questions)).unwrap_or_default()));
        let cached = self.cache.lock().get(&key).cloned();
        let (resp, note, from_cache) = if let Some(c) = cached {
            (c, Some("cache".to_string()), true)
        } else {
            let (resp, note) = match self.primary.as_ref().filter(|_| self.model_kinds.contains(&req.kind)) {
                Some(p) => match p.decide(req.clone()).await {
                    Ok(r) => (r, None),
                    Err(e) => {
                        tracing::warn!(error = %e, "decision provider failed; using local fallback");
                        let mut r = self.local.decide(req.clone()).await.expect("local decider is infallible");
                        r.provider = "local-fallback".into();
                        (r, Some(e.to_string()))
                    }
                },
                None => (self.local.decide(req.clone()).await.expect("local decider is infallible"), None),
            };
            let mut c = self.cache.lock();
            if c.len() >= CACHE_CAP {
                c.clear();
            }
            c.insert(key, resp.clone());
            (resp, note, false)
        };
        let briefs: serde_json::Map<String, Value> = resp.answers.iter().map(|(k, a)| (k.clone(), a.brief())).collect();
        let record = DecisionRecord {
            id: new_id(),
            kind: req.kind.as_str().into(),
            ts: Utc::now(),
            provider: if from_cache { format!("{}+cache", resp.provider) } else { resp.provider.clone() },
            answers: Value::Object(briefs),
            latency_ms: resp.latency_ms,
            tokens_in: if from_cache { 0 } else { resp.input_tokens },
            tokens_out: if from_cache { 0 } else { resp.output_tokens },
            note,
        };
        let entry = (resp.provider == "open-model").then(|| LedgerEntry {
            ts: Utc::now(),
            component: Component::Decision,
            model: resp.model.clone(),
            purpose: req.kind.as_str().into(),
            prompt_tokens: if from_cache { 0 } else { resp.input_tokens },
            completion_tokens: if from_cache { 0 } else { resp.output_tokens },
            cached_tokens: 0,
            latency_ms: resp.latency_ms,
            cached: from_cache,
            origin: None,
        });
        (resp, record, entry)
    }

    // ------------------------------------------------------------ route ----

    pub async fn route_task(&self, state: DecisionState) -> Decided<Routing> {
        let (r, record, entry) = self.decide(route_request(state)).await;
        let (tt, tconf) = r.choice("task_type").map(|(c, p)| (c.to_string(), p)).unwrap_or(("other".into(), 0.3));
        let (cscore, _) = r.score("complexity").unwrap_or((1.0, 0.5));
        let needs = r.noul("needs_tools").unwrap_or(0.5) >= 0.5;
        let (risk_s, rconf) = r.choice("risk").map(|(c, p)| (c.to_string(), p)).unwrap_or(("moderate".into(), 0.5));
        let risk = match risk_s.as_str() {
            "low" => Risk::Low,
            "high" => Risk::High,
            _ => Risk::Moderate,
        };
        let conf = self.calibrator.calibrate(&format!("route_task:{tt}"), (tconf + rconf) / 2.0) as f32;
        let yes = |k: &str| r.noul(k).map(|p| p >= 0.5).unwrap_or(false);
        let pick = |k: &str| r.choice(k).map(|(c, _)| c.to_string());
        let smalltalk = pick("smalltalk").filter(|c| c != "none");
        let role = pick("role");
        let mut routing = Routing::new(&tt, (cscore / 2.0).clamp(0.0, 1.0) as f32, needs, risk, conf);
        routing.changes = yes("changes_state");
        routing.conditional = yes("conditional_change");
        routing.smalltalk = smalltalk;
        routing.role = role;
        routing.feedback = pick("feedback").filter(|c| c != "none");
        Decided { value: routing, record, entry }
    }

    // ------------------------------------------------------------ tools ----

    /// Returns `(tool, relevance)` pairs above the threshold, best first.
    pub async fn select_tools(&self, state: DecisionState) -> Decided<Vec<(String, f32)>> {
        if state.tools.is_empty() {
            let (_, record, entry) = self.decide(tools_request(state)).await;
            return Decided { value: vec![], record, entry };
        }
        // a self-hosted decision model costs nothing per question, so it judges every tool
        let req = tools_request(state);
        let (r, record, entry) = self.decide(req).await;
        let mut scored: Vec<(String, f32)> = r
            .answers
            .iter()
            .filter_map(|(k, a)| match (k.strip_prefix("tool:"), a) {
                (Some(name), Answer::Noul { noul }) => Some((name.to_string(), *noul as f32)),
                _ => None,
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let mut picked: Vec<(String, f32)> = scored.iter().filter(|(_, p)| *p >= self.thresholds.tool_min_relevance).take(10).cloned().collect();
        if picked.is_empty()
            && let Some(top) = scored.first().filter(|(_, p)| *p >= 0.3)
        {
            picked.push(top.clone());
        }
        Decided { value: picked, record, entry }
    }

    // ------------------------------------------------------------- mode ----

    /// Pick the execution mode. Hard rules run *after* the provider: a
    /// provider can never select `direct` when no deterministic binding
    /// exists, nor `escalated` when no stronger model is configured.
    pub async fn select_mode(&self, state: DecisionState, requested: ModeRequest) -> Decided<ModeChoice> {
        let facts = state.mode_facts.clone().unwrap_or_default();
        let (r, record, entry) = self.decide(mode_request(state)).await;
        let (raw, rawconf) = r.choice("mode").map(|(c, p)| (c.to_string(), p)).unwrap_or(("assisted".into(), 0.5));
        let conf = self.calibrator.calibrate(&format!("select_mode:{raw}"), rawconf) as f32;
        let mut mode = match raw.as_str() {
            "direct" => ExecMode::Direct,
            "escalated" => ExecMode::Escalated,
            _ => ExecMode::Assisted,
        };
        let mut reason = match mode {
            ExecMode::Direct => format!("deterministic tool binding · decision confidence {conf:.2}"),
            ExecMode::Assisted => format!("needs planning or reasoning by the SLM · decision confidence {conf:.2}"),
            ExecMode::Escalated => format!(
                "medium or complex work (complexity {:.2}): the external model plans it, the local system carries it out · decision confidence {conf:.2}",
                facts.complexity
            ),
        };

        // user override first
        match requested {
            ModeRequest::Direct => {
                mode = ExecMode::Direct;
                reason = "requested direct".into();
            }
            ModeRequest::Assisted => {
                mode = ExecMode::Assisted;
                reason = "requested assisted".into();
            }
            ModeRequest::Escalated => {
                mode = ExecMode::Escalated;
                reason = "requested escalated".into();
            }
            ModeRequest::Auto => {}
        }
        if mode == ExecMode::Direct && !facts.direct_bindable {
            mode = if facts.slm_available {
                ExecMode::Assisted
            } else if facts.strong_available {
                ExecMode::Escalated
            } else {
                ExecMode::Assisted
            };
            reason = "no deterministic binding for this request".into();
        } else if mode == ExecMode::Direct && requested == ModeRequest::Auto && conf < self.thresholds.direct_min_confidence && facts.slm_available {
            mode = ExecMode::Assisted;
            reason = format!("direct confidence {conf:.2} below {:.2}", self.thresholds.direct_min_confidence);
        }
        if mode == ExecMode::Escalated && !facts.strong_available {
            mode = ExecMode::Assisted;
            reason = "no stronger model configured; using SLM".into();
        }
        if mode == ExecMode::Assisted && !facts.slm_available {
            if facts.strong_available {
                mode = ExecMode::Escalated;
                reason = "no SLM configured; using stronger model".into();
            } else if facts.direct_bindable {
                mode = ExecMode::Direct;
                reason = "no language model configured; using deterministic binding".into();
            }
        }
        if requested == ModeRequest::Auto && mode == ExecMode::Assisted && conf < self.thresholds.escalate_below && facts.strong_available {
            mode = ExecMode::Escalated;
            reason = format!("low confidence {conf:.2}; escalating");
        }
        Decided { value: ModeChoice { mode, confidence: conf, reason, raw_choice: raw }, record, entry }
    }

    // -------------------------------------------------- plan / recovery ----

    pub async fn assess_plan(&self, state: DecisionState) -> Decided<f32> {
        let (r, record, entry) = self.decide(plan_request(state)).await;
        Decided { value: r.noul("plan_ok").unwrap_or(0.5) as f32, record, entry }
    }

    pub async fn assess_recovery(&self, state: DecisionState) -> Decided<RecoveryAction> {
        let facts = state.step_facts.clone().unwrap_or_default();
        // Hard rule: policy denials are never retried or worked around.
        let (r, mut record, entry) = self.decide(recovery_request(state)).await;
        let raw = r.choice("recovery").map(|(c, _)| c.to_string()).unwrap_or_else(|| "fail".into());
        let mut action = RecoveryAction::parse(&raw);
        if matches!(facts.error_kind.as_str(), "policy" | "cancelled") {
            action = RecoveryAction::Fail;
            record.note = Some("hard rule: policy/cancel never recovered".into());
        }
        if action == RecoveryAction::Retry && facts.attempts >= facts.max_attempts {
            action = RecoveryAction::Replan;
        }
        if action == RecoveryAction::Replan && facts.replans_used >= facts.max_replans {
            action = if facts.strong_available { RecoveryAction::Escalate } else { RecoveryAction::Fail };
        }
        if action == RecoveryAction::Skip && !facts.optional {
            action = RecoveryAction::Fail;
        }
        if action == RecoveryAction::Escalate && !facts.strong_available {
            action = RecoveryAction::Fail;
        }
        Decided { value: action, record, entry }
    }

    pub async fn assess_completion(&self, state: DecisionState) -> Decided<f32> {
        let (r, record, entry) = self.decide(completion_request(state)).await;
        Decided { value: r.noul("complete").unwrap_or(0.0) as f32, record, entry }
    }

    /// Should the request be split across a team of agents? The answer is
    /// calibrated against how earlier splits actually turned out.
    pub async fn assess_decomposition(&self, state: DecisionState) -> Decided<f32> {
        let (r, record, entry) = self.decide(decomposition_request(state)).await;
        let raw = r.noul("split").unwrap_or(0.0);
        Decided { value: self.calibrator.calibrate("assess_decomposition:split", raw) as f32, record, entry }
    }

    /// Does the evidence satisfy what a planner expected of one step? A different question from "is the request done":
    /// a correct "nothing is there" satisfies an expectation of showing what exists.
    pub async fn judge_step(&self, state: DecisionState) -> Decided<f32> {
        let mut req = completion_request(state);
        req.questions.insert(
            "complete".into(),
            Question::noul(
                "Does the EVIDENCE satisfy the EXPECTATION of this STEP? Answer yes when it does. A result that correctly reports that something is missing or empty satisfies an expectation of showing what exists. Answer no only when the evidence is an error, is off-topic, or clearly contradicts the expectation.",
            ),
        );
        let (r, record, entry) = self.decide(req).await;
        Decided { value: r.noul("complete").unwrap_or(0.0) as f32, record, entry }
    }

    /// What, if anything, does the request lean on from earlier in the conversation? `options` are what exists
    /// (`file`, `url`, `place`, `code` with a description); the answer is one of them or `None`.
    pub async fn resolve_reference(&self, state: DecisionState, options: &[(String, String)]) -> Decided<Option<String>> {
        let (r, record, entry) = self.decide(resolve_request(state, options)).await;
        let value = r.choice("refers_to").filter(|(c, conf)| *c != "none" && *conf >= 0.5 && options.iter().any(|(k, _)| k == c)).map(|(c, _)| c.to_string());
        Decided { value, record, entry }
    }

    pub async fn assess_escalation(&self, state: DecisionState) -> Decided<f32> {
        let (r, record, entry) = self.decide(escalation_request(state)).await;
        Decided { value: r.noul("escalate").unwrap_or(0.0) as f32, record, entry }
    }

    /// Feed a verified outcome back into calibration.
    pub fn record_outcome(&self, kind: DecisionKind, choice: &str, success: bool) {
        self.calibrator.record(&format!("{}:{choice}", kind.as_str()), success);
    }

    pub fn calibration_snapshot(&self) -> Value {
        json!({ "provider": self.provider_name(), "stats": self.calibrator.snapshot() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    fn decider() -> Decider {
        Decider::new(None, Arc::new(Calibrator::new()), Config::default().thresholds)
    }

    #[tokio::test]
    async fn direct_requires_binding() {
        let d = decider();
        let mut st = DecisionState { request: "calculate 2+2".into(), ..Default::default() };
        st.mode_facts = Some(ModeFacts { direct_bindable: false, slm_available: true, ..Default::default() });
        let m = d.select_mode(st.clone(), ModeRequest::Direct).await.value;
        assert_eq!(m.mode, ExecMode::Assisted);
        st.mode_facts = Some(ModeFacts { direct_bindable: true, slm_available: true, ..Default::default() });
        let m = d.select_mode(st, ModeRequest::Auto).await.value;
        assert_eq!(m.mode, ExecMode::Direct);
    }

    #[tokio::test]
    async fn escalation_needs_a_strong_model() {
        let d = decider();
        let mut st = DecisionState { request: "x".into(), ..Default::default() };
        st.mode_facts = Some(ModeFacts { slm_available: true, strong_available: false, ..Default::default() });
        let m = d.select_mode(st, ModeRequest::Escalated).await.value;
        assert_eq!(m.mode, ExecMode::Assisted);
    }

    #[tokio::test]
    async fn medium_and_complex_tasks_go_to_the_external_model_when_there_is_one() {
        let d = decider();
        let mode = |complexity: f32, direct: bool, strong: bool, requested: ModeRequest| {
            let st = DecisionState {
                request: "x".into(),
                mode_facts: Some(ModeFacts {
                    complexity,
                    direct_bindable: direct,
                    strong_available: strong,
                    slm_available: true,
                    external_min_complexity: 0.3,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let d = &d;
            async move { d.select_mode(st, requested).await.value }
        };
        let m = mode(0.5, false, true, ModeRequest::Auto).await;
        assert_eq!(m.mode, ExecMode::Escalated);
        assert!(m.reason.contains("external model plans"), "{}", m.reason);
        assert_eq!(mode(0.2, false, true, ModeRequest::Auto).await.mode, ExecMode::Assisted, "simple tasks stay local");
        assert_eq!(mode(0.9, true, true, ModeRequest::Auto).await.mode, ExecMode::Direct, "what binds deterministically is never sent out");
        assert_eq!(mode(0.9, false, false, ModeRequest::Auto).await.mode, ExecMode::Assisted, "no external model, no escalation");
        assert_eq!(mode(0.9, false, true, ModeRequest::Assisted).await.mode, ExecMode::Assisted, "an explicit choice wins");
    }

    struct Failing;
    #[async_trait]
    impl DecisionProvider for Failing {
        fn name(&self) -> &str {
            "open-model"
        }
        async fn decide(&self, _r: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
            Err(DecisionError::Unavailable("down".into()))
        }
    }

    #[tokio::test]
    async fn falls_back_to_local_when_provider_fails() {
        let d = Decider::new(Some(Arc::new(Failing)), Arc::new(Calibrator::new()), Config::default().thresholds);
        let st = DecisionState { request: "calculate 1+1".into(), ..Default::default() };
        let out = d.route_task(st).await;
        assert_eq!(out.record.provider, "local-fallback");
        assert_eq!(out.value.task_type, "calculation");
        assert!(out.record.note.is_some());
    }

    #[tokio::test]
    async fn repeated_identical_decisions_hit_the_cache() {
        let d = decider();
        let st = DecisionState { request: "list files".into(), ..Default::default() };
        let a = d.route_task(st.clone()).await;
        let b = d.route_task(st).await;
        assert!(!a.record.provider.contains("cache"));
        assert!(b.record.provider.contains("cache"));
    }

    #[tokio::test]
    async fn policy_denials_are_never_recovered() {
        let d = decider();
        let st = DecisionState {
            request: "x".into(),
            step_facts: Some(StepFacts { error_kind: "policy".into(), max_attempts: 3, max_replans: 2, ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(d.assess_recovery(st).await.value, RecoveryAction::Fail);
    }
}
