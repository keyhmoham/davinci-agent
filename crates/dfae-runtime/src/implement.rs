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

/// The most common of several candidate argument sets (ties go to the earliest).
fn majority(candidates: Vec<Map<String, Value>>) -> Option<Map<String, Value>> {
    let keyed: Vec<String> = candidates.iter().map(|c| Value::Object(c.clone()).to_string()).collect();
    let best = (0..candidates.len()).max_by_key(|&i| (keyed.iter().filter(|k| **k == keyed[i]).count(), std::cmp::Reverse(i)))?;
    candidates.into_iter().nth(best)
}

/// A model that was told to reply with the file alone still wraps it in a code fence, or adds a sentence in front.
pub(crate) fn strip_fences(text: &str) -> String {
    let t = text.trim();
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        // skip the language tag on the fence line
        let body = after.split_once('\n').map(|(_, b)| b).unwrap_or(after);
        let inner = body.rsplit_once("```").map(|(b, _)| b).unwrap_or(body);
        return inner.trim_matches('\n').to_string();
    }
    // a sentence before a bare HTML document
    let lower = t.to_ascii_lowercase();
    if let Some(at) = lower.find("<!doctype").or_else(|| lower.find("<html"))
        && at > 0
    {
        return t[at..].to_string();
    }
    t.to_string()
}

/// A long text whose lines are mostly copies of each other: a small model stuck in a loop.
pub(crate) fn degenerate(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.len() < 30 {
        return false;
    }
    let unique: std::collections::HashSet<&str> = lines.iter().copied().collect();
    (unique.len() as f64) / (lines.len() as f64) < 0.4
}

/// Keep each line at most twice in a row (and three times in all): what is left of a loop.
pub(crate) fn collapse_repeats(text: &str) -> String {
    let mut seen: std::collections::HashMap<&str, u32> = Default::default();
    let mut out: Vec<&str> = Vec::new();
    for l in text.lines() {
        let key = l.trim();
        if key.is_empty() {
            out.push(l);
            continue;
        }
        let n = seen.entry(key).or_insert(0);
        *n += 1;
        if *n <= 3 && !(out.len() >= 2 && out[out.len() - 1].trim() == key && out[out.len() - 2].trim() == key) {
            out.push(l);
        }
    }
    out.join("\n").trim_end().to_string()
}

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
        temperature: f32,
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
        let goal = ctx.rec.lock().await.goal.clone();
        let step_text = if step.description.split_whitespace().count() < 9 {
            format!("{}\nFULL REQUEST (background only; other steps do the other parts): {}", step.description, tokens::truncate_tokens(&goal, 300))
        } else {
            step.description.clone()
        };
        let user = prompts::bind_user(&sig, &step_text, &ents, &Value::Object(prebound.clone()).to_string(), &inputs, &values, problem, rejected);
        let mut req = self.gen_request(tier, prompts::BIND_SYSTEM, user, "bind");
        req.json = true;
        req.temperature = temperature;
        // a self-hosted server can be told the exact shape: wrong parameter names and types become impossible
        req.schema = Some(meta.args_schema());
        if paid {
            req.max_tokens = req.max_tokens.min(400);
        }
        let v = self.gen_json(ctx, tier, req).await?;
        // some models wrap the arguments: {"args": {...}}
        let v = v.get("args").filter(|a| a.is_object()).cloned().unwrap_or(v);
        v.as_object().cloned().ok_or_else(|| "the reply was not a JSON object of arguments".to_string())
    }

    /// "write {{prev_code}} to index.html", "save that code": the content is something said earlier, used by name — never
    /// written again.
    fn names_an_earlier_value(&self, ctx: &Arc<TaskCtx>, step: &Step) -> bool {
        static EARLIER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"(?i)\b(?:that|this|the|above|previous|earlier|last|same)\s+(?:code|page|html|snippet|script|text|reply|answer|content)\b")
                .unwrap()
        });
        let artifacts = ctx.base_ctx.lock().artifacts.clone();
        !artifacts.is_empty() && (EARLIER.is_match(&step.description) || artifacts.iter().any(|(n, _, _)| step.description.contains(n.as_str())))
    }

    fn step_tier_for_content(&self, step: &Step) -> Tier {
        if (step.strong && self.models.has(Tier::Strong)) || !self.models.has(Tier::Slm) { Tier::Strong } else { Tier::Slm }
    }

    /// The content of the file a step writes, from the request, the step and the earlier results.
    async fn write_content(&self, ctx: &Arc<TaskCtx>, step: &Step, args: &Map<String, Value>, deps: &[Dep], feedback: Option<&str>) -> Option<String> {
        let tier = self.step_tier_for_content(step);
        if !self.models.has(tier) {
            return None;
        }
        let goal = ctx.rec.lock().await.goal.clone();
        let values = ctx.base_ctx.lock().artifacts.iter().map(|(n, _, d)| format!("{{{{{n}}}}} = {d}")).collect::<Vec<_>>().join("; ");
        // The step is the instruction. The whole request is background, and only when the step alone is too thin to go on:
        // with two files in one request a small model writes the wrong one into the wrong file.
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let mut user = format!("FILE TO WRITE (write only this one): {path}\nSTEP: {}\n", step.description);
        if step.description.split_whitespace().count() < 9 {
            user.push_str(&format!("BACKGROUND (the whole request; other files in it are written by other steps): {}\n", tokens::truncate_tokens(&goal, 300)));
        }
        let inputs = Self::inputs_text(deps, if tier == Tier::Strong { 150 } else { 1500 });
        if !inputs.is_empty() {
            user.push_str(&format!("INPUTS:\n{inputs}\n"));
        }
        if !values.is_empty() {
            user.push_str(&format!("AVAILABLE VALUES (they are only for you to read here): {values}\n"));
        }
        if let Some(f) = feedback {
            user.push_str(&format!("YOUR PREVIOUS VERSION WAS REJECTED: {f}\nWrite the whole file again and fix that.\n"));
        }
        let mut req = self.gen_request(tier, prompts::CONTENT_SYSTEM, user.clone(), "content");
        req.temperature = 0.2;
        req.frequency_penalty = 0.4;
        let text = strip_fences(&self.generate_text(ctx, tier, req).await.ok()?);
        if !degenerate(&text) {
            return (!text.trim().is_empty()).then_some(text);
        }
        // a loop ("*.egg-info" a hundred times): once more, hotter and with a heavier penalty, told what went wrong
        let mut req = self.gen_request(
            tier,
            prompts::CONTENT_SYSTEM,
            format!("{user}YOUR PREVIOUS VERSION REPEATED THE SAME LINES. Write the file once, short and complete, with no repetition.\n"),
            "content",
        );
        req.temperature = 0.6;
        req.frequency_penalty = 0.8;
        let again = strip_fences(&self.generate_text(ctx, tier, req).await.ok()?);
        let text = if degenerate(&again) { collapse_repeats(&again) } else { again };
        (!text.trim().is_empty()).then_some(text)
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
        // A file's content is written as plain text by a model, not squeezed into a JSON string (see `CONTENT_SYSTEM`); the
        // rules have already found the path.
        if meta.name == "fs_write"
            && args.get("path").and_then(|p| p.as_str()).is_some_and(|p| !p.is_empty() && !p.contains("{{"))
            && args.get("content").and_then(|c| c.as_str()).is_none_or(|c| c.trim().is_empty())
            && !self.names_an_earlier_value(ctx, step)
            && let Some(text) = self.write_content(ctx, step, &args, deps, feedback).await
        {
            args.insert("content".into(), Value::String(text));
            how.push(if self.step_tier_for_content(step) == Tier::Strong { "strong" } else { "slm" });
        }
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
            for (rung, tier) in ladder.into_iter().enumerate() {
                // A second self-hosted try is a vote: three samples, and the answer most of the valid ones agree on wins.
                // Self-hosted tokens are free, and one wrong sample no longer decides the step.
                let samples = if tier == Tier::Slm && rung == 1 { 3 } else { 1 };
                let temperature = if samples > 1 { 0.5 } else { 0.1 };
                let asks = (0..samples).map(|_| self.ask_for_args(ctx, tier, &meta, step, &entities, &args, deps, problem.as_deref(), rejected, temperature));
                let mut candidates: Vec<Map<String, Value>> = Vec::new();
                for reply in futures_util::future::join_all(asks).await {
                    match reply {
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
                                Ok(()) => candidates.push(merged),
                                Err(e) => problem = Some(e),
                            }
                        }
                        Err(e) => problem = Some(e),
                    }
                }
                if let Some(best) = majority(candidates) {
                    args = best;
                    how.push(if tier == Tier::Strong {
                        "strong"
                    } else if samples > 1 {
                        "slm-vote"
                    } else {
                        "slm"
                    });
                    satisfied = true;
                    break;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn the_answer_most_samples_agree_on_wins_and_ties_go_to_the_first() {
        let a = m(json!({"path": "a.txt"}));
        let b = m(json!({"path": "b.txt"}));
        assert_eq!(majority(vec![a.clone(), b.clone(), b.clone()]), Some(b.clone()));
        assert_eq!(majority(vec![a.clone(), b]), Some(a));
        assert_eq!(majority(vec![]), None);
    }
}

#[cfg(test)]
mod content_tests {
    use super::{collapse_repeats, degenerate, strip_fences};

    #[test]
    fn a_file_comes_out_of_whatever_a_small_model_wraps_around_it() {
        let page = "<!doctype html>\n<html><body><h1>Earth</h1></body></html>";
        assert_eq!(strip_fences(page), page);
        assert_eq!(strip_fences(&format!("```html\n{page}\n```")), page);
        assert_eq!(strip_fences(&format!("Sure! Here is the page:\n\n```html\n{page}\n```\nLet me know if you want changes.")), page);
        assert_eq!(strip_fences(&format!("Here is the page you asked for:\n{page}")), page);
        assert_eq!(strip_fences("```\nport = 1\n```"), "port = 1");
        assert_eq!(strip_fences("plain text file"), "plain text file");
    }

    #[test]
    fn a_loop_is_recognised_and_cut_down() {
        let looped = format!("# ignore\n*.pyc\n{}", "*.egg-info\n".repeat(200));
        assert!(degenerate(&looped));
        assert!(!degenerate("a\nb\nc"));
        let ok: String = (0..60).map(|i| format!("line {i}\n")).collect();
        assert!(!degenerate(&ok));
        let cut = collapse_repeats(&looped);
        assert_eq!(cut.matches("*.egg-info").count(), 2);
        assert!(cut.starts_with("# ignore\n*.pyc"));
    }
}
