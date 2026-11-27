//! Routines: work an agent (or the user) asks to be done later — once after a delay, at a time, every so often, or on a cron schedule —
//! with the limits a long-running agent needs (how many times, until when, in which hours) and gates that spare the model a run when
//! nothing changed. Prospective memory, in the literature's words: remembering to act in the future.
//!
//! Everything here is pure (no clock, no I/O): `now` is passed in, so the schedule logic is tested exhaustively.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// When a routine runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Once, at this instant.
    At { at: DateTime<Utc> },
    /// Every `secs` seconds, counted from `anchor` (so the times do not drift with how long a run takes).
    Every { secs: u64, anchor: DateTime<Utc> },
    /// A 5-field cron expression (minute hour day-of-month month day-of-week), in the routine's time zone.
    Cron { expr: String },
}

/// Who asked for a routine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Origin {
    /// `user` (the API, the TUI) or `agent` (a task's tool call).
    pub by: String,
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The task finished and was verified (or needed no verification).
    Ok,
    /// It finished, but its result is only partly verified.
    Partial,
    /// It failed or was cancelled.
    Failed,
    /// A gate said there was nothing to do: no task ran, no model was called.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineRun {
    pub at: DateTime<Utc>,
    pub outcome: RunOutcome,
    #[serde(default)]
    pub task_id: Option<String>,
    /// One line: the answer's start, or why it was skipped or failed.
    pub note: String,
    #[serde(default)]
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoutineState {
    pub next_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_run: Option<DateTime<Utc>>,
    /// Runs that started a task (gated skips do not count towards `max_runs`).
    #[serde(default)]
    pub runs: u32,
    /// Failures in a row (drives the backoff; a success resets it).
    #[serde(default)]
    pub failures: u32,
    /// The task of the run in progress, if any: a routine never overlaps itself.
    #[serde(default)]
    pub running: Option<String>,
    /// What the last run found (the next run is told, so it can report what changed).
    #[serde(default)]
    pub last_summary: Option<String>,
    /// Hash of the `if_changed` probe's output at the last run.
    #[serde(default)]
    pub watch_hash: Option<String>,
    #[serde(default)]
    pub spent_usd: f64,
    /// Why it stopped by itself (`max_runs reached`, `10 failures in a row`, …).
    #[serde(default)]
    pub ended: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub name: String,
    /// What to do, as a request (the same as typing it).
    pub goal: String,
    pub schedule: Schedule,
    /// The text the schedule was given as (`every 2h`, `weekdays 09:00`): shown back, never parsed again.
    pub when: String,
    /// IANA time zone for clock times and cron (`Europe/Berlin`); default the engine's.
    #[serde(default)]
    pub tz: Option<String>,
    pub enabled: bool,
    /// Set when the routine was proposed rather than created (it starts disabled and a human enables it).
    #[serde(default)]
    pub proposed: Option<String>,
    #[serde(default)]
    pub max_runs: Option<u32>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Only start runs between these local times (`08:00-22:00`; may wrap past midnight).
    #[serde(default)]
    pub active_hours: Option<String>,
    /// Run only if the output of this probe changed since the last run (a shell command, or `GET <url>`). No model is called otherwise.
    #[serde(default)]
    pub if_changed: Option<String>,
    /// Run only if this shell command exits 0.
    #[serde(default)]
    pub if_ok: Option<String>,
    /// After downtime: run a missed recurring slot once (`true`, default) or skip to the next one.
    #[serde(default = "yes")]
    pub catch_up: bool,
    /// Spending cap over all runs (USD); reaching it ends the routine.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    pub origin: Origin,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub state: RoutineState,
    /// The last runs, newest last (bounded).
    #[serde(default)]
    pub history: Vec<RoutineRun>,
}

fn yes() -> bool {
    true
}

pub const HISTORY_LEN: usize = 20;
pub const MIN_INTERVAL_SECS: u64 = 60;
/// After this many failures in a row a routine stops by itself (it would otherwise spend tokens failing).
pub const MAX_FAILURES: u32 = 10;

/// Backoff after `failures` consecutive failures of a recurring routine (30 s, 1 min, 5 min, 15 min, 1 h, then 1 h).
pub fn backoff(failures: u32) -> Duration {
    Duration::seconds(match failures {
        0 => 0,
        1 => 30,
        2 => 60,
        3 => 300,
        4 => 900,
        _ => 3600,
    })
}

pub fn parse_tz(tz: Option<&str>) -> Result<Tz, String> {
    match tz.map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(Tz::UTC),
        Some(t) => Tz::from_str(t).map_err(|_| format!("unknown time zone '{t}' (use an IANA name such as Europe/Berlin)")),
    }
}

static DURATION: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(\d+)\s*(seconds?|secs?|s|minutes?|mins?|m|hours?|hrs?|h|days?|d|weeks?|w)").unwrap());

/// Parse a duration such as `90s`, `20m`, `2h`, `1d`, `1h30m`, `2 hours and 5 minutes`, `1 week`: nothing else may be in the text.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim().to_lowercase();
    let mut total = 0i64;
    for c in DURATION.captures_iter(&s) {
        let n: i64 = c[1].parse().ok()?;
        total += n * match c[2].as_bytes()[0] {
            b's' => 1,
            b'm' => 60,
            b'h' => 3600,
            b'd' => 86_400,
            _ => 604_800,
        };
    }
    let rest = DURATION.replace_all(&s, "");
    let rest = rest.replace("and", "");
    (total > 0 && rest.chars().all(|c| c.is_whitespace() || c == ',')).then(|| Duration::seconds(total))
}

fn parse_clock(s: &str) -> Option<NaiveTime> {
    let s = s.trim().to_lowercase();
    let (body, pm) = match (s.strip_suffix("pm"), s.strip_suffix("am")) {
        (Some(b), _) => (b.trim().to_string(), Some(true)),
        (_, Some(b)) => (b.trim().to_string(), Some(false)),
        _ => (s.clone(), None),
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (body.parse::<u32>().ok()?, 0),
    };
    let h = match pm {
        Some(true) if h < 12 => h + 12,
        Some(false) if h == 12 => 0,
        _ => h,
    };
    NaiveTime::from_hms_opt(h, m, 0)
}

fn weekday(s: &str) -> Option<u32> {
    let w: Weekday = s.trim().parse().ok()?;
    Some(w.num_days_from_sunday())
}

/// The cron expression for the readable forms: `daily 09:00`, `weekdays 8:30`, `weekends 10:00`, `weekly mon,thu 18:00`,
/// `monthly 1 09:00` (day of month), `hourly :15`.
fn readable_cron(s: &str) -> Option<String> {
    let s = s.trim().to_lowercase();
    let mut words = s.split_whitespace();
    let head = words.next()?;
    let rest: Vec<&str> = words.collect();
    let clock = |w: Option<&&str>| -> Option<NaiveTime> { w.map_or(NaiveTime::from_hms_opt(9, 0, 0), |w| parse_clock(w.trim_start_matches("at"))) };
    let hm = |t: NaiveTime| format!("{} {}", chrono::Timelike::minute(&t), chrono::Timelike::hour(&t));
    Some(match head {
        "daily" | "everyday" => format!("{} * * *", hm(clock(rest.iter().find(|w| **w != "at"))?)),
        "weekdays" => format!("{} * * 1-5", hm(clock(rest.iter().find(|w| **w != "at"))?)),
        "weekends" => format!("{} * * 0,6", hm(clock(rest.iter().find(|w| **w != "at"))?)),
        "weekly" => {
            let days = rest.first()?;
            let days: Option<Vec<String>> = days.split(',').map(|d| weekday(d).map(|n| n.to_string())).collect();
            format!("{} * * {}", hm(clock(rest.get(1).filter(|w| **w != "at").or(rest.get(2)))?), days?.join(","))
        }
        "monthly" => {
            let day: u32 = rest.first()?.trim_end_matches(|c: char| c.is_alphabetic()).parse().ok().filter(|d| (1..=31).contains(d))?;
            format!("{} {day} * *", hm(clock(rest.get(1).filter(|w| **w != "at").or(rest.get(2)))?))
        }
        "hourly" => {
            let m: u32 = rest.first().map_or(Some(0), |w| w.trim_start_matches(':').parse().ok())?;
            (m < 60).then(|| format!("{m} * * * *"))?
        }
        _ => return None,
    })
}

/// Read a schedule as an agent or a person writes it. `now` and `tz` resolve relative and clock times.
///
/// `in 20m` · `at 2026-12-01 09:00` · `at 17:30` (today, else tomorrow) · `tomorrow 08:00` · `every 2h` · `every 15 minutes` ·
/// `daily 09:00` · `weekdays 08:30` · `weekly mon,fri 18:00` · `monthly 1 09:00` · `hourly :15` · `cron 0 9 * * 1-5` (or a bare
/// 5-field expression).
pub fn parse_when(when: &str, tz: Tz, now: DateTime<Utc>) -> Result<Schedule, String> {
    let w = when.trim();
    let low = w.to_lowercase();
    let bad = || {
        format!(
            "cannot read the schedule '{w}': use `in 20m`, `at 2026-12-01 09:00`, `at 17:30`, `tomorrow 08:00`, `every 2h`, `daily 09:00`, `weekdays 08:30`, `weekly mon,fri 18:00`, `monthly 1 09:00`, `hourly :15` or `cron <5 fields>`"
        )
    };
    if let Some(d) = low.strip_prefix("in ").or_else(|| low.strip_prefix("after ")).and_then(parse_duration) {
        return Ok(Schedule::At { at: now + d });
    }
    if let Some(rest) = low.strip_prefix("every ") {
        let rest = rest.trim();
        // "every day at 9", "every monday 9:00" read as the readable cron forms
        let as_cron = match rest.split_whitespace().next().unwrap_or("") {
            "day" => readable_cron(&format!("daily {}", rest.trim_start_matches("day").trim())),
            "weekday" => readable_cron(&format!("weekdays {}", rest.trim_start_matches("weekday").trim())),
            d if weekday(d).is_some() => readable_cron(&format!("weekly {}", rest)),
            _ => None,
        };
        if let Some(expr) = as_cron {
            return cron(&expr);
        }
        let d = parse_duration(rest).or_else(|| parse_duration(&format!("1 {rest}"))).ok_or_else(bad)?;
        let secs = d.num_seconds() as u64;
        if secs < MIN_INTERVAL_SECS {
            return Err(format!("every {secs}s is too often: the shortest interval is {MIN_INTERVAL_SECS} s"));
        }
        return Ok(Schedule::Every { secs, anchor: now });
    }
    if let Some(expr) = low.strip_prefix("cron ") {
        return cron(expr);
    }
    if let Some(expr) = readable_cron(&low) {
        return cron(&expr);
    }
    let at_text = low.strip_prefix("at ").or_else(|| low.strip_prefix("on ")).unwrap_or(&low).trim();
    if let Some(t) = at_text.strip_prefix("tomorrow").map(str::trim) {
        let day = now.with_timezone(&tz).date_naive() + Duration::days(1);
        let time = if t.is_empty() { NaiveTime::from_hms_opt(9, 0, 0) } else { parse_clock(t.trim_start_matches("at").trim()) }.ok_or_else(bad)?;
        return local(tz, day.and_time(time)).map(|at| Schedule::At { at });
    }
    if let Some(t) = parse_clock(at_text) {
        let today = now.with_timezone(&tz).date_naive();
        let mut at = local(tz, today.and_time(t))?;
        if at <= now {
            at = local(tz, (today + Duration::days(1)).and_time(t))?;
        }
        return Ok(Schedule::At { at });
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(w) {
        return Ok(Schedule::At { at: dt.with_timezone(&Utc) });
    }
    let text = w.trim_start_matches(|c: char| c.is_alphabetic() || c == ' ').trim();
    for f in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(text, f) {
            return local(tz, n).map(|at| Schedule::At { at });
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return local(tz, d.and_time(NaiveTime::from_hms_opt(9, 0, 0).unwrap())).map(|at| Schedule::At { at });
    }
    // a bare cron expression
    if low.split_whitespace().count() == 5 {
        return cron(&low);
    }
    Err(bad())
}

fn cron(expr: &str) -> Result<Schedule, String> {
    let expr = expr.trim().to_string();
    if expr.split_whitespace().count() != 5 {
        return Err(format!("a cron expression has 5 fields (minute hour day-of-month month day-of-week): '{expr}'"));
    }
    croner::Cron::from_str(&expr).map_err(|e| format!("invalid cron expression '{expr}': {e}"))?;
    Ok(Schedule::Cron { expr })
}

/// A local wall-clock time as an instant (the earlier one when a DST change makes it ambiguous; the next valid one when it is skipped).
fn local(tz: Tz, n: NaiveDateTime) -> Result<DateTime<Utc>, String> {
    match tz.from_local_datetime(&n) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => Ok(t.with_timezone(&Utc)),
        chrono::LocalResult::None => {
            tz.from_local_datetime(&(n + Duration::hours(1))).earliest().map(|t| t.with_timezone(&Utc)).ok_or_else(|| format!("{n} does not exist in {tz}"))
        }
    }
}

/// The first time strictly after `after` that the schedule fires (`None`: never again).
pub fn next_after(s: &Schedule, tz: Tz, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match s {
        Schedule::At { at } => (*at > after).then_some(*at),
        Schedule::Every { secs, anchor } => {
            let secs = (*secs).max(MIN_INTERVAL_SECS) as i64;
            if after < *anchor {
                return Some(*anchor + Duration::seconds(secs));
            }
            let n = (after - *anchor).num_seconds() / secs + 1;
            Some(*anchor + Duration::seconds(n * secs))
        }
        Schedule::Cron { expr } => {
            let c = croner::Cron::from_str(expr).ok()?;
            c.find_next_occurrence(&after.with_timezone(&tz), false).ok().map(|t| t.with_timezone(&Utc))
        }
    }
}

/// `08:00-22:00` → is `at` (in `tz`) inside the window? A window may wrap past midnight (`22:00-06:00`).
pub fn in_active_hours(window: &str, tz: Tz, at: DateTime<Utc>) -> Result<bool, String> {
    let (a, b) = window.split_once('-').ok_or_else(|| format!("active hours are written `08:00-22:00`, not '{window}'"))?;
    let (a, b) = (parse_clock(a).ok_or("bad start of active hours")?, parse_clock(b).ok_or("bad end of active hours")?);
    let t = at.with_timezone(&tz).time();
    Ok(if a <= b { t >= a && t < b } else { t >= a || t < b })
}

/// The next start inside the active window at or after `at` (minute resolution; at most 8 days ahead).
pub fn next_active(window: &str, tz: Tz, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if in_active_hours(window, tz, at).ok()? {
        return Some(at);
    }
    let (start, _) = window.split_once('-')?;
    let start = parse_clock(start)?;
    let day = at.with_timezone(&tz).date_naive();
    (0..9).filter_map(|d| local(tz, (day + Duration::days(d)).and_time(start)).ok()).find(|t| *t > at)
}

impl Routine {
    pub fn tz(&self) -> Tz {
        parse_tz(self.tz.as_deref()).unwrap_or(Tz::UTC)
    }

    /// When the routine should next start, given that it last fired at `from` (or was created then): the schedule's next time,
    /// pushed into the active window, and `None` once it has ended.
    pub fn compute_next(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.state.ended.is_some() || self.max_runs.is_some_and(|m| self.state.runs >= m) {
            return None;
        }
        let tz = self.tz();
        let mut next = next_after(&self.schedule, tz, from)?;
        if let Some(w) = &self.active_hours {
            let pushed = next_active(w, tz, next)?;
            // a recurring routine whose slot falls outside the window waits for its next slot inside it, not for the window's start
            next = match self.schedule {
                Schedule::At { .. } => pushed,
                _ => {
                    let mut t = next;
                    let mut guard = 0;
                    while !in_active_hours(w, tz, t).unwrap_or(true) && guard < 2000 {
                        t = next_after(&self.schedule, tz, t)?;
                        guard += 1;
                    }
                    if guard >= 2000 { pushed } else { t }
                }
            };
        }
        if self.until.is_some_and(|u| next > u) {
            return None;
        }
        Some(next)
    }

    /// A one-line description for lists and prompts.
    pub fn line(&self) -> String {
        let next = match (self.enabled, self.state.next_run, &self.state.ended) {
            (_, _, Some(e)) => format!("ended: {e}"),
            (false, _, _) if self.proposed.is_some() => "proposed (disabled until a person enables it)".into(),
            (false, _, _) => "paused".into(),
            (true, Some(n), _) => format!("next {}", n.with_timezone(&self.tz()).format("%Y-%m-%d %H:%M %Z")),
            (true, None, _) => "no further runs".into(),
        };
        format!("{} \"{}\" [{}] {} — {} — runs {}", self.id, self.name, self.when, next, crate::tokens::truncate_tokens(&self.goal, 30), self.state.runs)
    }

    pub fn record(&mut self, run: RoutineRun) {
        self.history.push(run);
        if self.history.len() > HISTORY_LEN {
            let extra = self.history.len() - HISTORY_LEN;
            self.history.drain(..extra);
        }
    }
}

/// Weekday numbers for day-of-week validation in tests and callers.
pub fn weekday_of(t: DateTime<Utc>, tz: Tz) -> u32 {
    t.with_timezone(&tz).weekday().num_days_from_sunday()
}

/// One task on the board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardTask {
    pub task_id: String,
    pub goal: String,
    pub status: crate::TaskStatus,
    pub mode: Option<crate::ExecMode>,
    /// The step running now (`s3 shell: run the tests`), if any.
    pub doing: Option<String>,
    pub steps_done: usize,
    pub steps_total: usize,
    /// `coordinator`, `researcher`, … or a custom agent's name.
    pub role: Option<String>,
    pub parent_id: Option<String>,
    pub routine_id: Option<String>,
    pub pending_approvals: usize,
    pub cost_usd: f64,
    pub started: DateTime<Utc>,
    pub updated: DateTime<Utc>,
}

/// What the agents are doing, what waits, what just finished and what is scheduled: one snapshot for the board (`GET /v1/board`); its
/// live updates are the event stream (`/v1/events`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub now: DateTime<Utc>,
    pub running: Vec<BoardTask>,
    pub waiting: Vec<BoardTask>,
    pub recent: Vec<BoardTask>,
    pub routines: Vec<Routine>,
    pub queued: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn routine(schedule: Schedule) -> Routine {
        let now = t("2026-11-27T10:00:00Z");
        Routine {
            id: "r1".into(),
            name: "n".into(),
            goal: "g".into(),
            schedule,
            when: "x".into(),
            tz: None,
            enabled: true,
            proposed: None,
            max_runs: None,
            until: None,
            active_hours: None,
            if_changed: None,
            if_ok: None,
            catch_up: true,
            max_cost_usd: None,
            origin: Origin::default(),
            created_at: now,
            updated_at: now,
            state: RoutineState::default(),
            history: vec![],
        }
    }

    #[test]
    fn schedules_are_read_as_people_write_them() {
        let now = t("2026-11-27T10:00:00Z"); // a Friday
        let utc = Tz::UTC;
        assert_eq!(parse_when("in 20m", utc, now).unwrap(), Schedule::At { at: t("2026-11-27T10:20:00Z") });
        assert_eq!(parse_when("in 1h30m", utc, now).unwrap(), Schedule::At { at: t("2026-11-27T11:30:00Z") });
        assert_eq!(parse_when("in 2 hours", utc, now).unwrap(), Schedule::At { at: t("2026-11-27T12:00:00Z") });
        assert_eq!(parse_when("every 2h", utc, now).unwrap(), Schedule::Every { secs: 7200, anchor: now });
        assert_eq!(parse_when("every 15 minutes", utc, now).unwrap(), Schedule::Every { secs: 900, anchor: now });
        assert_eq!(parse_when("every hour", utc, now).unwrap(), Schedule::Every { secs: 3600, anchor: now });
        assert!(parse_when("every 10s", utc, now).unwrap_err().contains("too often"));
        assert_eq!(parse_when("daily 09:00", utc, now).unwrap(), Schedule::Cron { expr: "0 9 * * *".into() });
        assert_eq!(parse_when("every day at 7:30pm", utc, now).unwrap(), Schedule::Cron { expr: "30 19 * * *".into() });
        assert_eq!(parse_when("weekdays 08:30", utc, now).unwrap(), Schedule::Cron { expr: "30 8 * * 1-5".into() });
        assert_eq!(parse_when("weekly mon,fri 18:00", utc, now).unwrap(), Schedule::Cron { expr: "0 18 * * 1,5".into() });
        assert_eq!(parse_when("every monday 9:00", utc, now).unwrap(), Schedule::Cron { expr: "0 9 * * 1".into() });
        assert_eq!(parse_when("monthly 1 09:00", utc, now).unwrap(), Schedule::Cron { expr: "0 9 1 * *".into() });
        assert_eq!(parse_when("hourly :15", utc, now).unwrap(), Schedule::Cron { expr: "15 * * * *".into() });
        assert_eq!(parse_when("cron */5 * * * *", utc, now).unwrap(), Schedule::Cron { expr: "*/5 * * * *".into() });
        assert_eq!(parse_when("0 9 * * 1-5", utc, now).unwrap(), Schedule::Cron { expr: "0 9 * * 1-5".into() });
        assert!(parse_when("cron 99 * * * *", utc, now).is_err());
        assert_eq!(parse_when("at 2026-12-01 09:00", utc, now).unwrap(), Schedule::At { at: t("2026-12-01T09:00:00Z") });
        assert_eq!(parse_when("2026-12-01T09:00:00+01:00", utc, now).unwrap(), Schedule::At { at: t("2026-12-01T08:00:00Z") });
        assert_eq!(parse_when("at 17:30", utc, now).unwrap(), Schedule::At { at: t("2026-11-27T17:30:00Z") }, "later today");
        assert_eq!(parse_when("at 08:00", utc, now).unwrap(), Schedule::At { at: t("2026-11-28T08:00:00Z") }, "passed today: tomorrow");
        assert_eq!(parse_when("tomorrow 8am", utc, now).unwrap(), Schedule::At { at: t("2026-11-28T08:00:00Z") });
        assert!(parse_when("whenever you like", utc, now).is_err());
        // a clock time in another zone
        let berlin: Tz = "Europe/Berlin".parse().unwrap();
        assert_eq!(parse_when("at 2026-12-01 09:00", berlin, now).unwrap(), Schedule::At { at: t("2026-12-01T08:00:00Z") });
    }

    #[test]
    fn next_runs_follow_the_schedule_the_zone_and_dst() {
        let utc = Tz::UTC;
        let every = Schedule::Every { secs: 3600, anchor: t("2026-11-27T10:00:00Z") };
        assert_eq!(next_after(&every, utc, t("2026-11-27T10:00:00Z")), Some(t("2026-11-27T11:00:00Z")));
        assert_eq!(next_after(&every, utc, t("2026-11-27T12:42:00Z")), Some(t("2026-11-27T13:00:00Z")), "no drift: anchored");
        let at = Schedule::At { at: t("2026-11-27T12:00:00Z") };
        assert_eq!(next_after(&at, utc, t("2026-11-27T10:00:00Z")), Some(t("2026-11-27T12:00:00Z")));
        assert_eq!(next_after(&at, utc, t("2026-11-27T12:00:00Z")), None, "a one-shot fires once");
        // 09:00 Berlin, across the end of summer time (2026-10-25): UTC+2 before, UTC+1 after
        let berlin: Tz = "Europe/Berlin".parse().unwrap();
        let daily = Schedule::Cron { expr: "0 9 * * *".into() };
        assert_eq!(next_after(&daily, berlin, t("2026-10-24T08:00:00Z")), Some(t("2026-10-25T08:00:00Z")));
        assert_eq!(next_after(&daily, berlin, t("2026-10-25T08:00:00Z")), Some(t("2026-10-26T08:00:00Z")));
        assert_eq!(next_after(&daily, berlin, t("2026-10-23T06:59:00Z")), Some(t("2026-10-23T07:00:00Z")));
        let weekdays = Schedule::Cron { expr: "30 8 * * 1-5".into() };
        let n = next_after(&weekdays, utc, t("2026-11-27T09:00:00Z")).unwrap(); // Friday after 08:30
        assert_eq!(n, t("2026-11-30T08:30:00Z"), "Monday");
        assert_eq!(weekday_of(n, utc), 1);
    }

    #[test]
    fn limits_and_active_hours_shape_the_next_run() {
        let mut r = routine(Schedule::Every { secs: 3600, anchor: t("2026-11-27T10:00:00Z") });
        assert_eq!(r.compute_next(t("2026-11-27T10:00:00Z")), Some(t("2026-11-27T11:00:00Z")));
        r.max_runs = Some(2);
        r.state.runs = 2;
        assert_eq!(r.compute_next(t("2026-11-27T10:00:00Z")), None, "max runs reached");
        r.max_runs = None;
        r.until = Some(t("2026-11-27T10:30:00Z"));
        assert_eq!(r.compute_next(t("2026-11-27T10:00:00Z")), None, "past the end date");
        r.until = None;
        // hourly, but only 08:00-12:00 UTC: after 11:00 the next slot is tomorrow 08:00
        r.active_hours = Some("08:00-12:00".into());
        assert_eq!(r.compute_next(t("2026-11-27T11:00:00Z")), Some(t("2026-11-28T08:00:00Z")));
        assert!(in_active_hours("22:00-06:00", Tz::UTC, t("2026-11-27T23:30:00Z")).unwrap(), "a window may wrap past midnight");
        assert!(!in_active_hours("22:00-06:00", Tz::UTC, t("2026-11-27T12:00:00Z")).unwrap());
        // a one-shot outside the window waits for the window to open
        let mut once = routine(Schedule::At { at: t("2026-11-27T23:00:00Z") });
        once.active_hours = Some("08:00-20:00".into());
        assert_eq!(once.compute_next(t("2026-11-27T10:00:00Z")), Some(t("2026-11-28T08:00:00Z")));
        assert_eq!(backoff(1), Duration::seconds(30));
        assert_eq!(backoff(9), Duration::seconds(3600));
    }

    #[test]
    fn durations_are_read_strictly() {
        assert_eq!(parse_duration("90s"), Some(Duration::seconds(90)));
        assert_eq!(parse_duration("1 week"), Some(Duration::seconds(604_800)));
        assert_eq!(parse_duration("2 hours and 5 minutes"), Some(Duration::seconds(7500)));
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(parse_duration("5 apples"), None);
    }
}
