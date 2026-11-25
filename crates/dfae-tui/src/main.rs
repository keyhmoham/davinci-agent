//! `dfae` — terminal client for the Decision-First Agentic Engine.

mod api;
mod app;
mod ui;

use api::{Api, TaskRow};
use app::{Action, App};
use crossterm::event::{Event as TermEvent, EventStream, KeyEventKind};
use dfae_core::*;
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

enum Msg {
    Term(TermEvent),
    Tick,
    Health(Result<Value, String>),
    Info(Value),
    Stats(Value),
    Dispatch(Value),
    Tasks(Vec<TaskRow>),
    Detail(Box<TaskRecord>),
    Ev(Box<Event>),
    Memory(Vec<Value>),
    Notice(String, bool),
    Created(String),
}

struct Args {
    url: String,
    token: Option<String>,
    ask: Option<String>,
    mode: ModeRequest,
    auto_approve: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        url: std::env::var("DFAE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        token: std::env::var("DFAE_API_TOKEN").ok().filter(|s| !s.is_empty()),
        ask: None,
        mode: ModeRequest::Auto,
        auto_approve: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => a.url = it.next().ok_or("--url needs a value")?,
            "--token" => a.token = Some(it.next().ok_or("--token needs a value")?),
            "--ask" | "-a" => a.ask = Some(it.next().ok_or("--ask needs a request")?),
            "--mode" => {
                a.mode = match it.next().ok_or("--mode needs a value")?.as_str() {
                    "auto" => ModeRequest::Auto,
                    "direct" => ModeRequest::Direct,
                    "assisted" => ModeRequest::Assisted,
                    "escalated" => ModeRequest::Escalated,
                    other => return Err(format!("unknown mode '{other}'")),
                }
            }
            "--auto-approve" => a.auto_approve = true,
            "-h" | "--help" => {
                println!(
                    "dfae — terminal client for the Decision-First Agentic Engine\n\n\
                     USAGE:\n  dfae                         interactive TUI\n  dfae --ask \"<request>\"       run one request, print the result, exit\n\n\
                     OPTIONS:\n  --url <URL>        server (default $DFAE_URL or http://127.0.0.1:8080)\n  --token <T>        bearer token (default $DFAE_API_TOKEN)\n  --mode <M>         auto|direct|assisted|escalated (with --ask)\n  --auto-approve     skip approvals (with --ask; server must allow it)\n"
                );
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("dfae {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' (try --help)")),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let api = Api::new(&args.url, args.token.clone());
    if let Some(goal) = args.ask.clone() {
        let code = ask(&api, goal, args.mode, args.auto_approve).await;
        std::process::exit(code);
    }
    run_tui(api, args.url).await
}

// ------------------------------------------------------------ --ask mode ----

fn describe(e: &Event) -> Option<String> {
    Some(match &e.kind {
        EventKind::Extraction { entities, backend, .. } => format!("extract   {} entities via {backend}", entities.len()),
        EventKind::Decision { record } => format!("decide    {} via {}", record.kind, record.provider),
        EventKind::Mode { mode, reason, .. } => format!("mode      {} — {reason}", format!("{mode:?}").to_uppercase()),
        EventKind::PlanCreated { plan } => format!("plan      {} steps ({:?})", plan.steps.len(), plan.source),
        EventKind::TeamFormed { goals, reason } => format!("team      {} agents — {reason}", goals.iter().filter(|g| !g.synth).count()),
        EventKind::AgentDone { step_id, role, status, generative_tokens, .. } => {
            format!("agent     {step_id} ({}) → {status:?}, {generative_tokens} generative tokens", role.as_str())
        }
        EventKind::Queued { position } => format!("queued    position {position}"),
        EventKind::PlanRevised { reason, .. } => format!("replan    {reason}"),
        EventKind::StepUpdate { step_id, status, tool, error, .. } if status.is_done() => format!(
            "step      {step_id} {} → {status:?}{}",
            tool.as_deref().unwrap_or("reasoning"),
            error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()
        ),
        EventKind::Model { purpose, prompt_tokens, completion_tokens, .. } => format!("model     {purpose} {prompt_tokens}→{completion_tokens} tok"),
        EventKind::Warning { message } => format!("warning   {message}"),
        _ => return None,
    })
}

async fn ask(api: &Api, goal: String, mode: ModeRequest, auto: bool) -> i32 {
    let req = TaskRequest { goal, session_id: None, mode, profile: None, auto_approve: auto, delegate: None };
    let id = match api.create(&req).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    eprintln!("task {}", short(&id));
    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let api2 = api.clone();
    let id2 = id.clone();
    tokio::spawn(async move {
        loop {
            let tx = tx.clone();
            let _ = api2
                .stream_events(&id2, move |e| {
                    let _ = tx.send(e);
                })
                .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    let mut seen = std::collections::HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await else { continue };
        if !seen.insert(e.seq) {
            continue;
        }
        if let Some(line) = describe(&e) {
            eprintln!("  {line}");
        }
        if let EventKind::ApprovalRequested { approval } = &e.kind.clone() {
            if approval.status != ApprovalStatus::Pending {
                continue;
            }
            eprint!("  approval needed: {} {} — {}\n  approve? [y/N] ", approval.tool, approval.args, approval.reason);
            let line = tokio::task::spawn_blocking(|| {
                let mut s = String::new();
                let _ = std::io::stdin().read_line(&mut s);
                s
            })
            .await
            .unwrap_or_default();
            let yes = line.trim().eq_ignore_ascii_case("y");
            if let Err(err) = api.decide(&id, &approval.id, yes).await {
                eprintln!("  {err}");
            }
        }
        if let EventKind::Final { answer, status } = &e.kind {
            println!("{answer}");
            if let Ok(t) = api.task(&id).await {
                let s = t.ledger.summary();
                eprintln!(
                    "\n[{status}] mode={} verification={} · generative tokens {} vs ~{} naive ({:.0}% saved) · slm calls {} · decision calls {}",
                    t.mode.map(|m| format!("{m:?}").to_lowercase()).unwrap_or_default(),
                    t.verification.as_ref().map(|v| format!("{:?}", v.status).to_lowercase()).unwrap_or_else(|| "n/a".into()),
                    s.generative_tokens,
                    s.baseline_tokens,
                    s.saved_pct,
                    s.slm_calls + s.strong_calls,
                    s.jev_calls
                );
            }
            return if *status == TaskStatus::Completed { 0 } else { 1 };
        }
    }
    eprintln!("timed out waiting for the task");
    1
}

// ------------------------------------------------------------------- TUI ----

async fn run_tui(api: Api, url: String) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let res = tui_loop(&mut terminal, api, url).await;
    ratatui::restore();
    res
}

async fn tui_loop(terminal: &mut ratatui::DefaultTerminal, api: Api, url: String) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let (sel_tx, sel_rx) = watch::channel::<Option<String>>(None);

    // terminal input
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut s = EventStream::new();
            while let Some(Ok(ev)) = s.next().await {
                if tx.send(Msg::Term(ev)).is_err() {
                    break;
                }
            }
        });
    }
    // spinner / flash clock
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(100));
            loop {
                t.tick().await;
                if tx.send(Msg::Tick).is_err() {
                    break;
                }
            }
        });
    }
    // slow pollers: health, info, stats
    {
        let (tx, api) = (tx.clone(), api.clone());
        tokio::spawn(async move {
            let mut have_info = false;
            loop {
                let h = api.health().await;
                let ok = h.is_ok();
                let _ = tx.send(Msg::Health(h));
                if ok {
                    if !have_info && let Ok(i) = api.info().await {
                        have_info = true;
                        let _ = tx.send(Msg::Info(i));
                    }
                    if let Ok(s) = api.stats().await {
                        let _ = tx.send(Msg::Stats(s));
                    }
                }
                tokio::time::sleep(Duration::from_millis(if ok { 2000 } else { 1000 })).await;
            }
        });
    }
    // dispatcher pools: fast, so worker-slot occupancy is visible while agents run
    {
        let (tx, api) = (tx.clone(), api.clone());
        tokio::spawn(async move {
            loop {
                if let Ok(d) = api.dispatch().await {
                    let _ = tx.send(Msg::Dispatch(d));
                }
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }
    // task list
    {
        let (tx, api) = (tx.clone(), api.clone());
        tokio::spawn(async move {
            loop {
                if let Ok(t) = api.tasks().await {
                    let _ = tx.send(Msg::Tasks(t));
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
        });
    }
    // selected task record
    {
        let (tx, api, sel) = (tx.clone(), api.clone(), sel_rx.clone());
        tokio::spawn(async move {
            loop {
                let id = sel.borrow().clone();
                let mut wait = 300;
                if let Some(id) = id
                    && let Ok(t) = api.task(&id).await
                {
                    if t.status.is_terminal() {
                        wait = 1200;
                    }
                    let _ = tx.send(Msg::Detail(Box::new(t)));
                }
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
        });
    }
    // event stream of the selected task (backlog replay + live)
    {
        let (tx, api, mut sel) = (tx.clone(), api.clone(), sel_rx.clone());
        tokio::spawn(async move {
            let mut current: Option<tokio::task::JoinHandle<()>> = None;
            loop {
                if let Some(h) = current.take() {
                    h.abort();
                }
                let id = sel.borrow_and_update().clone();
                if let Some(id) = id {
                    let (tx, api) = (tx.clone(), api.clone());
                    current = Some(tokio::spawn(async move {
                        loop {
                            let tx2 = tx.clone();
                            let _ = api
                                .stream_events(&id, move |e| {
                                    let _ = tx2.send(Msg::Ev(Box::new(e)));
                                })
                                .await;
                            tokio::time::sleep(Duration::from_millis(800)).await;
                        }
                    }));
                }
                if sel.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    let mut app = App::new(&url);
    let mut last_sel: Option<String> = None;
    terminal.draw(|f| ui::draw(f, &app))?;
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Term(TermEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                if let Some(action) = app.handle_key(k) {
                    match action {
                        Action::Quit => break,
                        Action::Submit(req) => {
                            let (api, tx) = (api.clone(), tx.clone());
                            tokio::spawn(async move {
                                match api.create(&req).await {
                                    Ok(id) => {
                                        let _ = tx.send(Msg::Created(id));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Msg::Notice(format!("submit failed: {e}"), true));
                                    }
                                }
                            });
                        }
                        Action::Cancel(id) => {
                            let (api, tx) = (api.clone(), tx.clone());
                            tokio::spawn(async move {
                                let m = match api.cancel(&id).await {
                                    Ok(()) => ("cancelling…".to_string(), false),
                                    Err(e) => (format!("cancel: {e}"), true),
                                };
                                let _ = tx.send(Msg::Notice(m.0, m.1));
                            });
                        }
                        Action::Decide { task, approval, approve } => {
                            let (api, tx) = (api.clone(), tx.clone());
                            tokio::spawn(async move {
                                let m = match api.decide(&task, &approval, approve).await {
                                    Ok(()) => (if approve { "approved" } else { "denied" }.to_string(), false),
                                    Err(e) => (format!("approval: {e}"), true),
                                };
                                let _ = tx.send(Msg::Notice(m.0, m.1));
                            });
                        }
                        Action::MemorySearch(q) => {
                            let (api, tx) = (api.clone(), tx.clone());
                            tokio::spawn(async move {
                                match api.memory_search(&q).await {
                                    Ok(h) => {
                                        let _ = tx.send(Msg::Memory(h));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Msg::Notice(format!("memory: {e}"), true));
                                    }
                                }
                            });
                        }
                        Action::Remember(t) => {
                            let (api, tx) = (api.clone(), tx.clone());
                            tokio::spawn(async move {
                                let m = match api.remember(&t).await {
                                    Ok(()) => ("remembered".to_string(), false),
                                    Err(e) => (format!("remember: {e}"), true),
                                };
                                let _ = tx.send(Msg::Notice(m.0, m.1));
                            });
                        }
                        Action::SelectionChanged => {}
                    }
                }
            }
            Msg::Term(_) => {}
            Msg::Tick => app.tick = app.tick.wrapping_add(1),
            Msg::Health(Ok(h)) => {
                app.connected = true;
                app.health = Some(h);
            }
            Msg::Health(Err(_)) => app.connected = false,
            Msg::Info(i) => app.info = Some(i),
            Msg::Stats(s) => app.stats = Some(s),
            Msg::Dispatch(d) => app.dispatch = Some(d),
            Msg::Tasks(t) => app.apply_tasks(t),
            Msg::Detail(d) => app.apply_detail(*d),
            Msg::Ev(e) => app.push_event(*e),
            Msg::Memory(h) => app.memory_hits = h,
            Msg::Notice(m, err) => app.notify(m, err),
            Msg::Created(id) => {
                app.want_select = Some(id);
                app.notify("task submitted", false);
            }
        }
        if app.quit {
            break;
        }
        let cur = app.selected_id();
        if cur != last_sel {
            last_sel = cur.clone();
            let _ = sel_tx.send(cur);
        }
        terminal.draw(|f| ui::draw(f, &app))?;
    }
    Ok(())
}
