//! The acting phase: turning a planner's intent into an action, step by step, right before the step runs.
//!
//! A brief plan (written by the planner model) says *what* each step should achieve. This module decides
//! *how*, using the self-hosted triple under the program's control — deterministic first, models only where
//! the rules run out, and always checked against the tool's schema and the policy before anything executes:
//!
//! 1. **GLiNER** (with the rules) reads the step's sentence for paths, names, versions, quoted text;
//! 2. **rules** bind arguments straight from the sentence (the tools' own patterns), then from the entities
//!    the tool's parameters ask for;
//! 3. the **decision model** judges which tool a step needs when the planner left it open, and later whether
//!    the result met the planner's expectation;
//! 4. the **worker model** fills in what is still missing (reading the results of earlier steps), with the validation
//!    error fed back if it gets it wrong;
//! 5. only if the worker model cannot, the **planner model** is asked for the arguments, in a lean prompt.
//!
//! A step that none of this can carry out fails with a precise message, and recovery (retry, repair, a
//! re-plan by the planner model) takes over.

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
    // Only a reply that IS one fenced block (optionally after a one-line "here is the file:" and before a one-line sign-off) is
    // unwrapped. Fences inside the content — a README's code samples, a doc's examples — are the content, and are kept.
    let lines: Vec<&str> = t.lines().collect();
    let fences: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.trim_start().starts_with("```")).map(|(i, _)| i).collect();
    if let (Some(&first), Some(&last)) = (fences.first(), fences.last())
        && last > first
    {
        let before: Vec<&str> = lines[..first].iter().copied().filter(|l| !l.trim().is_empty()).collect();
        let after: Vec<&str> = lines[last + 1..].iter().copied().filter(|l| !l.trim().is_empty()).collect();
        let is_preamble = |l: &str| {
            let l = l.trim();
            let w = l.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
            !l.starts_with('#') && l.len() <= 160 && (l.ends_with(':') || matches!(w.as_str(), "here" | "sure" | "certainly" | "below" | "okay" | "ok"))
        };
        let wrapped_start = before.is_empty() || (before.len() <= 2 && before.iter().all(|l| is_preamble(l)));
        let wrapped_end = after.is_empty() || (after.len() <= 2 && after.iter().all(|l| l.len() <= 160 && !l.trim_start().starts_with('#')));
        if wrapped_start && wrapped_end {
            return lines[first + 1..last].join("\n").trim_matches('\n').to_string();
        }
        return t.to_string();
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

/// Should this shell command run in the background? The step's sentence says so, or the command is a server (`http.server`, `uvicorn`,
/// `npm start`, `flask run`…), which would otherwise hold the step until its timeout.
pub(crate) fn implies_background(description: &str, command: &str) -> bool {
    static SENTENCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(?:in the background|background(?:ed)?|detached|as a daemon|keep(?:s|ing)? (?:it )?running|leave (?:it )?running|and serve)\b",
        )
        .unwrap()
    });
    static SERVER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:http\.server|SimpleHTTPServer|uvicorn|gunicorn|flask\s+run|php\s+-S|npm\s+(?:run\s+)?(?:start|dev|serve)|yarn\s+(?:start|dev)|vite(?:\s|$)|next\s+dev|live-server|http-server|python3?\s+-m\s+(?:http\.server|flask|uvicorn))\b").unwrap()
    });
    // `pip install fastapi uvicorn pytest` names a server as a package to install, it does not start one
    static INSTALLER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)^\s*(?:sudo\s+)?(?:(?:python3?\s+-m\s+)?pip3?|pipx|uv(?:\s+pip)?|poetry|npm|yarn|pnpm|bun|apt(?:-get)?|dfae-apt|cargo|go|gem|brew)\s+(?:install|add|i|get|ci)\b").unwrap()
    });
    SENTENCE.is_match(description)
        || command.split(['&', ';', '|']).filter(|seg| !seg.trim().is_empty() && !INSTALLER.is_match(seg)).any(|seg| SERVER.is_match(seg))
}

/// Parameters whose values are content (text to write): the one thing allowed to run long.
const CONTENT_PARAMS: &[&str] = &["content", "new_string", "body", "text"];
/// Parameters that hold a command line, which may carry a short script.
const COMMAND_PARAMS: &[&str] = &["command", "script"];

/// Is this step writing something that is itself the product — a page, code, a document, data — rather than doing ordinary work?
/// Only then may the worker model write at length.
pub(crate) fn content_like(description: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:write|compose|draft|generate|create|produce|implement|build|code|script|essay|article|report|document|page|html|css|function|class|module|readme|template|stylesheet|dataset|csv|json|yaml|poem|story|email|letter)\b").unwrap()
    });
    RE.is_match(description)
}

/// The tool as the binding model sees it: its required parameters, the optional text ones, and the optional numbers and flags the
/// step names; the rest is left to the tool's defaults. (Digits in the step are no reason: a port is not a timeout.)
fn bind_view(meta: &ToolMetadata, step_text: &str) -> ToolMetadata {
    let text = step_text.to_lowercase();
    let mut m = meta.clone();
    m.params.retain(|p| {
        p.required
            || text.contains(&p.name.to_lowercase())
            // a word of the name ("timeout" of `timeout_secs`, "lines" of `max_lines`) is the step asking for it
            || p.name.split('_').any(|w| w.len() >= 4 && text.contains(&w.to_lowercase()))
            || p.ty == ParamType::String
    });
    m
}

/// `command` without its leading `cd <cwd> &&` (or `;`), when that is where it already runs.
pub(crate) fn strip_redundant_cd(command: &str, cwd: &str) -> Option<String> {
    let norm = |d: &str| d.trim().trim_matches(['"', '\'', '`']).trim_start_matches("./").trim_end_matches('/').to_string();
    let rest = command.trim_start().strip_prefix("cd ")?;
    let (dir, tail) = rest.split_once("&&").or_else(|| rest.split_once(';'))?;
    (norm(dir) == norm(cwd) && !norm(cwd).is_empty() && !tail.trim().is_empty()).then(|| tail.trim().to_string())
}

/// The directory of the workspace a shell step says it runs in ("…in proj", "inside the folder site", "cwd app"), unless the command
/// already goes there itself (`cd`, `-C`, a path through it).
pub(crate) fn directory_named(step: &str, command: &str, workspace: &std::path::Path) -> Option<String> {
    static IN_DIR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)\b(?:in|inside|within|under|into|from|cwd|directory|folder|dir)\s+(?:the\s+|a\s+|an\s+)?(?:(?:directory|folder|dir|repo|repository|project)\s+)?[`"']?([A-Za-z0-9_][A-Za-z0-9_./-]*)[`"']?"#).unwrap()
    });
    if command.trim_start().starts_with("cd ") || command.contains(" cd ") || command.contains("&& cd") {
        return None;
    }
    IN_DIR
        .captures_iter(step)
        .map(|c| c[1].trim_end_matches(['.', '/']).to_string())
        .find(|d| !d.is_empty() && !d.starts_with("..") && workspace.join(d).is_dir() && !command.contains(d.as_str()))
}

/// Some models end a reply with their own tool-call syntax (`</arg_value><arg_key>path</arg_key>…</tool_call>`): it is never part
/// of a file or an argument.
pub(crate) fn strip_tool_markup(text: &str) -> String {
    const MARKS: [&str; 5] = ["</arg_value>", "<arg_key>", "<tool_call>", "</tool_call>", "<arg_value>"];
    let cut = MARKS.iter().filter_map(|m| text.find(m)).min().unwrap_or(text.len());
    text[..cut].trim_end().to_string()
}

/// A long text stuck in a loop: one substantial line over and over (`*.egg-info` a hundred times), or a text that is mostly copies of
/// its own lines. Structural lines every real page and program repeats (`}`, `</div>`, `end`) are not evidence: only lines of 12
/// characters or more count (8 for a run of identical lines).
/// The names a step gives (a product, a person, a place, a quoted title) that a prose or page file about them must mention: a README
/// for "a tool called Brewster" that never says Brewster was written about something else, or is an empty shell. Only for prose and
/// pages; code and config files name things in their own ways.
pub(crate) fn missing_names(step: &str, path: &str, text: &str) -> Vec<String> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(ext.as_str(), "md" | "markdown" | "html" | "htm" | "txt" | "rst") || path.ends_with(".gitignore") {
        return vec![];
    }
    const COMMON: &[&str] = &[
        "README",
        "Markdown",
        "HTML",
        "CSS",
        "JSON",
        "YAML",
        "TOML",
        "URL",
        "API",
        "UI",
        "UX",
        "SEO",
        "FAQ",
        "I",
        "A",
        "An",
        "The",
        "It",
        "Write",
        "Create",
        "Add",
        "Make",
        "Use",
        "Include",
        "Python",
        "JavaScript",
        "TypeScript",
        "Rust",
        "Go",
        "Node",
        "React",
        "Next",
        "Tailwind",
    ];
    let lower = text.to_lowercase();
    let mut names: Vec<String> = Vec::new();
    // quoted titles
    for part in step.split('"').skip(1).step_by(2) {
        let p = part.trim();
        if !p.is_empty() && p.len() <= 60 && !p.contains('/') && !p.contains('.') {
            names.push(p.to_string());
        }
    }
    // proper nouns: capitalised words that do not start a sentence
    let unquoted: String = step.split('"').step_by(2).collect::<Vec<_>>().join(" ");
    let mut sentence_start = true;
    for raw in unquoted.split_whitespace() {
        let w = raw.trim_matches(|c: char| !c.is_alphanumeric());
        let starts = sentence_start;
        sentence_start = raw.ends_with(['.', ':', '!', '?']);
        if starts || w.len() < 3 || w.contains(['/', '.', '_']) || COMMON.contains(&w) {
            continue;
        }
        if w.chars().next().is_some_and(|c| c.is_uppercase()) && w.chars().skip(1).any(|c| c.is_lowercase()) && !names.iter().any(|n| n == w) {
            names.push(w.to_string());
        }
    }
    names.into_iter().filter(|n| !lower.contains(&n.to_lowercase())).take(3).collect()
}

pub(crate) fn degenerate(text: &str) -> bool {
    // the same line (of 8 characters or more) forty times in a row: a starfield of 30 `<div class="star"></div>` is a page, a
    // loop goes on until the token limit
    let near: Vec<&str> = text.lines().map(str::trim).filter(|l| l.chars().count() >= 8).collect();
    let mut run = 1;
    for w in near.windows(2) {
        run = if w[0] == w[1] { run + 1 } else { 1 };
        if run >= 40 {
            return true;
        }
    }
    let lines: Vec<&str> = near.into_iter().filter(|l| l.chars().count() >= 12).collect();
    if lines.len() < 40 {
        return false;
    }
    let unique: std::collections::HashSet<&str> = lines.iter().copied().collect();
    (unique.len() as f64) / (lines.len() as f64) < 0.25
}

/// Keep each line at most twice in a row (and a long line three times in all): what is left of a loop. Short structural lines
/// (`</div>`, `}`) are legitimately everywhere and are only collapsed when they run on.
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
        if (*n <= 3 || key.chars().count() < 20) && !(out.len() >= 2 && out[out.len() - 1].trim() == key && out[out.len() - 2].trim() == key) {
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
                        prefixed: false,
                        latency_ms: res.latency_ms,
                        cached: res.cached,
                        origin: None,
                        reasoning_tokens: 0,
                        cache_write_tokens: 0,
                        cost_usd: None,
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
        let paid = tier == Tier::Planner;
        let ents = entities
            .iter()
            .filter(|e| e.label != "action")
            .take(24)
            .map(|e| format!("{}={}", e.label.replace(' ', "_"), e.text))
            .collect::<Vec<_>>()
            .join(" | ");
        // The model is asked for what the step needs, not for every knob of the tool: an optional number nobody mentioned
        // (`timeout_secs`) is a number a small model can run away with ("1200120120…" until the token limit).
        let mut asked = bind_view(meta, &step.description);
        // content a model already wrote (as plain text, in full) is never asked for again as a JSON string: that is the most expensive
        // thing a retry can do, and what is wrong with the arguments is not it
        asked
            .params
            .retain(|p| !(CONTENT_PARAMS.contains(&p.name.as_str()) && prebound.get(&p.name).and_then(|v| v.as_str()).is_some_and(|c| !c.trim().is_empty())));
        let meta = &asked;
        // both hosted models are billed by the token: compact signature (the parameter help rides in the schema), trimmed inputs
        let sig = meta.signature();
        let mut inputs = Self::inputs_text(deps, if paid { 120 } else { 400 });
        // an edit replaces text that exists: a model that is not shown the file invents it (or leaves it empty)
        if meta.name == "fs_edit"
            && let Some(file) = self.file_to_edit(prebound, entities, &step.description)
        {
            inputs.push_str(&format!("\n{}", dfae_core::safety::wrap_untrusted("FILE (copy old_string from it exactly)", &file)));
        }
        let values = ctx.base_ctx.lock().artifacts.iter().map(|(n, _, d)| format!("{{{{{n}}}}} = {d}")).collect::<Vec<_>>().join("; ");
        let goal = ctx.rec.lock().await.goal.clone();
        let step_text = if step.description.split_whitespace().count() < 9 {
            // a short step ("so what is its price ??") leans on the conversation: the last turns say what "its" is
            let recent: Vec<String> = {
                let turns = ctx.base_ctx.lock().turns.clone();
                turns
                    .iter()
                    .skip(turns.len().saturating_sub(2))
                    .map(|(q, a)| format!("user: {} → assistant: {}", tokens::truncate_tokens(q, 40), tokens::truncate_tokens(a, 60)))
                    .collect()
            };
            let recent = if recent.is_empty() { String::new() } else { format!("\nRECENT TURNS: {}", recent.join(" | ")) };
            format!("{}\nFULL REQUEST (background only; other steps do the other parts): {}{recent}", step.description, tokens::truncate_tokens(&goal, 300))
        } else {
            step.description.clone()
        };
        let user = prompts::bind_user(&sig, &step_text, &ents, &Value::Object(prebound.clone()).to_string(), &inputs, &values, problem, rejected);
        // what binds a shell command is told what the shell is, and that it may install (other tools need no such words)
        let system = match meta.name.as_str() {
            "shell" => format!("{}\n{}", prompts::BIND_SYSTEM, self.shell_environment(true)),
            "fs_edit" => format!(
                "{}\nold_string is copied exactly from the FILE (indentation included) and long enough to occur once; new_string replaces it.",
                prompts::BIND_SYSTEM
            ),
            _ => prompts::BIND_SYSTEM.to_string(),
        };
        let mut req = self.gen_request(tier, &system, user, "bind");
        req.json = true;
        req.temperature = temperature;
        // a self-hosted server can be told the exact shape: wrong parameter names and types become impossible
        req.schema = Some(meta.args_schema());
        // Choosing a path or a pattern is a few dozen tokens and is held to a small limit. A tool whose arguments *are* content — a
        // file's text, a shell command that may carry a script — is not: cutting that off mid-string is a broken JSON object.
        let has = |names: &[&str]| meta.params.iter().any(|p| names.contains(&p.name.as_str()));
        if has(CONTENT_PARAMS) {
            req.max_tokens = req.max_tokens.max(self.cfg.worker.content_max_tokens);
        } else if has(COMMAND_PARAMS) {
            req.max_tokens = req.max_tokens.max(600); // a command line: not a document, and a runaway must be cheap
        } else if paid {
            req.max_tokens = req.max_tokens.min(400);
        }
        let v = self.gen_json(ctx, tier, req).await?;
        // some models wrap the arguments: {"args": {...}}
        let v = v.get("args").filter(|a| a.is_object()).cloned().unwrap_or(v);
        let mut args = v.as_object().cloned().ok_or_else(|| "the reply was not a JSON object of arguments".to_string())?;
        for value in args.values_mut() {
            if let Value::String(t) = value {
                *t = strip_tool_markup(t);
            }
        }
        Ok(args)
    }

    /// The files this task has written so far (the last three, from the same project), as text for a prompt: what a file written now must
    /// agree with. Each is cut short; only files, never what is outside the workspace.
    async fn related_files(&self, ctx: &Arc<TaskCtx>, this_step: &str, target: &str) -> String {
        let paths: Vec<String> = {
            let r = ctx.rec.lock().await;
            let Some(plan) = r.plan.as_ref() else { return String::new() };
            plan.steps
                .iter()
                .filter(|s| s.id != this_step && s.status == StepStatus::Succeeded && s.tool.as_deref() == Some("fs_write"))
                .filter_map(|s| s.args.get("path").and_then(|p| p.as_str()).map(String::from))
                .filter(|p| p != target)
                .collect()
        };
        let project = |p: &str| p.split('/').next().filter(|_| p.contains('/')).unwrap_or("").to_string();
        let mine = project(target);
        let mut shown: Vec<String> = Vec::new();
        for p in paths.iter().rev().filter(|p| project(p) == mine).take(3) {
            if let Ok(full) = self.policy.guard().read_path(p)
                && let Ok(text) = std::fs::read_to_string(full)
                && !text.is_empty()
            {
                let cut: String = text.chars().take(3500).collect();
                shown.push(dfae_core::safety::wrap_untrusted(
                    &format!("file {p}"),
                    &if cut.len() < text.len() { format!("{cut}\n… [file continues]") } else { cut },
                ));
            }
        }
        if shown.is_empty() { String::new() } else { format!("RELATED FILES:\n{}", shown.join("\n")) }
    }

    /// The text of the file an edit step is about (as far as the model needs to see it), found from the arguments or the step.
    fn file_to_edit(&self, prebound: &Map<String, Value>, entities: &[Entity], step_text: &str) -> Option<String> {
        let candidates = prebound
            .get("path")
            .and_then(|p| p.as_str())
            .map(String::from)
            .into_iter()
            .chain(entities.iter().filter(|e| e.label == "file path").map(|e| e.text.clone()))
            .chain(
                step_text
                    .split_whitespace()
                    .map(|w| w.trim_matches(['`', '"', '\'', ',', '.', ':', ';', '(', ')']).to_string())
                    .filter(|w| w.contains('/') || w.contains('.')),
            );
        for c in candidates {
            if let Ok(p) = self.policy.guard().read_path(&c)
                && p.is_file()
                && let Ok(text) = std::fs::read_to_string(&p)
            {
                let shown: String = text.chars().take(8000).collect();
                return Some(if shown.len() < text.len() { format!("{shown}\n… [file continues]") } else { shown });
            }
        }
        None
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
        if (step.planner && self.models.has(Tier::Planner)) || !self.models.has(Tier::Worker) { Tier::Planner } else { Tier::Worker }
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
        let inputs = Self::inputs_text(deps, if tier == Tier::Planner { 150 } else { 1500 });
        if !inputs.is_empty() {
            user.push_str(&format!("INPUTS:\n{inputs}\n"));
        }
        // a file that exists is being replaced: what it holds may be what the framework needs (a stylesheet's `@import "tailwindcss"`, a
        // config's plugins), and a rewrite from scratch loses it
        if let Ok(existing) = self.policy.guard().read_path(path)
            && existing.is_file()
            && let Ok(old) = std::fs::read_to_string(&existing)
            && !old.trim().is_empty()
        {
            let shown: String = old.chars().take(3000).collect();
            user.push_str(&format!(
                "{}\nThis file already exists and is being replaced: keep what the project's tooling needs from it (imports, directives, configuration, exports the other files use).\n",
                dfae_core::safety::wrap_untrusted(&format!("CURRENT {path}"), &shown)
            ));
        }
        // a file that belongs to a project is written knowing the others (the tests of a module see the module; a page sees its styles)
        let related = self.related_files(ctx, &step.id, path).await;
        if !related.is_empty() {
            user.push_str(&format!("{related}\nThis file must fit with the files above (names, imports, paths, interfaces).\n"));
        }
        if !values.is_empty() {
            user.push_str(&format!("AVAILABLE VALUES (they are only for you to read here): {values}\n"));
        }
        if let Some(f) = feedback {
            user.push_str(&format!("YOUR PREVIOUS VERSION WAS REJECTED: {f}\nWrite the whole file again and fix that.\n"));
        }
        let mut req = self.gen_request(tier, prompts::CONTENT_SYSTEM, user.clone(), "content");
        // No frequency penalty: markup and code repeat their own tokens (`</div>`, `class`) by nature, and a penalty on them makes a
        // long page decay into word salad. Loops are caught afterwards by `degenerate`.
        req.temperature = 0.2;
        let text = strip_tool_markup(&strip_fences(&self.generate_text(ctx, tier, req).await.ok()?));
        // looping, or cut off by the limit / never finished (an unclosed tag, a half JSON object)
        let missing = missing_names(&step.description, path, &text);
        let why = if degenerate(&text) {
            Some("it repeated the same lines".to_string())
        } else if !missing.is_empty() {
            Some(format!("it does not mention {}, which the step names: the file is about that", missing.join(", ")))
        } else {
            crate::syntax::problem(&self.policy.workspace().join(path), &text).await
        };
        let Some(why) = why else {
            return (!text.trim().is_empty()).then_some(text);
        };
        self.emit(ctx, EventKind::Warning { message: format!("{path}: the first version was rejected ({why}); it is written again") }).await;
        // once more, hotter and with a heavier penalty, told what went wrong and to keep it short
        let mut req = self.gen_request(
            tier,
            prompts::CONTENT_SYSTEM,
            format!("{user}YOUR PREVIOUS VERSION WAS REJECTED: {why}. Write the whole file again, complete, with its styles inside it and no external files, and with no repetition.\n"),
            "content",
        );
        req.temperature = 0.4;
        let again = strip_tool_markup(&strip_fences(&self.generate_text(ctx, tier, req).await.ok()?));
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
            // reasoning: the worker model writes it, unless the planner wants the planner model to
            done.kind = StepKind::Reasoning;
            done.tool = None;
            done.how = Some("reasoning".into());
            self.emit(ctx, EventKind::StepImplemented { step_id: step.id.clone(), how: "reasoning".into(), tool: None, args: String::new() }).await;
            return Ok(done);
        };
        let meta = scoped.iter().find(|m| m.name == tool).cloned().ok_or_else(|| fail(ErrorKind::NotFound, format!("tool '{tool}' is not available")))?;

        // A planner's sentence about a file describes it ("a landing page with `Earth` in the title"): a quoted word in it is not the file's
        // content. Content comes from a model that writes it, unless the sentence is a short, literal "write 'x' to y".
        let describes_content = meta.name == "fs_write" && step.description.split_whitespace().count() >= 12;
        let entities_for_args: Vec<Entity> =
            if describes_content { entities.iter().filter(|e| e.label != "quoted").cloned().collect() } else { entities.clone() };
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
            if let Some(f) = repair::fix_args(&meta, &Value::Object(args.clone()), &step.description, &entities_for_args) {
                args = object(&f.args);
                if !how.contains(&"rules") {
                    how.push("rules");
                }
            } else {
                // not complete yet, but what the entities do supply is taken before any model is asked
                let (partial, added) = repair::fill_partial(&meta, &Value::Object(args.clone()), &step.description, &entities_for_args);
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
            // what a step prints is never a path: a small model told it may use {{s1}} puts it everywhere, a file name included
            for k in ["path", "cwd", "from", "to", "dir", "directory"] {
                if a.get(k).and_then(|v| v.as_str()).is_some_and(|p| p.contains("{{")) {
                    return Err(format!("`{k}` must be written out as a path, not taken from a step's output ({{{{…}}}})"));
                }
            }
            // a directory to run in is one the workspace has by now (the steps before this one have run): a made-up one
            // (`/workspace/site`, `site/background`) is the model's mistake to correct here, not a failed step to re-plan
            if let Some(c) = a.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty() && !c.contains("{{")) {
                match self.policy.guard().read_path(c) {
                    Ok(p) if p.is_dir() => {}
                    Ok(_) => return Err(format!("`cwd` '{c}' is not a directory of the workspace: use one that exists, or leave cwd out")),
                    Err(_) => return Err(format!("`cwd` '{c}' is outside the workspace: use a directory inside it, relative (`site`), or leave cwd out")),
                }
            }
            // an edit replaces text the file has: a model that invented it is told so here, not by a failed step and a re-plan
            if meta.name == "fs_edit"
                && let (Some(path), Some(old)) = (a.get("path").and_then(|v| v.as_str()), a.get("old_string").and_then(|v| v.as_str()))
                && !path.contains("{{")
            {
                if old.is_empty() {
                    return Err("old_string is empty: copy the exact text to replace from the FILE".into());
                }
                if let Ok(p) = self.policy.guard().read_path(path)
                    && let Ok(text) = std::fs::read_to_string(p)
                {
                    let n = text.matches(old).count();
                    if n == 0 {
                        return Err(format!("old_string is not in {path}: copy a piece of it exactly from the FILE (indentation included)"));
                    }
                    if n > 1 && a.get("replace_all").and_then(|v| v.as_bool()) != Some(true) {
                        return Err(format!("old_string occurs {n} times in {path}: make it longer so it occurs once (or set replace_all)"));
                    }
                }
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
            how.push(if self.step_tier_for_content(step) == Tier::Planner { "planner" } else { "worker" });
        }
        if valid(&args).is_ok() {
            if how.is_empty() {
                how.push(if planned { "plan" } else { "rules" });
            }
        } else {
            // ---- the models: the worker model first, the planner model only if the worker model cannot ----
            let mut problem = feedback.map(str::to_string).or_else(|| valid(&args).err());
            let mut ladder: Vec<Tier> = Vec::new();
            // a step the planner kept for itself (a fix the worker model could not make) is bound by the planner model
            if step.planner && self.models.has(Tier::Planner) {
                ladder.push(Tier::Planner);
            } else {
                if self.models.has(Tier::Worker) {
                    ladder.extend([Tier::Worker, Tier::Worker]);
                }
                if self.models.has(Tier::Planner) {
                    ladder.push(Tier::Planner);
                }
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
                let samples = if tier == Tier::Worker && rung == 1 { 3 } else { 1 };
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
                            if let Some(f) = repair::fix_args(&meta, &Value::Object(merged.clone()), &step.description, &entities_for_args) {
                                merged = object(&f.args);
                            }
                            let mut verdict = valid(&merged);
                            // a directory that is nowhere is no reason to throw the command away: without it the command runs in the workspace
                            if verdict.as_ref().is_err_and(|e| e.starts_with("`cwd`")) && !meta.params.iter().any(|p| p.name == "cwd" && p.required) {
                                merged.remove("cwd");
                                verdict = valid(&merged);
                            }
                            match verdict {
                                Ok(()) => candidates.push(merged),
                                Err(e) => problem = Some(e),
                            }
                        }
                        Err(e) => problem = Some(e),
                    }
                }
                if let Some(best) = majority(candidates) {
                    args = best;
                    how.push(if tier == Tier::Planner {
                        "planner"
                    } else if samples > 1 {
                        "worker-vote"
                    } else {
                        "worker"
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

        // `cd proj && make` with cwd already `proj`: the cd would fail where it is, and the command is right without it
        if tool == "shell"
            && let (Some(cwd), Some(command)) = (args.get("cwd").and_then(|c| c.as_str()), args.get("command").and_then(|c| c.as_str()))
            && let Some(rest) = strip_redundant_cd(command, cwd)
        {
            args.insert("command".into(), Value::String(rest));
            how.push("rules");
        }

        // "…in proj": a command that does not say where it runs, for a step that names a directory that exists, runs there
        if tool == "shell" && args.get("cwd").is_none() && meta.params.iter().any(|p| p.name == "cwd") {
            let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
            if let Some(dir) = directory_named(&step.description, command, self.policy.workspace()) {
                args.insert("cwd".into(), Value::String(dir));
                how.push("rules");
            }
        }

        // A server never returns: run in the foreground it holds the step until its timeout. The planner's sentence ("in the background") and
        // the command itself say so; a small model forgets to set the flag.
        if tool == "shell" && args.get("background").is_none() && meta.params.iter().any(|p| p.name == "background") {
            let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
            if implies_background(&step.description, command) {
                args.insert("background".into(), Value::Bool(true));
                how.push("rules");
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
        // "report.md exists" of a step that ran in `proj`: the file is where the step was
        if let Some(cwd) = step.args.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty())
            && self.policy.guard().exists(&format!("{}/{want}", cwd.trim_end_matches('/'))).unwrap_or(false)
        {
            return true;
        }
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
            if let Check::UrlServes { url, value } = c {
                // a server that was just started builds on its first request: it is given time to come up
                if let Err(why) = crate::verify::probe_url(url, value.as_deref(), std::time::Duration::from_secs(45)).await {
                    missed.push(format!("{} ({why})", c.describe()));
                }
                continue;
            }
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
    fn a_page_about_a_named_thing_must_name_it() {
        let step = "Create README.md at the workspace root with a concise project description for Brewster, a tool that tracks coffee orders.";
        assert_eq!(missing_names(step, "README.md", "git clone <repository-url>\ncd repo"), vec!["Brewster".to_string()]);
        assert!(missing_names(step, "README.md", "# Brewster\nTracks coffee orders.").is_empty());
        assert!(missing_names("write .gitignore for a Python project", ".gitignore", "__pycache__/").is_empty(), "config files are not checked");
        assert!(missing_names("write src/main.rs that prints Hello", "src/main.rs", "fn main(){}").is_empty(), "code is not checked");
        assert!(missing_names("Write site/index.html, a landing page about planet Earth", "site/index.html", "<html>earth</html>").is_empty());
        assert_eq!(missing_names("write about.html titled \"Our Story\"", "about.html", "<h1>About</h1>"), vec!["Our Story".to_string()]);
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
    use super::{bind_view, collapse_repeats, degenerate, strip_fences, strip_tool_markup};

    #[test]
    fn a_file_comes_out_of_whatever_a_small_model_wraps_around_it() {
        let page = "<!doctype html>\n<html><body><h1>Earth</h1></body></html>";
        assert_eq!(strip_fences(page), page);
        assert_eq!(strip_fences(&format!("```html\n{page}\n```")), page);
        assert_eq!(strip_fences(&format!("Sure! Here is the page:\n\n```html\n{page}\n```\nLet me know if you want changes.")), page);
        assert_eq!(strip_fences(&format!("Here is the page you asked for:\n{page}")), page);
        assert_eq!(strip_fences("```\nport = 1\n```"), "port = 1");
        assert_eq!(strip_fences("plain text file"), "plain text file");
        // a README's own code samples are content, not a wrapper
        let readme = "# Brewster\n\nTracks coffee orders.\n\n## Usage\n\n```bash\ngit clone https://example.com/brewster.git\ncd brewster\n```\n\nThen run it.";
        assert_eq!(strip_fences(readme), readme);
        let short = "# Brewster\nTracks orders.\n```bash\npip install brewster\n```";
        assert_eq!(strip_fences(short), short, "a heading is content, never a preamble");
        // a markdown file sent inside a fence keeps its own inner fences
        assert_eq!(strip_fences(&format!("```markdown\n{readme}\n```")), readme);
        assert_eq!(strip_fences(&format!("Here is README.md:\n```markdown\n{readme}\n```")), readme);
    }

    #[test]
    fn a_real_page_is_not_a_loop_however_much_structure_it_repeats() {
        let mut page = String::from("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<style>\n");
        for i in 0..60 {
            page.push_str(&format!(".card-{i} {{\n  margin: 0 auto;\n  padding: 1rem;\n  color: #fff;\n}}\n"));
        }
        page.push_str("</style>\n</head>\n<body>\n");
        for i in 0..40 {
            page.push_str(&format!("<div class=\"card-{i}\">\n  <h3>Card {i}</h3>\n  <p>Text of card {i}.</p>\n</div>\n"));
        }
        page.push_str("</body>\n</html>\n");
        assert!(!degenerate(&page), "a page with 400 lines of repeated structure is a page");
    }

    #[test]
    fn the_binding_model_is_not_asked_for_numbers_nobody_mentioned() {
        use dfae_core::*;
        let shell = ToolMetadata {
            name: "shell".into(),
            description: "run".into(),
            category: "system".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: true,
            reversible: false,
            cost_class: "low".into(),
            keywords: vec![],
            direct_patterns: vec![],
            cacheable: false,
            contract: vec![],
            params: vec![
                ParamSpec::new("command", ParamType::String, true, "command"),
                ParamSpec::new("cwd", ParamType::String, false, "directory"),
                ParamSpec::new("timeout_secs", ParamType::Integer, false, "seconds"),
                ParamSpec::new("background", ParamType::Boolean, false, "detach"),
            ],
        };
        let names = |m: &ToolMetadata| m.params.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&bind_view(&shell, "start the server in the site directory")), ["command", "cwd"]);
        assert_eq!(names(&bind_view(&shell, "serve it on port 8001")), ["command", "cwd"], "a port is not a timeout");
        assert_eq!(names(&bind_view(&shell, "run the tests with a timeout of 300 seconds")), ["command", "cwd", "timeout_secs"]);
        assert_eq!(names(&bind_view(&shell, "run it in the background")), ["command", "cwd", "background"]);
    }

    #[test]
    fn a_cd_into_the_directory_the_command_already_runs_in_is_dropped() {
        use super::strip_redundant_cd as s;
        assert_eq!(s("cd lumen && npm run build", "lumen").as_deref(), Some("npm run build"));
        assert_eq!(s("cd ./lumen/ ; npm test", "lumen").as_deref(), Some("npm test"));
        assert_eq!(s("cd \"app\" && make -j4", "./app").as_deref(), Some("make -j4"));
        assert_eq!(s("cd lumen/sub && ls", "lumen"), None, "a deeper directory is a real cd");
        assert_eq!(s("npm run build", "lumen"), None);
    }

    #[test]
    fn a_shell_step_that_names_a_directory_runs_in_it() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("proj/src")).unwrap();
        let d = |step: &str, cmd: &str| super::directory_named(step, cmd, ws.path());
        assert_eq!(d("show the Git log in proj using git log", "git log").as_deref(), Some("proj"));
        assert_eq!(d("run the tests inside the folder `proj`", "pytest").as_deref(), Some("proj"));
        assert_eq!(d("compile with cwd proj/src", "make").as_deref(), Some("proj/src"));
        assert_eq!(d("show the log in proj", "cd proj && git log"), None, "it goes there itself");
        assert_eq!(d("show the log in proj", "git -C proj log"), None);
        assert_eq!(d("run in the workspace root", "ls"), None);
        assert_eq!(d("list files in nowhere", "ls"), None, "a directory that is not there is no cwd");
    }

    #[test]
    fn a_models_own_tool_call_syntax_never_reaches_a_file_or_an_argument() {
        let page = "<html><body>x</body></html>";
        assert_eq!(strip_tool_markup(&format!("{page}\n</arg_value><arg_key>path</arg_key><arg_value>a.html</arg_value></tool_call>")), page);
        assert_eq!(strip_tool_markup("site/index.html</arg_value>"), "site/index.html");
        assert_eq!(strip_tool_markup(page), page);
    }

    #[test]
    fn a_page_with_a_starfield_is_not_cut_down() {
        let mut page = String::from("<!DOCTYPE html>\n<html>\n<body>\n<div class=\"sky\">\n");
        page.push_str(&"<div class=\"star\"></div>\n".repeat(30));
        page.push_str("</div>\n<div>\n<div>\n<div>\nx\n</div>\n</div>\n</div>\n</body>\n</html>");
        assert!(!degenerate(&page));
        // what a loop leaves is still a whole file: the closing tags far from the loop stay
        let kept = collapse_repeats(&format!("{page}\n{}", "<p>some line of text</p>\n".repeat(50)));
        assert!(kept.contains("</html>") && kept.matches("</body>").count() == 1, "{kept}");
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

#[cfg(test)]
mod background_tests {
    use super::implies_background;

    #[test]
    fn installing_a_server_is_not_starting_one() {
        assert!(!implies_background("install the dependencies and run the tests", "pip install fastapi uvicorn pytest httpx && python3 -m pytest -q"));
        assert!(!implies_background("install", "npm install next && npm run build"));
        assert!(implies_background("serve it", "pip install uvicorn && uvicorn main:app --port 8001"));
    }

    #[test]
    fn servers_and_background_work_do_not_hold_the_step() {
        assert!(implies_background("start the server in the background on port 8000", "python3 -m http.server 8000"));
        assert!(implies_background("serve the site directory", "python3 -m http.server 8000 --bind 0.0.0.0"));
        assert!(implies_background("run the dev server", "npm run dev"));
        assert!(implies_background("run it", "uvicorn app:app --port 8001"));
        assert!(!implies_background("list the files", "ls -la"));
        assert!(!implies_background("run the tests", "cargo test"));
    }
}
