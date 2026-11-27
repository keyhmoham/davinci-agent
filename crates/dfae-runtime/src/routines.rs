//! The routine book and its scheduler: work that runs later, once or again and again, in the background (plan item R1).
//!
//! * **Durable**: the book is one document in the store's key/value table (Postgres in the container), written on every change; at
//!   start it is read back, missed one-shots run, a missed recurring slot runs once (or is skipped, `catch_up: false`), and a run that was
//!   in progress is watched to its end (the task itself resumes from its checkpoint).
//! * **Cheap**: a run is an ordinary task in the routine's own session (it sees the last run's turn), told what the last run found so it
//!   reports what changed; gates run first and need no model at all — `if_ok` (a command that must succeed) and `if_changed` (a command
//!   or `GET <url>` whose output must differ from last time; the first run only records it). Measured elsewhere: more wake-ups are not
//!   better (PM-Bench: a fixed 30-minute heartbeat had the most false alarms), so nothing here wakes on its own; routines do.
//! * **Bounded**: a routine never overlaps itself, at most `DFAE_ROUTINE_CONCURRENCY` run at once and `DFAE_ROUTINE_MAX_RUNS_PER_DAY`
//!   start per day; failures back off (30 s … 1 h) and end a routine after ten in a row; a spending cap ends it too.
//! * **Not a way in**: a routine asked for by a task that has read outside content, or by a routine's own run, is only *proposed* — it
//!   starts disabled and a person enables it (the board, the API). A page cannot plant a job that runs after the task is gone.

use crate::engine::{Engine, Inner};
use chrono::{DateTime, Utc};
use dfae_core::routine::*;
use dfae_core::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

const KV_KEY: &str = "routines";
const MAX_ROUTINES: usize = 100;
/// Events about routines are published under this task id.
pub const ROUTINES_STREAM: &str = "routines";

/// What a caller gives to add a routine or change one (`None`: unchanged, or the default on add).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutineSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub max_runs: Option<u32>,
    /// RFC 3339, or a date (`2026-12-31`, the end of that day in the routine's zone).
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub active_hours: Option<String>,
    #[serde(default)]
    pub if_changed: Option<String>,
    #[serde(default)]
    pub if_ok: Option<String>,
    #[serde(default)]
    pub catch_up: Option<bool>,
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
}

#[derive(Default)]
pub(crate) struct RoutineBook {
    map: Mutex<BTreeMap<String, Routine>>,
    wake: tokio::sync::Notify,
    /// (day, runs started that day)
    day: Mutex<(chrono::NaiveDate, u32)>,
    started: std::sync::atomic::AtomicBool,
}

impl RoutineBook {
    pub fn list(&self) -> Vec<Routine> {
        let mut v: Vec<Routine> = self.map.lock().values().cloned().collect();
        v.sort_by_key(|r| (r.state.next_run.is_none(), r.state.next_run, r.created_at));
        v
    }
    pub fn get(&self, id: &str) -> Option<Routine> {
        let m = self.map.lock();
        m.get(id).cloned().or_else(|| m.values().find(|r| r.id.starts_with(id) && id.len() >= 4).cloned())
    }
}

fn parse_until(s: &str, tz: chrono_tz::Tz) -> Result<DateTime<Utc>, String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s.trim()) {
        return Ok(t.with_timezone(&Utc));
    }
    let d = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| format!("'until' is a date (2026-12-31) or an RFC 3339 time, not '{s}'"))?;
    use chrono::TimeZone;
    tz.from_local_datetime(&d.and_hms_opt(23, 59, 59).unwrap()).earliest().map(|t| t.with_timezone(&Utc)).ok_or_else(|| "bad 'until'".into())
}

fn clip(s: &str, n: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n { one } else { format!("{}…", one.chars().take(n).collect::<String>()) }
}

impl Inner {
    fn default_tz(&self) -> Option<String> {
        self.cfg.routines.timezone.clone()
    }

    async fn save_routines(&self) {
        let doc = serde_json::to_value(self.routines.list()).unwrap_or(Value::Null);
        if let Err(e) = self.store.kv_set(KV_KEY, doc).await {
            tracing::warn!(error = %e, "saving routines failed");
        }
    }

    fn routine_event(&self, id: &str, action: &str, detail: String) {
        self.bus.publish(Event {
            task_id: ROUTINES_STREAM.into(),
            seq: 0,
            ts: Utc::now(),
            kind: EventKind::Routine { routine_id: id.into(), action: action.into(), detail },
        });
    }

    /// Apply `spec` to `r` (validated). `creating`: goal and when are required.
    fn apply_spec(&self, r: &mut Routine, spec: &RoutineSpec, now: DateTime<Utc>) -> Result<(), String> {
        if let Some(tz) = &spec.tz {
            parse_tz(Some(tz))?;
            r.tz = Some(tz.trim().to_string()).filter(|t| !t.is_empty());
        }
        let tz = r.tz();
        if let Some(g) = &spec.goal {
            let g = g.trim();
            if g.is_empty() || g.chars().count() > 2000 {
                return Err("a routine's goal is 1–2000 characters".into());
            }
            r.goal = g.to_string();
        }
        if let Some(n) = &spec.name {
            r.name = clip(n, 60);
        }
        if let Some(w) = &spec.when {
            r.schedule = parse_when(w, tz, now)?;
            r.when = w.trim().to_string();
        }
        if let Some(m) = spec.max_runs {
            r.max_runs = (m > 0).then_some(m);
        }
        if let Some(u) = &spec.until {
            r.until = if u.trim().is_empty() { None } else { Some(parse_until(u, tz)?) };
        }
        if let Some(a) = &spec.active_hours {
            r.active_hours = if a.trim().is_empty() {
                None
            } else {
                in_active_hours(a, tz, now)?;
                Some(a.trim().to_string())
            };
        }
        for (field, val) in [(&mut r.if_changed, &spec.if_changed), (&mut r.if_ok, &spec.if_ok)] {
            if let Some(v) = val {
                *field = Some(v.trim().to_string()).filter(|v| !v.is_empty());
            }
        }
        if let Some(c) = spec.catch_up {
            r.catch_up = c;
        }
        if let Some(c) = spec.max_cost_usd {
            r.max_cost_usd = (c > 0.0).then_some(c);
        }
        if let Some(e) = spec.enabled {
            r.enabled = e;
            if e {
                r.proposed = None;
            }
        }
        r.updated_at = now;
        r.state.ended = None;
        r.state.next_run = r.compute_next(now.max(r.state.last_run.unwrap_or(now)) - chrono::Duration::seconds(1)).filter(|_| r.enabled);
        Ok(())
    }

    /// Add a routine. `proposed`: why it starts disabled (outside content was read, or a routine's own run asked), if it does.
    pub(crate) async fn add_routine(&self, spec: RoutineSpec, origin: Origin, proposed: Option<String>) -> Result<Routine, String> {
        let (Some(goal), Some(when)) = (spec.goal.clone(), spec.when.clone()) else {
            return Err("a routine needs a goal (what to do) and when (in 20m, every 2h, daily 09:00, cron …)".into());
        };
        if self.routines.map.lock().len() >= MAX_ROUTINES {
            return Err(format!("there are already {MAX_ROUTINES} routines: remove one first"));
        }
        let now = Utc::now();
        let mut r = Routine {
            id: new_id(),
            name: spec.name.clone().map(|n| clip(&n, 60)).unwrap_or_else(|| clip(&goal, 40)),
            goal: String::new(),
            schedule: Schedule::At { at: now },
            when: when.clone(),
            tz: self.default_tz(),
            enabled: proposed.is_none(),
            proposed: proposed.clone(),
            max_runs: None,
            until: None,
            active_hours: None,
            if_changed: None,
            if_ok: None,
            catch_up: true,
            max_cost_usd: None,
            origin,
            created_at: now,
            updated_at: now,
            state: RoutineState::default(),
            history: vec![],
        };
        let mut spec = spec;
        if proposed.is_some() {
            spec.enabled = Some(false);
        }
        self.apply_spec(&mut r, &spec, now)?;
        if proposed.is_none() && r.state.next_run.is_none() {
            return Err(format!("'{when}' never runs (it is in the past, or its limits end it before its first run)"));
        }
        self.routines.map.lock().insert(r.id.clone(), r.clone());
        self.save_routines().await;
        self.routine_event(&r.id, if proposed.is_some() { "proposed" } else { "added" }, r.line());
        self.routines.wake.notify_one();
        Ok(r)
    }

    pub(crate) async fn update_routine(&self, id: &str, spec: RoutineSpec) -> Result<Routine, String> {
        let id = self.routines.get(id).ok_or_else(|| format!("no routine '{id}'"))?.id;
        let r = {
            let mut m = self.routines.map.lock();
            let r = m.get_mut(&id).ok_or("gone")?;
            let mut next = r.clone();
            self.apply_spec(&mut next, &spec, Utc::now())?;
            *r = next;
            r.clone()
        };
        self.save_routines().await;
        self.routine_event(&r.id, "updated", r.line());
        self.routines.wake.notify_one();
        Ok(r)
    }

    pub(crate) async fn remove_routine(&self, id: &str) -> Result<Routine, String> {
        let id = self.routines.get(id).ok_or_else(|| format!("no routine '{id}'"))?.id;
        let r = self.routines.map.lock().remove(&id).ok_or("gone")?;
        self.save_routines().await;
        self.routine_event(&r.id, "removed", r.name.clone());
        Ok(r)
    }

    /// Read the book back and start the scheduler (once).
    pub(crate) async fn start_routines(self: &Arc<Self>) {
        if self.routines.started.swap(true, std::sync::atomic::Ordering::SeqCst) || !self.cfg.routines.enabled {
            return;
        }
        let saved: Vec<Routine> = self.store.kv_get(KV_KEY).await.ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let now = Utc::now();
        let mut watch = Vec::new();
        {
            let mut m = self.routines.map.lock();
            for mut r in saved {
                if let Some(task) = r.state.running.clone() {
                    watch.push((r.id.clone(), task));
                }
                // a missed recurring slot: once now (catch-up), or the next one
                if r.enabled && r.state.next_run.is_some_and(|n| n < now) && !matches!(r.schedule, Schedule::At { .. }) && !r.catch_up {
                    r.state.next_run = r.compute_next(now);
                }
                m.insert(r.id.clone(), r);
            }
        }
        for (id, task) in watch {
            let inner = self.clone();
            tokio::spawn(async move { inner.finish_run(&id, &task).await });
        }
        let inner = Arc::downgrade(self);
        tokio::spawn(async move { scheduler(inner).await });
    }

    /// Start a run of `id` now, whatever its schedule says (gates still apply unless `force`).
    pub(crate) async fn fire_routine(self: &Arc<Self>, id: &str, force: bool) -> Result<Option<String>, String> {
        let r = self.routines.get(id).ok_or_else(|| format!("no routine '{id}'"))?;
        if let Some(t) = &r.state.running {
            return Err(format!("its previous run ({}) is still going", short(t)));
        }
        let now = Utc::now();
        let capped = {
            let mut d = self.routines.day.lock();
            if d.0 != now.date_naive() {
                *d = (now.date_naive(), 0);
            }
            d.1 >= self.cfg.routines.max_runs_per_day
        };
        if capped {
            self.skip(&r.id, now, &format!("the daily cap of {} routine runs is reached", self.cfg.routines.max_runs_per_day), true).await;
            return Ok(None);
        }
        if !force && let Some(why) = self.gate(&r).await {
            self.skip(&r.id, now, &why, true).await;
            return Ok(None);
        }
        let mut goal = r.goal.clone();
        if let (Some(last), Some(at)) = (&r.state.last_summary, r.state.last_run) {
            goal = format!(
                "{goal}\n(This is routine \"{}\", run {}. Last run {}: {}. Say what is new or changed since then; if nothing is, say so in one line.)",
                r.name,
                r.state.runs + 1,
                at.with_timezone(&r.tz()).format("%Y-%m-%d %H:%M"),
                clip(last, 240)
            );
        }
        let req = TaskRequest { goal, session_id: Some(format!("routine-{}", short(&r.id))), routine: Some(r.id.clone()), ..Default::default() };
        let task = self.submit_request(req).await?;
        self.routines.day.lock().1 += 1;
        let line = {
            let mut m = self.routines.map.lock();
            let Some(x) = m.get_mut(&r.id) else { return Ok(Some(task)) };
            x.state.running = Some(task.clone());
            x.state.last_run = Some(now);
            x.state.runs += 1;
            x.state.next_run = x.compute_next(now).filter(|_| x.enabled);
            x.line()
        };
        self.save_routines().await;
        self.routine_event(&r.id, "started", format!("task {} — {line}", short(&task)));
        let inner = self.clone();
        let (rid, t) = (r.id.clone(), task.clone());
        tokio::spawn(async move { inner.finish_run(&rid, &t).await });
        Ok(Some(task))
    }

    /// The gates, in order: `if_ok`, then `if_changed`. `Some(why)`: skip this run (no task, no model).
    async fn gate(&self, r: &Routine) -> Option<String> {
        if let Some(cmd) = &r.if_ok {
            match self.probe(cmd).await {
                Ok((true, _)) => {}
                Ok((false, out)) => return Some(format!("if_ok did not pass: {}", clip(&out, 120))),
                Err(e) => return Some(format!("if_ok could not run: {e}")),
            }
        }
        if let Some(p) = &r.if_changed {
            let out = match self.probe(p).await {
                Ok((_, out)) => out,
                Err(e) => return Some(format!("if_changed could not run: {e}")),
            };
            let hash = sha256_hex(&out);
            let before = {
                let mut m = self.routines.map.lock();
                m.get_mut(&r.id).map(|x| x.state.watch_hash.replace(hash.clone())).unwrap_or(None)
            };
            match before {
                None => return Some("if_changed: the first look only records what is there now".into()),
                Some(b) if b == hash => return Some("if_changed: nothing changed".into()),
                _ => {}
            }
        }
        None
    }

    /// Run a gate: `GET <url>` through `http_get`, anything else through the shell — both through the policy, like any tool call.
    async fn probe(&self, spec: &str) -> Result<(bool, String), String> {
        let (tool, args) = match spec.trim().strip_prefix("GET ").or_else(|| spec.trim().strip_prefix("get ")) {
            Some(url) => ("http_get", json!({"url": url.trim()})),
            None => ("shell", json!({"command": spec.trim()})),
        };
        let meta = self.tools.meta(tool).ok_or_else(|| format!("no {tool} tool"))?;
        match self.policy.check(&meta, &args, false) {
            dfae_tools::PolicyDecision::Deny(m) => return Err(m),
            dfae_tools::PolicyDecision::RequireApproval(m) if self.cfg.approvals == ApprovalMode::Ask => return Err(format!("needs approval: {m}")),
            _ => {}
        }
        let t = self.tools.get(tool).ok_or("tool vanished")?;
        let tctx = ToolContext {
            task_id: ROUTINES_STREAM.into(),
            step_id: "gate".into(),
            workspace: self.policy.workspace().to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        match tokio::time::timeout(Duration::from_secs(60), t.execute(args, tctx)).await {
            Ok(Ok(o)) => Ok((o.ok && o.exit_code.unwrap_or(0) == 0, o.content)),
            Ok(Err(e)) if tool == "shell" => Ok((false, e.message)),
            Ok(Err(e)) => Err(e.message),
            Err(_) => Err("timed out after 60 s".into()),
        }
    }

    async fn skip(&self, id: &str, now: DateTime<Utc>, why: &str, advance: bool) {
        let line = {
            let mut m = self.routines.map.lock();
            let Some(r) = m.get_mut(id) else { return };
            r.record(RoutineRun { at: now, outcome: RunOutcome::Skipped, task_id: None, note: why.into(), cost_usd: 0.0 });
            if advance {
                r.state.next_run = r.compute_next(now).filter(|_| r.enabled);
            }
            r.line()
        };
        self.save_routines().await;
        self.routine_event(id, "skipped", format!("{why} — {line}"));
    }

    /// Wait for a run's task to end and record what it did.
    async fn finish_run(self: &Arc<Self>, id: &str, task: &str) {
        let limit = Duration::from_secs(self.cfg.budget.max_seconds.max(60) + 600);
        let start = std::time::Instant::now();
        let rec = loop {
            let live = self.live.lock().contains_key(task);
            let rec = self.store.get_task(task).await.ok().flatten();
            if let Some(r) = &rec
                && r.status.is_terminal()
                && !live
            {
                break rec;
            }
            if start.elapsed() > limit || (rec.is_none() && !live && start.elapsed() > Duration::from_secs(30)) {
                break rec;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        let now = Utc::now();
        let (outcome, note, cost) = match &rec {
            Some(t) if t.status == TaskStatus::Completed => {
                let partial = t.verification.as_ref().is_some_and(|v| matches!(v.status, VerificationStatus::Partial | VerificationStatus::Failed));
                (if partial { RunOutcome::Partial } else { RunOutcome::Ok }, t.final_answer.clone().unwrap_or_default(), t.ledger.summary().cost_usd)
            }
            Some(t) => (RunOutcome::Failed, t.error.clone().unwrap_or_else(|| format!("{:?}", t.status)), t.ledger.summary().cost_usd),
            None => (RunOutcome::Failed, "the task's record was not found".into(), 0.0),
        };
        let (line, ended) = {
            let mut m = self.routines.map.lock();
            let Some(r) = m.get_mut(id) else { return };
            r.state.running = None;
            r.state.spent_usd += cost;
            r.record(RoutineRun { at: now, outcome, task_id: Some(task.into()), note: clip(&note, 200), cost_usd: cost });
            if outcome == RunOutcome::Failed {
                r.state.failures += 1;
                let retry = now + backoff(r.state.failures);
                match r.schedule {
                    // a one-shot is retried three times, then it has failed
                    Schedule::At { .. } if r.state.failures <= 3 => r.state.next_run = Some(retry),
                    Schedule::At { .. } => r.state.ended = Some(format!("failed {} times", r.state.failures)),
                    _ => r.state.next_run = r.state.next_run.map(|n| n.max(retry)),
                }
                if r.state.failures >= MAX_FAILURES {
                    r.state.ended = Some(format!("{MAX_FAILURES} failures in a row"));
                }
            } else {
                r.state.failures = 0;
                r.state.last_summary = Some(clip(&note, 400));
            }
            if r.max_cost_usd.is_some_and(|c| r.state.spent_usd >= c) {
                r.state.ended = Some(format!("its spending cap of ${:.2} is reached", r.max_cost_usd.unwrap_or(0.0)));
            }
            if r.state.ended.is_none() && r.state.next_run.is_none() && r.enabled {
                r.state.ended = Some(match r.schedule {
                    Schedule::At { .. } => "done".into(),
                    _ if r.max_runs.is_some_and(|x| r.state.runs >= x) => format!("{} runs done", r.state.runs),
                    _ => "its end date has passed".into(),
                });
            }
            if r.state.ended.is_some() {
                r.state.next_run = None;
            }
            (r.line(), r.state.ended.clone())
        };
        self.save_routines().await;
        self.routine_event(id, "finished", format!("{outcome:?}: {} — {line}", clip(&note, 160)));
        if let Some(e) = ended {
            self.routine_event(id, "ended", e);
        }
        self.routines.wake.notify_one();
    }
}

/// Sleep until the next routine is due (or the book changes), then start what is due.
async fn scheduler(inner: Weak<Inner>) {
    loop {
        let Some(me) = inner.upgrade() else { return };
        let now = Utc::now();
        let routines = me.routines.list();
        let running = routines.iter().filter(|r| r.state.running.is_some()).count();
        let due: Vec<String> = routines
            .iter()
            .filter(|r| r.enabled && r.state.running.is_none() && r.state.next_run.is_some_and(|n| n <= now))
            .map(|r| r.id.clone())
            .take(me.cfg.routines.concurrency.saturating_sub(running))
            .collect();
        for id in due {
            if let Err(e) = me.fire_routine(&id, false).await {
                // the dispatcher is full or the task was refused: try again shortly, not on the next slot
                let mut m = me.routines.map.lock();
                if let Some(r) = m.get_mut(&id) {
                    r.state.next_run = Some(now + chrono::Duration::seconds(30));
                }
                drop(m);
                me.routine_event(&id, "skipped", format!("could not start: {e}"));
            }
        }
        let next = me.routines.list().iter().filter(|r| r.enabled && r.state.running.is_none()).filter_map(|r| r.state.next_run).min();
        let wait = next.map(|n| (n - Utc::now()).to_std().unwrap_or(Duration::ZERO)).unwrap_or(Duration::from_secs(60)).min(Duration::from_secs(60));
        // the engine is held for at most a minute: a dropped engine ends the loop at the next turn
        tokio::select! {
            _ = tokio::time::sleep(wait.max(Duration::from_millis(200))) => {}
            _ = me.routines.wake.notified() => {}
        }
    }
}

// ------------------------------------------------------------ public API ----

impl Engine {
    /// Read the routine book and start the scheduler (call once at startup, after `resume_unfinished`).
    pub async fn start_routines(&self) {
        self.inner.start_routines().await;
    }
    pub fn routines(&self) -> Vec<Routine> {
        self.inner.routines.list()
    }
    pub fn routine(&self, id: &str) -> Option<Routine> {
        self.inner.routines.get(id)
    }
    /// Add a routine for a person (the API, the TUI): enabled unless the spec says otherwise.
    pub async fn add_routine(&self, spec: RoutineSpec, by: &str) -> Result<Routine, String> {
        self.inner.add_routine(spec, Origin { by: by.into(), task_id: None }, None).await
    }
    pub async fn update_routine(&self, id: &str, spec: RoutineSpec) -> Result<Routine, String> {
        self.inner.update_routine(id, spec).await
    }
    pub async fn remove_routine(&self, id: &str) -> Result<Routine, String> {
        self.inner.remove_routine(id).await
    }
    /// Run a routine now. `gates`: still only if its `if_ok` / `if_changed` gates pass. The task's id, if one started.
    pub async fn run_routine(&self, id: &str, gates: bool) -> Result<Option<String>, String> {
        self.inner.fire_routine(id, !gates).await
    }
}

// --------------------------------------------------------- agent tools ----

/// Why a routine change asked for by this task may only be proposed: the task has read outside content (a page could be asking), or it
/// is a routine's own run (a routine planting routines). `None`: the request is the user's own.
async fn proposal_reason(inner: &Inner, task_id: &str) -> Option<String> {
    let ctx = inner.live.lock().get(task_id).cloned();
    let rec = match ctx {
        Some(c) => c.rec.lock().await.clone(),
        None => inner.store.get_task(task_id).await.ok().flatten()?,
    };
    if rec.exposed {
        Some("asked for after content from outside (the web or an outside service) was read".into())
    } else if rec.request.routine.is_some() {
        Some("asked for by a routine's own run".into())
    } else {
        None
    }
}

struct RoutineTool {
    inner: Weak<Inner>,
    name: &'static str,
}

fn p(name: &str, ty: ParamType, req: bool, d: &str) -> ParamSpec {
    ParamSpec::new(name, ty, req, d)
}

fn spec_from(v: &Value) -> RoutineSpec {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from).filter(|x| !x.trim().is_empty());
    RoutineSpec {
        name: s("name"),
        goal: s("goal"),
        when: s("when"),
        tz: s("tz"),
        enabled: v.get("enabled").and_then(|x| x.as_bool().or_else(|| x.as_str().map(|s| matches!(s, "true" | "yes" | "on")))),
        max_runs: v.get("max_runs").and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).map(|n| n as u32),
        until: s("until"),
        active_hours: s("active_hours"),
        if_changed: s("if_changed"),
        if_ok: s("if_ok"),
        catch_up: v.get("catch_up").and_then(|x| x.as_bool()),
        max_cost_usd: v.get("max_cost_usd").and_then(|x| x.as_f64()),
    }
}

#[async_trait::async_trait]
impl Tool for RoutineTool {
    fn metadata(&self) -> ToolMetadata {
        let (desc, params, side, kws): (&str, Vec<ParamSpec>, bool, &[&str]) = match self.name {
            "routine_add" => (
                "schedule work to run later in the background: once (in 20m, at 2026-12-01 09:00, tomorrow 8am) or repeatedly (every 2h, daily 09:00, weekdays 08:30, weekly mon,fri 18:00, cron 0 9 * * 1-5)",
                vec![
                    p("goal", ParamType::String, true, "what to do each time, as a request"),
                    p("when", ParamType::String, true, "the schedule"),
                    p("name", ParamType::String, false, "a short name"),
                    p("max_runs", ParamType::Integer, false, "stop after this many runs"),
                    p("until", ParamType::String, false, "stop after this date (2026-12-31)"),
                    p("active_hours", ParamType::String, false, "only start between these local times (08:00-22:00)"),
                    p(
                        "if_changed",
                        ParamType::String,
                        false,
                        "run only when this command's output, or `GET <url>`, changed since the last run (no model call otherwise)",
                    ),
                    p("if_ok", ParamType::String, false, "run only when this shell command exits 0"),
                    p("tz", ParamType::String, false, "IANA time zone (Europe/Berlin)"),
                ],
                true,
                &[
                    "schedule",
                    "remind",
                    "reminder",
                    "later",
                    "every",
                    "daily",
                    "weekly",
                    "hourly",
                    "cron",
                    "routine",
                    "recurring",
                    "periodically",
                    "monitor",
                    "watch",
                    "tomorrow",
                ],
            ),
            "routine_list" => ("list the scheduled routines with their next run", vec![], false, &["routines", "scheduled", "reminders", "jobs"]),
            "routine_update" => (
                "change a routine: pause or resume it (enabled), its schedule, goal or limits",
                vec![
                    p("id", ParamType::String, true, "the routine's id (or its first 8 characters)"),
                    p("enabled", ParamType::Boolean, false, "false pauses it, true resumes it"),
                    p("when", ParamType::String, false, "a new schedule"),
                    p("goal", ParamType::String, false, "what to do instead"),
                    p("max_runs", ParamType::Integer, false, "stop after this many runs"),
                    p("until", ParamType::String, false, "stop after this date"),
                    p("active_hours", ParamType::String, false, "08:00-22:00"),
                ],
                true,
                &["pause", "resume", "reschedule", "routine"],
            ),
            "routine_remove" => ("delete a routine", vec![p("id", ParamType::String, true, "the routine's id")], true, &["cancel", "unschedule", "routine"]),
            _ => {
                ("run a routine now (its task starts in the background)", vec![p("id", ParamType::String, true, "the routine's id")], true, &["run", "routine"])
            }
        };
        let mut m = ToolMetadata {
            name: self.name.into(),
            description: desc.into(),
            category: "routines".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: side,
            reversible: true,
            cost_class: "free".into(),
            params,
            keywords: kws.iter().map(|k| k.to_string()).collect(),
            direct_patterns: vec![],
            cacheable: false,
            contract: vec![],
        };
        if self.name == "routine_list" {
            m.direct_patterns = vec![r"(?i)^\s*(?:list|show)\s+(?:my\s+|the\s+)?(?:routines|scheduled tasks|reminders)\s*\??$".into()];
        }
        m
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let inner = self.inner.upgrade().ok_or_else(|| ToolError::new(ErrorKind::Other, "the engine is shutting down"))?;
        let id = || arg_str(&input, "id").map(|s| s.trim().to_string());
        let err = |e: String| ToolError::new(ErrorKind::InvalidArgs, e);
        match self.name {
            "routine_add" => {
                let proposed = proposal_reason(&inner, &ctx.task_id).await;
                let r = inner
                    .add_routine(spec_from(&input), Origin { by: "agent".into(), task_id: Some(ctx.task_id.clone()) }, proposed.clone())
                    .await
                    .map_err(err)?;
                Ok(ToolOutput::ok(match proposed {
                    Some(why) => format!("proposed routine {} (disabled: {why}; a person enables it on the board): {}", short(&r.id), r.line()),
                    None => format!("added routine {}: {}", short(&r.id), r.line()),
                }))
            }
            "routine_list" => {
                let all = inner.routines.list();
                Ok(ToolOutput::ok(if all.is_empty() { "no routines".into() } else { all.iter().map(|r| r.line()).collect::<Vec<_>>().join("\n") }))
            }
            "routine_update" => {
                let spec = spec_from(&input);
                // pausing is always allowed; resuming or widening one is the user's call when outside content (or a routine) asks
                let widens = spec.enabled == Some(true) || spec.goal.is_some() || spec.when.is_some() || spec.if_ok.is_some() || spec.if_changed.is_some();
                if widens && let Some(why) = proposal_reason(&inner, &ctx.task_id).await {
                    return Err(ToolError::new(
                        ErrorKind::Withheld,
                        format!("refused: changing what a routine does or resuming it was {why}; a person can do it on the board"),
                    ));
                }
                let r = inner.update_routine(&id()?, spec).await.map_err(err)?;
                Ok(ToolOutput::ok(format!("updated routine {}: {}", short(&r.id), r.line())))
            }
            "routine_remove" => {
                let r = inner.remove_routine(&id()?).await.map_err(err)?;
                Ok(ToolOutput::ok(format!("removed routine {} \"{}\"", short(&r.id), r.name)))
            }
            _ => {
                if let Some(why) = proposal_reason(&inner, &ctx.task_id).await {
                    return Err(ToolError::new(ErrorKind::Withheld, format!("refused: running a routine was {why}")));
                }
                match inner.fire_routine(&id()?, true).await.map_err(err)? {
                    Some(t) => Ok(ToolOutput::ok(format!("started task {} for the routine", short(&t)))),
                    None => Ok(ToolOutput::ok("the run was skipped (see the routine's history)")),
                }
            }
        }
    }
}

pub(crate) fn register_tools(inner: &Arc<Inner>) {
    for name in ["routine_add", "routine_list", "routine_update", "routine_remove", "routine_run"] {
        inner.tools.register(Arc::new(RoutineTool { inner: Arc::downgrade(inner), name }));
    }
}

// ------------------------------------------------------------- the board ----

pub use dfae_core::routine::{Board, BoardTask};

fn board_task(t: &TaskRecord) -> BoardTask {
    let (done, total, doing) = t.plan.as_ref().map_or((0, 0, None), |p| {
        let doing = p.steps.iter().find(|s| s.status == StepStatus::Running).map(|s| {
            format!("{} {}: {}", s.id, s.tool.as_deref().unwrap_or(if s.kind == StepKind::Agent { "agent" } else { "reasoning" }), clip(&s.description, 80))
        });
        (p.steps.iter().filter(|s| s.status.is_done()).count(), p.steps.len(), doing)
    });
    BoardTask {
        task_id: t.task_id.clone(),
        goal: clip(&t.request.goal, 160),
        status: t.status,
        mode: t.mode,
        doing,
        steps_done: done,
        steps_total: total,
        role: t
            .team
            .as_ref()
            .map(|i| i.reason.clone().filter(|r| r.starts_with("agent:")).map(|r| r[6..].to_string()).unwrap_or_else(|| i.role.as_str().to_string())),
        parent_id: t.team.as_ref().and_then(|i| i.link.as_ref()).map(|l| l.parent_id.clone()),
        routine_id: t.request.routine.clone(),
        pending_approvals: t.approvals.iter().filter(|a| a.status == ApprovalStatus::Pending).count(),
        cost_usd: t.ledger.summary().cost_usd,
        started: t.created_at,
        updated: t.updated_at,
    }
}

impl Engine {
    pub async fn board(&self) -> Board {
        let live: Vec<Arc<crate::engine::TaskCtx>> = self.inner.live.lock().values().cloned().collect();
        let mut running = Vec::new();
        let mut waiting = Vec::new();
        for c in live {
            let t = board_task(&*c.rec.lock().await);
            if matches!(t.status, TaskStatus::WaitingForApproval) || t.pending_approvals > 0 {
                waiting.push(t);
            } else if !t.status.is_terminal() {
                running.push(t);
            }
        }
        running.sort_by_key(|t| t.started);
        let recent = self.inner.store.list_tasks(60).await.unwrap_or_default().iter().filter(|t| t.status.is_terminal()).take(20).map(board_task).collect();
        Board { now: Utc::now(), running, waiting, recent, routines: self.routines(), queued: self.inner.dispatcher.snapshot().queued }
    }
}
