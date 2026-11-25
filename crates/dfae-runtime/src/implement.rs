//! The acting phase: turning a planner's intent into an action, step by step, right before the step runs.
//!
//! A brief plan (written by the external model) says *what* each step should achieve. This module decides
//! *how*, using the self-hosted triple under the program's control — deterministic first, models only where
//! the rules run out, and always checked against the tool's schema and the policy before anything executes:
//!
//! 1. **GLiNER** (with the rules) reads the step's sentence for paths, names, versions, quoted text;
//! 2. **rules** bind arguments straight from the sentence (the tools' own patterns), then from the entities
//!    the tool's parameters ask for;
//! 3. the **decision model** judges which tool a step needs when the planner left it open, and later whether
//!    the result met the planner's expectation;
//! 4. the **SLM** fills in what is still missing (reading the results of earlier steps), with the validation
//!    error fed back if it gets it wrong;
//! 5. only if the SLM cannot, the **external model** is asked for the arguments, in a lean prompt.
//!
//! A step that none of this can carry out fails with a precise message, and recovery (retry, repair, a
//! re-plan by the external model) takes over.

use crate::economy::repair;
use crate::engine::{Inner, TaskCtx};
use crate::executor::StepFailure;
use crate::{planner, prompts};
use dfae_core::*;
use dfae_extract::labels_for;
use dfae_models::Tier;
use dfae_tools::bind_direct;
use serde_json::{Map, Value};
use std::sync::Arc;

type Dep = (String, Option<String>, String, String);

fn object(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// Merge `extra` into `base` for keys `base` does not have a usable value for.
fn fill(base: &mut Map<String, Value>, extra: &Map<String, Value>) {
    for (k, v) in extra {
        if base.get(k).map(|b| b.is_null() || b == &Value::String(String::new())).unwrap_or(true) {
            base.insert(k.clone(), v.clone());
        }
    }
}

impl Inner {
    /// Entities in a step's sentence (GLiNER + rules), merged with what the task already knows.
    async fn step_entities(&self, ctx: &Arc<TaskCtx>, text: &str) -> Vec<Entity> {
        let (task_type, mut known) = {
            let r = ctx.rec.lock().await;
            (r.route.as_ref().map(|x| x.task_type.clone()), r.entities.clone())
        };
        let labels = labels_for(task_type.as_deref());
        if let Ok(res) = self.extractor.extract(ExtractionRequest { text: text.to_string(), labels, threshold: 0.4 }).await {
            if res.backend.contains("gliner") {
                self.add_ledger(
                    ctx,
                    LedgerEntry {
                        ts: chrono::Utc::now(),
                        component: Component::Gliner,
                        model: res.backend.clone(),
                        purpose: "extract_step".into(),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        cached_tokens: 0,
                        latency_ms: res.latency_ms,
                        cached: res.cached,
                        origin: None,
                    },
                )
                .await;
            }
            // the step's own entities come first: they are the most specific
            let mut out = res.entities;
            for e in known.drain(..) {
                if !out.iter().any(|x| x.label == e.label && x.text == e.text) {
                    out.push(e);
                }
            }
            return out;
        }
        known
    }

    fn inputs_text(deps: &[Dep], per_dep_tokens: usize) -> String {
        deps.iter()
            .map(|(id, tool, summary, _)| {
                dfae_core::safety::wrap_untrusted(
                    &format!("{id} {}", tool.as_deref().unwrap_or("reasoning")),
                    &tokens::truncate_tokens(summary, per_dep_tokens),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Ask a model for the arguments of one tool call. The reply is merged, repaired and validated by the caller.
    #[allow(clippy::too_many_arguments)]
    async fn ask_for_args(
        &self,
        ctx: &Arc<TaskCtx>,
        tier: Tier,
        meta: &ToolMetadata,
        step: &Step,
        entities: &[Entity],
        prebound: &Map<String, Value>,
        deps: &[Dep],
        problem: Option<&str>,
        rejected: bool,
    ) -> Result<Map<String, Value>, String> {
        let paid = tier == Tier::Strong;
        let ents = entities
            .iter()
            .filter(|e| e.label != "action")
            .take(24)
            .map(|e| format!("{}={}", e.label.replace(' ', "_"), e.text))
            .collect::<Vec<_>>()
            .join(" | ");
        let sig = if paid { meta.signature() } else { meta.signature_detailed() };
        let inputs = Self::inputs_text(deps, if paid { 120 } else { 900 });
        let values = ctx.base_ctx.lock().artifacts.iter().map(|(n, _, d)| format!("{{{{{n}}}}} = {d}")).collect::<Vec<_>>().join("; ");
        let user = prompts::bind_user(&sig, &step.description, &ents, &Value::Object(prebound.clone()).to_string(), &inputs, &values, problem, rejected);
        let mut req = self.gen_request(tier, prompts::BIND_SYSTEM, user, "bind");
        req.json = true;
        if paid {
            req.max_tokens = req.max_tokens.min(400);
        }
        let v = self.gen_json(ctx, tier, req).await?;
        // some models wrap the arguments: {"args": {...}}
        let v = v.get("args").filter(|a| a.is_object()).cloned().unwrap_or(v);
        v.as_object().cloned().ok_or_else(|| "the reply was not a JSON object of arguments".to_string())
    }

    /// Decide what a brief step becomes and bind its arguments. Returns the concrete step.
    ///
    /// `feedback` is set when a previous implementation of this step ran and its result was unusable (the file it wrote
    /// does not parse): the generated parts are produced again by a model, told what was wrong.
    pub(crate) async fn implement_step(self: &Arc<Self>, ctx: &Arc<TaskCtx>, step: &Step, deps: &[Dep], feedback: Option<&str>) -> Result<Step, StepFailure> {
        let fail = |kind: ErrorKind, m: String| StepFailure { kind, message: m };
        let allowed = ctx.allowed.lock().clone();
        let scoped: Vec<ToolMetadata> = self.scoped_metas(ctx).into_iter().filter(|m| allowed.contains(&m.name)).collect();
        let entities = self.step_entities(ctx, &step.description).await;

        // ---- what kind of action? -------------------------------------------------
        let mut tool = step.tool.clone().filter(|t| scoped.iter().any(|m| &m.name == t));
        let mut how: Vec<&str> = Vec::new();
        let mut direct_args: Option<Map<String, Value>> = None;
        if tool.is_none() {
            // the planner left it to thinking; a request that binds to a tool outright is still just a tool call
            if let Some(calls) = bind_direct(&scoped, &step.description, &entities)
                && calls.len() == 1
            {
                tool = Some(calls[0].tool.clone());
                direct_args = calls[0].args.as_object().cloned();
                how.push("rules");
            }
        }
        let mut done = step.clone();
        done.brief = false;
        let Some(tool) = tool else {
            // reasoning: the SLM writes it, unless the planner wants the external model to
            done.kind = StepKind::Reasoning;
            done.tool = None;
            done.how = Some("reasoning".into());
            self.emit(ctx, EventKind::StepImplemented { step_id: step.id.clone(), how: "reasoning".into(), tool: None, args: String::new() }).await;
            return Ok(done);
        };
        let meta = scoped.iter().find(|m| m.name == tool).cloned().ok_or_else(|| fail(ErrorKind::NotFound, format!("tool '{tool}' is not available")))?;

        // ---- arguments: the planner's, then rules, then entities --------------------
        let mut args = object(&step.args);
        if feedback.is_some() {
            for k in ["content", "new_string", "text", "body"] {
                args.remove(k);
            }
        }
        let planned = !args.is_empty();
        if let Some(d) = &direct_args {
            fill(&mut args, d);
        }
        if meta.validate_args(&Value::Object(args.clone())).is_err()
            && let Some(calls) = bind_direct(std::slice::from_ref(&meta), &step.description, &entities)
            && let Some(c) = calls.iter().find(|c| c.tool == tool)
        {
            fill(&mut args, &object(&c.args));
            if !how.contains(&"rules") {
                how.push("rules");
            }
        }
        if meta.validate_args(&Value::Object(args.clone())).is_err() {
            if let Some(f) = repair::fix_args(&meta, &Value::Object(args.clone()), &step.description, &entities) {
                args = object(&f.args);
                if !how.contains(&"rules") {
                    how.push("rules");
                }
            } else {
                // not complete yet, but what the entities do supply is taken before any model is asked
                let (partial, added) = repair::fill_partial(&meta, &Value::Object(args.clone()), &step.description, &entities);
                if added {
                    args = object(&partial);
                    if !how.contains(&"rules") {
                        how.push("rules");
                    }
                }
            }
        }
        let valid = |a: &Map<String, Value>| -> Result<(), String> {
            meta.validate_args(&Value::Object(a.clone()))?;
            let bad = planner::malformed_placeholders(&Value::Object(a.clone()));
            if !bad.is_empty() {
                return Err(format!(
                    "{} is not a value: write the real text, or {{{{<step id>}}}} / {{{{prev_code}}}} to use an earlier result",
                    bad.join(", ")
                ));
            }
            // a `{{id}}` template may only name a step this one depends on
            for r in planner::template_refs(&Value::Object(a.clone())) {
                if !step.depends_on.contains(&r) && !crate::session::ARTIFACT_NAMES.contains(&r.as_str()) {
                    return Err(format!("{{{{{r}}}}} is not one of this step's inputs ({})", step.depends_on.join(", ")));
                }
            }
            Ok(())
        };
        if valid(&args).is_ok() {
            if how.is_empty() {
                how.push(if planned { "plan" } else { "rules" });
            }
        } else {
            // ---- the models: the SLM first, the external model only if the SLM cannot ----
            let mut problem = feedback.map(str::to_string).or_else(|| valid(&args).err());
            let mut ladder: Vec<Tier> = Vec::new();
            if self.models.has(Tier::Slm) {
                ladder.extend([Tier::Slm, Tier::Slm]);
            }
            if self.models.has(Tier::Strong) {
                ladder.push(Tier::Strong);
            }
            if ladder.is_empty() {
                return Err(fail(
                    ErrorKind::InvalidArgs,
                    format!("cannot determine the arguments of {tool}: {} (no language model is configured to help)", problem.unwrap_or_default()),
                ));
            }
            let mut satisfied = false;
            let mut rejected = feedback.is_some(); // the first ask says what is missing; later ones say what was wrong with the answer
            for tier in ladder {
                match self.ask_for_args(ctx, tier, &meta, step, &entities, &args, deps, problem.as_deref(), rejected).await {
                    Ok(got) => {
                        let mut merged = args.clone();
                        // the model's values replace rule guesses only where the rules had nothing valid
                        for (k, v) in got {
                            merged.entry(k).or_insert(v);
                        }
                        if let Some(f) = repair::fix_args(&meta, &Value::Object(merged.clone()), &step.description, &entities) {
                            merged = object(&f.args);
                        }
                        match valid(&merged) {
                            Ok(()) => {
                                args = merged;
                                how.push(if tier == Tier::Strong { "strong" } else { "slm" });
                                satisfied = true;
                                break;
                            }
                            Err(e) => problem = Some(e),
                        }
                    }
                    Err(e) => problem = Some(e),
                }
                rejected = true;
            }
            if !satisfied {
                return Err(fail(
                    ErrorKind::InvalidArgs,
                    format!(
                        "cannot determine the arguments of {tool} for \"{}\": {}",
                        tokens::truncate_tokens(&step.description, 20),
                        problem.unwrap_or_default()
                    ),
                ));
            }
        }

        done.kind = StepKind::Tool;
        done.tool = Some(tool.clone());
        done.args = Value::Object(args.clone());
        done.approval_required = meta.requires_approval;
        let how = how.join("+");
        done.how = Some(how.clone());
        self.emit(
            ctx,
            EventKind::StepImplemented { step_id: step.id.clone(), how, tool: Some(tool), args: tokens::truncate_tokens(&Value::Object(args).to_string(), 40) },
        )
        .await;
        Ok(done)
    }

    /// A planner says "index.html exists" for a step that wrote `site/index.html`: the file it named exists under the
    /// path the step used, so the expectation holds.
    fn satisfied_by_arguments(&self, c: &Check, step: &Step) -> bool {
        let Check::FileExists { path } = c else { return false };
        let want = path.trim_start_matches("./").trim_end_matches('/');
        let mut texts = Vec::new();
        if let Some(m) = step.args.as_object() {
            texts.extend(m.values().filter_map(|v| v.as_str()));
        }
        texts.iter().any(|t| {
            let t = t.trim_start_matches("./").trim_end_matches('/');
            (t.ends_with(&format!("/{want}")) || want.ends_with(&format!("/{t}")) || t == want)
                && self.policy.guard().read_path(t).map(|p| p.exists()).unwrap_or(false)
        })
    }

    /// Did a step's result meet what the planner expected of it? `Some(reason)` if the decision model says no.
    pub(crate) async fn judge_step(&self, ctx: &Arc<TaskCtx>, step: &Step, output: &str) -> Option<String> {
        let expect = step.expect.as_deref()?;
        // what code can check, code checks: facts about files and output need no model, and a model must not overrule them
        let (checks, understood) = crate::planner::expect_checks(expect);
        let mut missed: Vec<String> = Vec::new();
        for c in &checks {
            let (pass, ev) = self.eval_check(c, output);
            if !pass && !self.satisfied_by_arguments(c, step) {
                missed.push(format!("{} ({ev})", c.describe()));
            }
        }
        if !missed.is_empty() {
            return Some(format!("not met: {}", missed.join("; ")));
        }
        if understood {
            return None;
        }
        let entities = ctx.rec.lock().await.entities.clone();
        let mut st = self.decision_state(&format!("STEP: {}\nEXPECTATION: {expect}", step.description), &entities);
        st.completion_facts = Some(CompletionFacts {
            steps_ok: 1,
            steps_failed: 0,
            steps_total: 1,
            checks_passed: 0,
            checks_failed: 0,
            evidence: tokens::truncate_tokens(output, 600),
        });
        let p = self.note(ctx, self.decider.judge_step(st).await).await;
        (p < 0.35).then(|| format!("the result does not meet the expectation \"{expect}\" (decision model: {p:.2})"))
    }
}
