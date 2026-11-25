//! Rendering.

use crate::app::{App, Tab};
use dfae_core::*;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table, Tabs};
use serde_json::Value;

const ACCENT: Color = Color::Rgb(86, 212, 221);
const DIM: Color = Color::Rgb(125, 133, 146);
const OK: Color = Color::Rgb(120, 220, 140);
const WARN: Color = Color::Rgb(245, 200, 90);
const BAD: Color = Color::Rgb(240, 100, 110);
const VIOLET: Color = Color::Rgb(190, 150, 245);
const SEL_BG: Color = Color::Rgb(36, 48, 62);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn fmt_tokens(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n >= 1000 {
        format!("{:.2}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn trunc(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        s.to_string()
    } else if w <= 1 {
        "…".into()
    } else {
        format!("{}…", s.chars().take(w - 1).collect::<String>())
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for para in s.lines() {
        let mut cur = String::new();
        for word in para.split(' ') {
            let w = word.chars().count();
            if w > width {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                let chars: Vec<char> = word.chars().collect();
                for ch in chars.chunks(width) {
                    out.push(ch.iter().collect());
                }
                continue;
            }
            if cur.chars().count() + w + usize::from(!cur.is_empty()) > width {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        if !cur.is_empty() || para.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.tick as usize) % SPINNER.len()]
}

fn status_icon(app: &App, s: TaskStatus) -> (String, Color) {
    match s {
        TaskStatus::Completed => ("✔".into(), OK),
        TaskStatus::Failed => ("✘".into(), BAD),
        TaskStatus::Cancelled => ("⊘".into(), DIM),
        TaskStatus::WaitingForApproval => ("⏸".into(), WARN),
        TaskStatus::Created => ("○".into(), DIM),
        _ => (spinner(app).into(), ACCENT),
    }
}

fn mode_badge(m: Option<ExecMode>) -> (&'static str, Color) {
    match m {
        Some(ExecMode::Direct) => ("D", OK),
        Some(ExecMode::Assisted) => ("A", WARN),
        Some(ExecMode::Escalated) => ("E", VIOLET),
        None => ("·", DIM),
    }
}

fn block(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(title.into())
}

fn label(s: &str) -> Span<'static> {
    Span::styled(s.to_string(), Style::new().fg(DIM))
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < 50 || area.height < 12 {
        f.render_widget(Paragraph::new("Terminal too small — please enlarge (min 50x12)").alignment(Alignment::Center), area);
        return;
    }
    let [header, body, input, footer] = Layout::vertical([Constraint::Length(1), Constraint::Min(6), Constraint::Length(3), Constraint::Length(1)]).areas(area);
    draw_header(f, header, app);
    let left_w = (area.width / 4).clamp(26, 40);
    let [left, right] = Layout::horizontal([Constraint::Length(left_w), Constraint::Min(20)]).areas(body);
    draw_tasks(f, left, app);
    let [tabs, content] = Layout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(right);
    draw_tabs(f, tabs, app);
    match app.tab {
        Tab::Timeline => draw_timeline(f, content, app),
        Tab::Plan => draw_plan(f, content, app),
        Tab::Context => draw_cognition(f, content, app),
        Tab::Tokens => draw_tokens(f, content, app),
        Tab::Memory => draw_memory(f, content, app),
        Tab::Tools => draw_tools(f, content, app),
    }
    draw_input(f, input, app);
    draw_footer(f, footer, app);
    if let Some((t, a)) = app.pending_approval() {
        draw_approval(f, area, t, a);
    }
    if app.show_help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" ◆ DFAE ", Style::new().fg(Color::Black).bg(ACCENT).bold()), Span::raw(" ")];
    if let Some(h) = &app.health {
        for c in h["components"].as_array().cloned().unwrap_or_default() {
            let (dot, col) = match c["status"].as_str() {
                Some("ok") => ("●", OK),
                Some("down") => ("●", BAD),
                _ => ("○", DIM),
            };
            spans.push(Span::styled(dot, Style::new().fg(col)));
            spans.push(Span::styled(format!("{} ", c["name"].as_str().unwrap_or("?")), Style::new().fg(DIM)));
        }
    } else {
        spans.push(Span::styled(if app.connected { "checking components…" } else { "connecting…" }, Style::new().fg(DIM)));
    }
    let mut right = Vec::new();
    let pend = app.total_pending_approvals();
    if pend > 0 {
        right.push(Span::styled(
            format!(" ⚠ {pend} approval{} pending (Ctrl+A) ", if pend == 1 { "" } else { "s" }),
            Style::new().fg(Color::Black).bg(WARN).bold(),
        ));
        right.push(Span::raw(" "));
    }
    let (txt, col) = if app.connected { ("online", OK) } else { ("offline", BAD) };
    right.push(Span::styled(format!("● {txt} "), Style::new().fg(col)));
    right.push(Span::styled(trunc(&app.base_url, 28), Style::new().fg(DIM)));
    let rw: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let [l, r] = Layout::horizontal([Constraint::Min(10), Constraint::Length(rw as u16 + 1)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(spans)), l);
    f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), r);
}

fn draw_tasks(f: &mut Frame, area: Rect, app: &App) {
    let w = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .tasks
        .iter()
        .map(|t| {
            let (icon, col) = status_icon(app, t.status);
            let (m, mc) = mode_badge(t.mode);
            let l1 = Line::from(vec![
                Span::styled(format!("{icon} "), Style::new().fg(col)),
                Span::styled(format!("{m} "), Style::new().fg(mc).bold()),
                Span::raw(trunc(&one_line(&t.goal), w.saturating_sub(4))),
            ]);
            let s = &t.ledger;
            let detail = format!(
                "  {} tok · {}{}",
                fmt_tokens(s.generative_tokens),
                if s.baseline_tokens > 0 { format!("{:.0}% saved", s.saved_pct) } else { "…".into() },
                if t.pending_approvals > 0 { " · ⚠ approval" } else { "" }
            );
            ListItem::new(vec![l1, Line::from(Span::styled(trunc(&detail, w + 2), Style::new().fg(DIM)))])
        })
        .collect();
    let title = Line::from(vec![Span::styled(" Tasks ", Style::new().fg(ACCENT).bold()), Span::styled(format!("{} ", app.tasks.len()), Style::new().fg(DIM))]);
    if items.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled("  No tasks yet.", Style::new().fg(DIM))),
                Line::from(Span::styled("  Type a request below.", Style::new().fg(DIM))),
            ])
            .block(block(title)),
            area,
        );
        return;
    }
    let mut st = ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(List::new(items).block(block(title)).highlight_style(Style::new().bg(SEL_BG)).highlight_symbol("▌"), area, &mut st);
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<Line> = Tab::ALL.iter().enumerate().map(|(i, t)| Line::from(format!(" {} {} ", i + 1, t.title()))).collect();
    f.render_widget(
        Tabs::new(titles)
            .select(app.tab.index())
            .style(Style::new().fg(DIM))
            .highlight_style(Style::new().fg(Color::Black).bg(ACCENT).bold())
            .divider("")
            .block(Block::new().borders(Borders::BOTTOM).border_style(Style::new().fg(DIM))),
        area,
    );
}

// ------------------------------------------------------------- timeline ----

fn kv_brief(v: &Value) -> String {
    match v.as_object() {
        Some(m) => m
            .iter()
            .map(|(k, x)| format!("{}={}", k.trim_start_matches("tool:"), x.as_str().map(String::from).unwrap_or_else(|| x.to_string())))
            .collect::<Vec<_>>()
            .join(" "),
        None => v.to_string(),
    }
}

fn args_brief(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            m.iter().map(|(k, x)| format!("{k}={}", x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))).collect::<Vec<_>>().join(" ")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn badge(text: &str, color: Color) -> Span<'static> {
    Span::styled(format!(" {text:<8}"), Style::new().fg(color).bold())
}

pub fn timeline_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let Some(d) = &app.detail else {
        if app.tasks.is_empty() {
            out.extend(welcome());
        } else {
            out.push(Line::from(Span::styled(" loading…", Style::new().fg(DIM))));
        }
        return out;
    };
    out.push(Line::from(vec![
        Span::styled(" you ", Style::new().fg(Color::Black).bg(VIOLET).bold()),
        Span::raw(" "),
        Span::styled(trunc(&one_line(&d.goal), width.saturating_sub(8)), Style::new().bold()),
    ]));
    out.push(Line::from(""));
    let t0 = app.events.first().map(|e| e.ts);
    let body_w = width.saturating_sub(18).max(20);
    for e in &app.events {
        let rel = t0.map(|t| (e.ts - t).num_milliseconds() as f64 / 1000.0).unwrap_or(0.0);
        let stamp = Span::styled(format!("{rel:>6.2}s"), Style::new().fg(DIM));
        let mut push = |b: Span<'static>, text: String, style: Style| {
            out.push(Line::from(vec![stamp.clone(), b, Span::styled(trunc(&text, body_w), style)]));
        };
        match &e.kind {
            EventKind::TaskCreated { .. } | EventKind::Ledger { .. } | EventKind::Final { .. } => {}
            EventKind::Status { status } => push(badge("status", DIM), status.to_string(), Style::new().fg(DIM)),
            EventKind::Extraction { entities, backend, latency_ms } => {
                let ents = entities
                    .iter()
                    .filter(|e| e.label != "action")
                    .take(6)
                    .map(|e| format!("{}={}", e.label.replace(' ', "_"), e.text))
                    .collect::<Vec<_>>()
                    .join(", ");
                push(
                    badge("extract", ACCENT),
                    format!("{} entities via {backend} ({latency_ms}ms){}", entities.len(), if ents.is_empty() { String::new() } else { format!(" · {ents}") }),
                    Style::new(),
                );
            }
            EventKind::Retrieval { items } => push(
                badge("memory", ACCENT),
                if items.is_empty() {
                    "nothing relevant".into()
                } else {
                    format!("{} relevant: {}", items.len(), items.iter().map(|i| i.preview.clone()).collect::<Vec<_>>().join(" | "))
                },
                Style::new(),
            ),
            EventKind::Decision { record } => push(
                badge("decide", VIOLET),
                format!(
                    "{} · {} ({}ms{}) {}",
                    record.kind,
                    record.provider,
                    record.latency_ms,
                    if record.tokens_in > 0 { format!(", {}→{} tok", record.tokens_in, record.tokens_out) } else { String::new() },
                    kv_brief(&record.answers)
                ),
                Style::new(),
            ),
            EventKind::Mode { mode, reason, skill_id } => {
                let (n, c) = match mode {
                    ExecMode::Direct => ("DIRECT", OK),
                    ExecMode::Assisted => ("ASSISTED", WARN),
                    ExecMode::Escalated => ("ESCALATED", VIOLET),
                };
                out.push(Line::from(vec![
                    stamp.clone(),
                    badge("mode", c),
                    Span::styled(format!(" {n} "), Style::new().fg(Color::Black).bg(c).bold()),
                    Span::styled(
                        format!(" {}{}", trunc(reason, body_w.saturating_sub(14)), if skill_id.is_some() { " · skill" } else { "" }),
                        Style::new().fg(DIM),
                    ),
                ]));
            }
            EventKind::PlanCreated { plan } => push(
                badge("plan", ACCENT),
                format!(
                    "{} step{} ({:?}) {}",
                    plan.steps.len(),
                    if plan.steps.len() == 1 { "" } else { "s" },
                    plan.source,
                    plan.steps.iter().map(|s| s.tool.clone().unwrap_or_else(|| "reason".into())).collect::<Vec<_>>().join(" → ")
                ),
                Style::new(),
            ),
            EventKind::PlanRevised { plan, reason } => push(badge("replan", WARN), format!("rev {} — {reason}", plan.revision), Style::new().fg(WARN)),
            EventKind::StepUpdate { step_id, status, tool, summary, error, attempts } => {
                let (icon, col) = match status {
                    StepStatus::Running => (spinner(app), ACCENT),
                    StepStatus::Succeeded => ("✔", OK),
                    StepStatus::Failed => ("✘", BAD),
                    StepStatus::WaitingApproval => ("⏸", WARN),
                    StepStatus::Cancelled | StepStatus::Skipped => ("⊘", DIM),
                    StepStatus::Pending => ("↻", WARN),
                };
                let text = match (summary, error) {
                    (_, Some(er)) => format!("{step_id} {} failed: {er}", tool.as_deref().unwrap_or("reasoning")),
                    (Some(s), _) => format!("{step_id} {} → {}", tool.as_deref().unwrap_or("reasoning"), one_line(s)),
                    _ => format!(
                        "{step_id} {}{}",
                        tool.as_deref().unwrap_or("reasoning"),
                        if *attempts > 1 { format!(" (attempt {attempts})") } else { String::new() }
                    ),
                };
                out.push(Line::from(vec![
                    stamp.clone(),
                    Span::styled(format!(" {icon} step   "), Style::new().fg(col).bold()),
                    Span::styled(trunc(&text, body_w), Style::new()),
                ]));
            }
            EventKind::ApprovalRequested { approval } => {
                push(badge("approve", WARN), format!("{} {} — {}", approval.tool, args_brief(&approval.args), approval.reason), Style::new().fg(WARN))
            }
            EventKind::ApprovalResolved { status, .. } => {
                push(badge("approve", if *status == ApprovalStatus::Approved { OK } else { BAD }), format!("{status:?}").to_lowercase(), Style::new())
            }
            EventKind::Model { component, model, purpose, prompt_tokens, completion_tokens, latency_ms } => push(
                badge(if *component == Component::Strong { "strong" } else { "slm" }, VIOLET),
                format!("{purpose} · {model} · {prompt_tokens}→{completion_tokens} tok · {latency_ms}ms"),
                Style::new().fg(DIM),
            ),
            EventKind::Verification { result } => {
                let (n, c) = match result.status {
                    VerificationStatus::Verified => ("VERIFIED", OK),
                    VerificationStatus::Partial => ("PARTIAL", WARN),
                    VerificationStatus::Unverified => ("UNVERIFIED", WARN),
                    VerificationStatus::Failed => ("FAILED", BAD),
                };
                let passed = result.checks.iter().filter(|c| c.status == CheckStatus::Passed).count();
                out.push(Line::from(vec![
                    stamp.clone(),
                    badge("verify", c),
                    Span::styled(format!(" {n} "), Style::new().fg(Color::Black).bg(c).bold()),
                    Span::styled(
                        format!(
                            " {passed}/{} checks{}",
                            result.checks.len(),
                            if result.unverified_claims.is_empty() {
                                String::new()
                            } else {
                                format!(" · unverified: {}", result.unverified_claims.join(", "))
                            }
                        ),
                        Style::new().fg(DIM),
                    ),
                ]));
            }
            EventKind::Warning { message } => push(badge("warn", WARN), message.clone(), Style::new().fg(WARN)),
        }
    }
    if let Some(ans) = &d.final_answer {
        out.push(Line::from(""));
        let (col, title) = match d.status {
            TaskStatus::Completed => (OK, " answer "),
            TaskStatus::Cancelled => (DIM, " cancelled "),
            _ => (BAD, " could not complete "),
        };
        out.push(Line::from(vec![
            Span::styled("┌─", Style::new().fg(col)),
            Span::styled(title, Style::new().fg(col).bold()),
            Span::styled("─".repeat(width.saturating_sub(title.len() + 4).min(60)), Style::new().fg(col)),
        ]));
        for l in wrap_text(ans, width.saturating_sub(4)) {
            out.push(Line::from(vec![Span::styled("│ ", Style::new().fg(col)), Span::raw(l)]));
        }
        let s = d.ledger.summary();
        out.push(Line::from(vec![
            Span::styled("└─ ", Style::new().fg(col)),
            Span::styled(
                format!(
                    "{} generative tokens vs ~{} for a naive LLM agent ({:.0}% saved)",
                    fmt_tokens(s.generative_tokens),
                    fmt_tokens(s.baseline_tokens),
                    s.saved_pct
                ),
                Style::new().fg(DIM),
            ),
        ]));
    } else if !d.status.is_terminal() {
        out.push(Line::from(vec![
            Span::styled(format!(" {} ", spinner(app)), Style::new().fg(ACCENT)),
            Span::styled(d.status.to_string().replace('_', " "), Style::new().fg(DIM)),
        ]));
    }
    out
}

fn welcome() -> Vec<Line<'static>> {
    let ds = |s: &str| Span::styled(s.to_string(), Style::new().fg(DIM));
    let d = |s: &str| Line::from(ds(s));
    vec![
        Line::from(""),
        Line::from(Span::styled("  Decision-First Agentic Engine", Style::new().fg(ACCENT).bold())),
        d("  The runtime is the agent; models are specialised services inside it."),
        Line::from(""),
        Line::from(vec![Span::styled("  D ", Style::new().fg(OK).bold()), ds("direct    decision model + code + tools. No generative model.")]),
        Line::from(vec![Span::styled("  A ", Style::new().fg(WARN).bold()), ds("assisted  small language model plans / reasons / writes.")]),
        Line::from(vec![Span::styled("  E ", Style::new().fg(VIOLET).bold()), ds("escalated stronger model, only when needed.")]),
        Line::from(""),
        d("  Try:  what is 12 * (3 + 4)?"),
        d("        list the files in ./  and what time is it"),
        d("        write 'hello' to notes/hello.txt        (asks for approval)"),
        d("        remember that the staging db is db-staging-01"),
        d("  F1 for help · Tab switches views · F2 cycles execution mode"),
    ]
}

fn draw_timeline(f: &mut Frame, area: Rect, app: &App) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines = timeline_lines(app, area.width.saturating_sub(2) as usize);
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = app.scroll.min(max_scroll);
    let start = total.saturating_sub(inner_h + scroll);
    let visible: Vec<Line> = lines.into_iter().skip(start).take(inner_h).collect();
    let mut title = vec![Span::styled(" Timeline ", Style::new().fg(ACCENT).bold())];
    if scroll > 0 {
        title.push(Span::styled(format!("↑{scroll} (End to follow) "), Style::new().fg(WARN)));
    }
    f.render_widget(Paragraph::new(visible).block(block(Line::from(title))), area);
}

// ----------------------------------------------------------------- plan ----

fn waves(plan: &Plan) -> Vec<usize> {
    let mut level = vec![1usize; plan.steps.len()];
    for _ in 0..plan.steps.len() {
        for (i, s) in plan.steps.iter().enumerate() {
            let l = s.depends_on.iter().filter_map(|d| plan.steps.iter().position(|x| &x.id == d)).map(|j| level[j] + 1).max().unwrap_or(1);
            level[i] = l;
        }
    }
    level
}

fn draw_plan(f: &mut Frame, area: Rect, app: &App) {
    let Some(plan) = app.detail.as_ref().and_then(|d| d.plan.as_ref()) else {
        f.render_widget(Paragraph::new(Line::from(Span::styled(" no plan yet", Style::new().fg(DIM)))).block(block(" Plan ")), area);
        return;
    };
    let [top, bottom] = Layout::vertical([Constraint::Percentage(50), Constraint::Min(4)]).areas(area);
    let lv = waves(plan);
    let sel = app.plan_sel.min(plan.steps.len().saturating_sub(1));
    let rows: Vec<Row> = plan
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (icon, col) = match s.status {
                StepStatus::Running => (spinner(app), ACCENT),
                StepStatus::Succeeded => ("✔", OK),
                StepStatus::Failed => ("✘", BAD),
                StepStatus::WaitingApproval => ("⏸", WARN),
                StepStatus::Pending => ("○", DIM),
                _ => ("⊘", DIM),
            };
            let dur = match (s.started_at, s.finished_at) {
                (Some(a), Some(b)) => format!("{}ms", (b - a).num_milliseconds()),
                _ => String::new(),
            };
            let row = Row::new(vec![
                Cell::from(Span::styled(icon, Style::new().fg(col))),
                Cell::from(s.id.clone()),
                Cell::from(format!("W{}", lv[i])),
                Cell::from(Span::styled(
                    s.tool.clone().unwrap_or_else(|| "reasoning".into()),
                    Style::new().fg(if s.kind == StepKind::Tool { ACCENT } else { VIOLET }),
                )),
                Cell::from(trunc(&one_line(&args_brief(&s.args)), 40)),
                Cell::from(s.depends_on.join(",")),
                Cell::from(if s.attempts > 1 { format!("×{}", s.attempts) } else { String::new() }),
                Cell::from(dur),
            ]);
            if i == sel { row.style(Style::new().bg(SEL_BG)) } else { row }
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(7),
        Constraint::Length(3),
        Constraint::Length(14),
        Constraint::Min(16),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(8),
    ];
    let title = Line::from(vec![
        Span::styled(" Plan ", Style::new().fg(ACCENT).bold()),
        Span::styled(format!("{:?} · rev {} · W = parallel wave · Ctrl+↑/↓ select ", plan.source, plan.revision), Style::new().fg(DIM)),
    ]);
    f.render_widget(
        Table::new(rows, widths).header(Row::new(["", "id", "W", "tool", "args", "after", "try", "time"]).style(Style::new().fg(DIM))).block(block(title)),
        top,
    );

    let mut lines: Vec<Line> = Vec::new();
    if let Some(s) = plan.steps.get(sel) {
        lines.push(Line::from(vec![label("step "), Span::styled(s.id.clone(), Style::new().bold()), label("  "), Span::raw(trunc(&s.description, 80))]));
        if s.kind == StepKind::Tool {
            lines.push(Line::from(vec![label("args "), Span::raw(trunc(&s.args.to_string(), 120))]));
        }
        if !s.checks.is_empty() {
            lines.push(Line::from(vec![label("checks "), Span::raw(s.checks.iter().map(|c| c.describe()).collect::<Vec<_>>().join("; "))]));
        }
        for e in &s.evidence {
            lines.push(Line::from(vec![Span::styled("✓ ", Style::new().fg(OK)), Span::raw(trunc(e, 110))]));
        }
        if let Some(er) = &s.error {
            lines.push(Line::from(vec![Span::styled("✘ ", Style::new().fg(BAD)), Span::styled(trunc(er, 110), Style::new().fg(BAD))]));
        }
        if let Some(o) = &s.output {
            lines.push(Line::from(label("output")));
            for l in o.lines().take((bottom.height as usize).saturating_sub(lines.len() + 3)) {
                lines.push(Line::from(Span::raw(format!("  {}", trunc(l, bottom.width.saturating_sub(6) as usize)))));
            }
        }
    }
    f.render_widget(Paragraph::new(lines).block(block(" Step detail ")), bottom);
}

// ------------------------------------------------------------ cognition ----

fn draw_cognition(f: &mut Frame, area: Rect, app: &App) {
    let Some(d) = &app.detail else {
        f.render_widget(Paragraph::new(Line::from(Span::styled(" select a task", Style::new().fg(DIM)))).block(block(" Cognition ")), area);
        return;
    };
    let [head, ents, dec] = Layout::vertical([Constraint::Length(4), Constraint::Percentage(40), Constraint::Min(4)]).areas(area);
    let mut l = Vec::new();
    if let Some(r) = &d.route {
        l.push(Line::from(vec![
            label("route  "),
            Span::raw(
                format!(
                    "{} · complexity {:.2} · risk {:?} · needs_tools {} · confidence {:.2}",
                    r.task_type, r.complexity, r.risk, r.needs_tools, r.confidence
                )
                .to_lowercase(),
            ),
        ]));
    }
    if let Some(m) = d.mode {
        l.push(Line::from(vec![
            label("mode   "),
            Span::styled(format!("{m:?}").to_uppercase(), Style::new().fg(mode_badge(Some(m)).1).bold()),
            Span::raw(format!("  {}", d.mode_reason.clone().unwrap_or_default())),
        ]));
    }
    l.push(Line::from(vec![
        label("budget "),
        Span::raw(format!(
            "model calls {}/{} · tool calls {}/{} · replans {}/{}",
            d.usage.model_calls, d.budget.max_model_calls, d.usage.tool_calls, d.budget.max_tool_calls, d.usage.replans, d.budget.max_replans
        )),
        if d.tainted { Span::styled("  ⚠ tainted context", Style::new().fg(WARN)) } else { Span::raw("") },
    ]));
    f.render_widget(Paragraph::new(l).block(block(" Routing ")), head);

    let mut rows: Vec<Row> = d
        .entities
        .iter()
        .map(|e| {
            Row::new(vec![
                Cell::from(Span::styled(e.label.clone(), Style::new().fg(ACCENT))),
                Cell::from(trunc(&e.text, 40)),
                Cell::from(format!("{:.2}", e.score)),
                Cell::from(e.source.clone()),
            ])
        })
        .collect();
    for r in &d.retrieved {
        rows.push(Row::new(vec![
            Cell::from(Span::styled(format!("memory:{}", r.kind), Style::new().fg(VIOLET))),
            Cell::from(trunc(&r.preview, 40)),
            Cell::from(format!("{:.2}", r.score)),
            Cell::from("recall"),
        ]));
    }
    f.render_widget(
        Table::new(rows, [Constraint::Length(16), Constraint::Min(20), Constraint::Length(6), Constraint::Length(8)])
            .header(Row::new(["label", "text", "score", "src"]).style(Style::new().fg(DIM)))
            .block(block(" Extracted entities & recalled memory ")),
        ents,
    );

    let rows: Vec<Row> = d
        .decisions
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.kind.clone()),
                Cell::from(Span::styled(r.provider.clone(), Style::new().fg(if r.provider.starts_with("jev") { VIOLET } else { DIM }))),
                Cell::from(trunc(&kv_brief(&r.answers), 70)),
                Cell::from(format!("{}ms", r.latency_ms)),
                Cell::from(if r.tokens_in > 0 { format!("{}→{}", r.tokens_in, r.tokens_out) } else { "-".into() }),
            ])
        })
        .collect();
    f.render_widget(
        Table::new(rows, [Constraint::Length(18), Constraint::Length(15), Constraint::Min(20), Constraint::Length(7), Constraint::Length(9)])
            .header(Row::new(["decision", "provider", "answers", "time", "tok"]).style(Style::new().fg(DIM)))
            .block(block(" Decisions ")),
        dec,
    );
}

// --------------------------------------------------------------- tokens ----

fn bar(label_s: &str, value: u64, max: u64, width: usize, color: Color) -> Line<'static> {
    let w = width.saturating_sub(32).max(4);
    let n = if max == 0 { 0 } else { ((value as f64 / max as f64) * w as f64).round() as usize };
    let n = if value > 0 { n.max(1) } else { 0 };
    Line::from(vec![
        Span::styled(format!(" {label_s:<26}"), Style::new().fg(DIM)),
        Span::styled("█".repeat(n), Style::new().fg(color)),
        Span::raw(format!(" {}", fmt_tokens(value))),
    ])
}

fn draw_tokens(f: &mut Frame, area: Rect, app: &App) {
    let [top, mid, bot] = Layout::vertical([Constraint::Length(9), Constraint::Min(4), Constraint::Length(7)]).areas(area);
    let s = app.detail.as_ref().map(|d| d.ledger.summary()).unwrap_or_default();
    let w = top.width.saturating_sub(2) as usize;
    let max = s.baseline_tokens.max(s.generative_tokens).max(1);
    let lines = vec![
        bar("naive LLM agent (est.)", s.baseline_tokens, max, w, BAD),
        bar("generative: SLM", s.slm_tokens, max, w, WARN),
        bar("generative: strong model", s.strong_tokens, max, w, VIOLET),
        bar("decision model (Jev)", s.decision_tokens, max, w, ACCENT),
        Line::from(vec![
            label(" calls   "),
            Span::raw(format!(
                "slm {} · strong {} · jev {} · extractor {} · cache hits {}",
                s.slm_calls, s.strong_calls, s.jev_calls, s.gliner_calls, s.cache_hits
            )),
        ]),
    ];
    let [txt, gauge] = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).areas(top);
    f.render_widget(Paragraph::new(lines).block(block(" This task ")), txt);
    f.render_widget(
        Gauge::default()
            .block(Block::new().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM).border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)))
            .gauge_style(Style::new().fg(OK).bg(Color::Rgb(40, 44, 52)))
            .ratio((s.saved_pct as f64 / 100.0).clamp(0.0, 1.0))
            .label(format!("{:.0}% generative tokens saved vs a naive agent (estimate)", s.saved_pct)),
        gauge,
    );

    let rows: Vec<Row> = app
        .detail
        .as_ref()
        .map(|d| {
            d.ledger
                .entries
                .iter()
                .map(|e| {
                    Row::new(vec![
                        Cell::from(Span::styled(
                            format!("{:?}", e.component).to_lowercase(),
                            Style::new().fg(match e.component {
                                Component::Slm => WARN,
                                Component::Strong => VIOLET,
                                Component::Jev => ACCENT,
                                Component::Gliner => OK,
                            }),
                        )),
                        Cell::from(e.purpose.clone()),
                        Cell::from(trunc(&e.model, 26)),
                        Cell::from(format!("{}", e.prompt_tokens)),
                        Cell::from(format!("{}", e.completion_tokens)),
                        Cell::from(format!("{}ms{}", e.latency_ms, if e.cached { " ⚡" } else { "" })),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    f.render_widget(
        Table::new(
            rows,
            [Constraint::Length(8), Constraint::Length(14), Constraint::Min(14), Constraint::Length(8), Constraint::Length(8), Constraint::Length(10)],
        )
        .header(Row::new(["who", "purpose", "model", "in", "out", "time"]).style(Style::new().fg(DIM)))
        .block(block(" Calls ")),
        mid,
    );

    let g = |k: &str| app.stats.as_ref().map(|s| s[k].as_u64().unwrap_or(0)).unwrap_or(0);
    let t = |k: &str| app.stats.as_ref().map(|s| s["totals"][k].as_u64().unwrap_or(0)).unwrap_or(0);
    let pct = app.stats.as_ref().map(|s| s["totals"]["saved_pct"].as_f64().unwrap_or(0.0)).unwrap_or(0.0);
    let all = vec![
        Line::from(vec![
            label(" tasks  "),
            Span::raw(format!("{} total · {} completed · {} failed · {} verified", g("tasks"), g("completed"), g("failed"), g("verified"))),
        ]),
        Line::from(vec![
            label(" modes  "),
            Span::styled(format!("direct {}", g("direct")), Style::new().fg(OK)),
            Span::raw(" · "),
            Span::styled(format!("assisted {}", g("assisted")), Style::new().fg(WARN)),
            Span::raw(" · "),
            Span::styled(format!("escalated {}", g("escalated")), Style::new().fg(VIOLET)),
        ]),
        Line::from(vec![
            label(" tokens "),
            Span::raw(format!(
                "generative {} (slm {} / strong {}) · decision {} · naive baseline {}",
                fmt_tokens(t("generative_tokens")),
                fmt_tokens(t("slm_tokens")),
                fmt_tokens(t("strong_tokens")),
                fmt_tokens(t("decision_tokens")),
                fmt_tokens(t("baseline_tokens"))
            )),
        ]),
        Line::from(vec![label(" saved  "), Span::styled(format!("{} tokens ({pct:.0}%)", fmt_tokens(t("saved_tokens"))), Style::new().fg(OK).bold())]),
    ];
    f.render_widget(Paragraph::new(all).block(block(" All tasks ")), bot);
}

// --------------------------------------------------------------- memory ----

fn draw_memory(f: &mut Frame, area: Rect, app: &App) {
    let [top, bot] = Layout::vertical([Constraint::Min(4), Constraint::Length(5)]).areas(area);
    let mut lines = vec![];
    if app.memory_query.is_empty() {
        lines.push(Line::from(Span::styled(" /memory <query> to search · /remember <fact> to store", Style::new().fg(DIM))));
    } else {
        lines.push(Line::from(vec![label(" query  "), Span::styled(app.memory_query.clone(), Style::new().bold())]));
        if app.memory_hits.is_empty() {
            lines.push(Line::from(Span::styled(" no results", Style::new().fg(DIM))));
        }
    }
    for h in &app.memory_hits {
        let kind = h["item"]["kind"].as_str().unwrap_or("?");
        let col = match kind {
            "procedural" => VIOLET,
            "episodic" => WARN,
            _ => ACCENT,
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" [{kind}] "), Style::new().fg(col)),
            Span::raw(trunc(&one_line(h["item"]["text"].as_str().unwrap_or("")), area.width.saturating_sub(20) as usize)),
            Span::styled(format!("  {:.2}", h["similarity"].as_f64().unwrap_or(0.0)), Style::new().fg(DIM)),
        ]));
    }
    f.render_widget(Paragraph::new(lines).block(block(" Memory search ")), top);
    let m = |k: &str| app.stats.as_ref().map(|s| s["memory"][k].as_u64().unwrap_or(0)).unwrap_or(0);
    let backend = app.stats.as_ref().and_then(|s| s["memory"]["backend"].as_str().map(String::from)).unwrap_or_else(|| "?".into());
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![label(" backend    "), Span::raw(backend)]),
            Line::from(vec![label(" episodic   "), Span::raw(format!("{}  (task outcomes)", m("episodic")))]),
            Line::from(vec![label(" semantic   "), Span::raw(format!("{}  (validated facts)", m("semantic")))]),
            Line::from(vec![label(" procedural "), Span::raw(format!("{}  (skills learned from verified plans)", m("procedural")))]),
        ])
        .block(block(" Stores ")),
        bot,
    );
}

fn draw_tools(f: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Row> = app
        .info
        .as_ref()
        .and_then(|i| i["tools"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|t| {
            let risk = t["risk"].as_str().unwrap_or("low");
            let col = match risk {
                "high" => BAD,
                "moderate" => WARN,
                _ => OK,
            };
            Row::new(vec![
                Cell::from(Span::styled(t["name"].as_str().unwrap_or("").to_string(), Style::new().fg(ACCENT))),
                Cell::from(Span::styled(risk.to_string(), Style::new().fg(col))),
                Cell::from(if t["side_effects"].as_bool().unwrap_or(false) { "writes" } else { "read-only" }),
                Cell::from(trunc(t["signature"].as_str().unwrap_or(""), area.width.saturating_sub(40) as usize)),
            ])
        })
        .collect();
    f.render_widget(
        Table::new(rows, [Constraint::Length(14), Constraint::Length(9), Constraint::Length(10), Constraint::Min(20)])
            .header(Row::new(["tool", "risk", "effect", "signature"]).style(Style::new().fg(DIM)))
            .block(block(" Tool registry · the model only sees the tools the decision layer selects ")),
        area,
    );
}

// ----------------------------------------------------------- chrome -----

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let (m, mc) = match app.mode {
        ModeRequest::Auto => ("auto", ACCENT),
        ModeRequest::Direct => ("direct", OK),
        ModeRequest::Assisted => ("assisted", WARN),
        ModeRequest::Escalated => ("escalated", VIOLET),
    };
    let title = Line::from(vec![
        Span::styled(" Ask ", Style::new().fg(ACCENT).bold()),
        Span::styled("mode:", Style::new().fg(DIM)),
        Span::styled(format!("{m} "), Style::new().fg(mc).bold()),
        Span::styled(format!("session:{} ", dfae_core::short(&app.session_id)), Style::new().fg(DIM)),
        if app.auto_approve { Span::styled("AUTO-APPROVE ", Style::new().fg(Color::Black).bg(BAD).bold()) } else { Span::raw("") },
    ]);
    let inner_w = area.width.saturating_sub(5) as usize;
    let chars: Vec<char> = app.input.text.chars().collect();
    let start = app.input.cursor.saturating_sub(inner_w.saturating_sub(1));
    let shown: String = chars.iter().skip(start).take(inner_w).collect();
    f.render_widget(Paragraph::new(Line::from(vec![Span::styled("› ", Style::new().fg(ACCENT).bold()), Span::raw(shown)])).block(block(title)), area);
    if app.pending_approval().is_none() && !app.show_help {
        f.set_cursor_position((area.x + 3 + (app.input.cursor - start) as u16, area.y + 1));
    }
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    if let Some((msg, err)) = app.flash_text() {
        f.render_widget(Paragraph::new(Span::styled(format!(" {msg}"), Style::new().fg(if err { BAD } else { OK }))), area);
        return;
    }
    let hints = " ⏎ send · Tab view · ↑↓ task · PgUp/PgDn scroll · F2 mode · ^X cancel · F1 help · ^C quit";
    let saved = app.stats.as_ref().map(|s| {
        (
            s["totals"]["saved_pct"].as_f64().unwrap_or(0.0),
            s["totals"]["generative_tokens"].as_u64().unwrap_or(0),
            s["totals"]["baseline_tokens"].as_u64().unwrap_or(0),
        )
    });
    let right = saved.filter(|(_, _, b)| *b > 0).map(|(p, g, b)| format!("Σ {p:.0}% saved ({} vs ~{}) ", fmt_tokens(g), fmt_tokens(b))).unwrap_or_default();
    let [l, r] = Layout::horizontal([Constraint::Min(10), Constraint::Length(right.chars().count() as u16 + 1)]).areas(area);
    f.render_widget(Paragraph::new(Span::styled(trunc(hints, l.width as usize), Style::new().fg(DIM))), l);
    f.render_widget(Paragraph::new(Span::styled(right, Style::new().fg(OK))).alignment(Alignment::Right), r);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn draw_approval(f: &mut Frame, area: Rect, t: &TaskRecord, a: &Approval) {
    let r = centered(area, 78, 13);
    f.render_widget(Clear, r);
    let rc = match a.risk {
        Risk::High => BAD,
        Risk::Moderate => WARN,
        Risk::Low => OK,
    };
    let mut lines = vec![
        Line::from(vec![label("task    "), Span::raw(trunc(&one_line(&t.goal), 62))]),
        Line::from(vec![
            label("tool    "),
            Span::styled(a.tool.clone(), Style::new().fg(ACCENT).bold()),
            Span::styled(format!("   risk {:?}", a.risk).to_lowercase(), Style::new().fg(rc)),
        ]),
    ];
    for l in wrap_text(&args_brief(&a.args), 66).into_iter().take(4) {
        lines.push(Line::from(vec![label("args    "), Span::raw(l)]));
    }
    lines.push(Line::from(vec![label("why     "), Span::raw(trunc(&a.reason, 62))]));
    lines.push(Line::from(vec![
        label("bound   "),
        Span::styled(format!("sha256:{}…  (approval covers exactly this action)", &a.args_hash[..a.args_hash.len().min(12)]), Style::new().fg(DIM)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [y] approve  ", Style::new().fg(Color::Black).bg(OK).bold()),
        Span::raw("   "),
        Span::styled("  [n] deny  ", Style::new().fg(Color::Black).bg(BAD).bold()),
    ]));
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Double)
                .border_style(Style::new().fg(WARN))
                .title(Span::styled(" ⚠ Approval required ", Style::new().fg(WARN).bold())),
        ),
        r,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let r = centered(area, 74, 22);
    f.render_widget(Clear, r);
    let k = |a: &str, b: &str| Line::from(vec![Span::styled(format!("  {a:<16}"), Style::new().fg(ACCENT).bold()), Span::raw(b.to_string())]);
    let lines = vec![
        k("Enter", "submit the request"),
        k("Tab / Shift+Tab", "switch view (Timeline · Plan · Cognition · Tokens · Memory · Tools)"),
        k("↑ / ↓", "select task (input empty) · recall history (input non-empty)"),
        k("PgUp / PgDn / End", "scroll the timeline · jump to latest"),
        k("Ctrl+↑ / Ctrl+↓", "select a step in the Plan view"),
        k("F2", "cycle execution mode: auto → direct → assisted → escalated"),
        k("y / n", "approve / deny the pending approval (input empty)"),
        k("Ctrl+A", "jump to a task waiting for approval"),
        k("Ctrl+X", "cancel the selected task"),
        k("Ctrl+U / Ctrl+W", "clear line / delete word"),
        Line::from(""),
        k("/mode <m>", "auto | direct | assisted | escalated"),
        k("/memory <q>", "search long-term memory"),
        k("/remember <fact>", "store a fact"),
        k("/new", "start a new session (fresh conversational context)"),
        k("/autoapprove on", "skip approvals (benchmarks only; server must allow)"),
        k("/tools  /cancel  /quit", ""),
        Line::from(""),
        Line::from(Span::styled("  press any key to close", Style::new().fg(DIM))),
    ];
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(Span::styled(" Help ", Style::new().fg(ACCENT).bold())),
        ),
        r,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::TaskRow;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn sample() -> App {
        let mut app = App::new("http://dfae:8080");
        let mut rec = TaskRecord::new(TaskRequest { goal: "write 'hi' to a.txt".into(), ..Default::default() }, Budget::default());
        rec.status = TaskStatus::WaitingForApproval;
        rec.mode = Some(ExecMode::Direct);
        rec.entities.push(Entity { label: "file path".into(), text: "a.txt".into(), start: None, end: None, score: 1.0, source: "rules".into() });
        let mut s = Step::tool("s1", "fs_write", "write", serde_json::json!({"path": "a.txt", "content": "hi"}));
        s.status = StepStatus::WaitingApproval;
        rec.plan = Some(Plan { goal: "g".into(), steps: vec![s, Step::reasoning("s2", "explain")], source: PlanSource::Direct, revision: 0 });
        let now = chrono::Utc::now();
        rec.approvals.push(Approval {
            id: "ap1".into(),
            step_id: "s1".into(),
            tool: "fs_write".into(),
            args: serde_json::json!({"path": "a.txt"}),
            args_hash: "abcdef0123456789".into(),
            reason: "writes a file".into(),
            risk: Risk::Moderate,
            status: ApprovalStatus::Pending,
            created_at: now,
            expires_at: now,
            decided_by: None,
        });
        rec.final_answer = Some("Done. wrote 2 bytes".into());
        app.tasks = vec![TaskRow {
            task_id: rec.task_id.clone(),
            goal: rec.goal.clone(),
            status: rec.status,
            mode: rec.mode,
            verification: None,
            pending_approvals: 1,
            ledger: Default::default(),
            session_id: rec.session_id.clone(),
        }];
        app.events = vec![Event {
            task_id: rec.task_id.clone(),
            seq: 0,
            ts: now,
            kind: EventKind::Mode { mode: ExecMode::Direct, reason: "deterministic binding".into(), skill_id: None },
        }];
        app.detail = Some(rec);
        app.health = Some(
            serde_json::json!({"components": [{"name": "memory", "status": "ok"}, {"name": "slm", "status": "off"}, {"name": "extractor", "status": "down"}]}),
        );
        app.info = Some(
            serde_json::json!({"tools": [{"name": "fs_write", "risk": "moderate", "side_effects": true, "signature": "fs_write(path:str*, content:str*)"}]}),
        );
        app.connected = true;
        app
    }

    #[test]
    fn every_tab_renders_at_several_sizes_without_panicking() {
        let mut app = sample();
        for (w, h) in [(160, 48), (100, 30), (80, 24), (60, 14), (50, 12), (30, 8)] {
            for tab in Tab::ALL {
                app.tab = tab;
                let out = render(&app, w, h);
                assert!(!out.is_empty());
            }
        }
    }

    #[test]
    fn welcome_screen_and_chrome() {
        let mut app = App::new("http://x:8080");
        app.connected = true;
        let out = render(&app, 120, 40);
        assert!(out.contains("DFAE"));
        assert!(out.contains("Decision-First Agentic Engine"));
        assert!(out.contains("Ask"));
    }

    #[test]
    fn approval_modal_shows_the_bound_action() {
        let app = sample();
        let out = render(&app, 120, 40);
        assert!(out.contains("Approval required"));
        assert!(out.contains("fs_write"));
        assert!(out.contains("[y] approve") && out.contains("[n] deny"));
        assert!(out.contains("sha256:abcdef012345"));
        assert!(out.contains("⚠ 1 approval pending"));
    }

    #[test]
    fn views_show_their_data() {
        let mut app = sample();
        app.detail.as_mut().unwrap().approvals.clear();
        app.tasks[0].pending_approvals = 0;
        app.tab = Tab::Timeline;
        let out = render(&app, 120, 40);
        assert!(out.contains("DIRECT") && out.contains("Done. wrote 2 bytes"));
        app.tab = Tab::Plan;
        let out = render(&app, 120, 40);
        assert!(out.contains("fs_write") && out.contains("reasoning") && out.contains("W1"));
        app.tab = Tab::Context;
        assert!(render(&app, 120, 40).contains("a.txt"));
        app.tab = Tab::Tools;
        assert!(render(&app, 120, 40).contains("fs_write(path:str*"));
        app.tab = Tab::Tokens;
        assert!(render(&app, 120, 40).contains("naive LLM agent"));
    }

    #[test]
    fn wraps_text_and_formats_tokens() {
        assert_eq!(wrap_text("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap_text(&"x".repeat(20), 8).len(), 3);
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(1234), "1.23k");
        assert_eq!(fmt_tokens(25_000), "25.0k");
    }
}
