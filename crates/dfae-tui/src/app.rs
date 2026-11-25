//! Application state and input handling. Pure (no I/O): key presses produce
//! [`Action`]s that the event loop executes, and server data is merged in via
//! the `apply_*` methods. This keeps the interface unit-testable.

use crate::api::TaskRow;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dfae_core::*;
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Timeline,
    Plan,
    Team,
    Context,
    Tokens,
    Memory,
    Tools,
}

impl Tab {
    pub const ALL: [Tab; 7] = [Tab::Timeline, Tab::Plan, Tab::Team, Tab::Context, Tab::Tokens, Tab::Memory, Tab::Tools];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Timeline => "Timeline",
            Tab::Plan => "Plan",
            Tab::Team => "Team",
            Tab::Context => "Cognition",
            Tab::Tokens => "Tokens",
            Tab::Memory => "Memory",
            Tab::Tools => "Tools",
        }
    }
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Submit(TaskRequest),
    Cancel(String),
    Decide { task: String, approval: String, approve: bool },
    MemorySearch(String),
    Remember(String),
    SelectionChanged,
    Quit,
}

/// Single-line text editor.
#[derive(Debug, Default, Clone)]
pub struct InputBox {
    pub text: String,
    /// Cursor position in chars.
    pub cursor: usize,
}

impl InputBox {
    fn byte(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(self.text.len())
    }
    pub fn insert(&mut self, c: char) {
        let b = self.byte(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let b = self.byte(self.cursor - 1);
            self.text.remove(b);
            self.cursor -= 1;
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.text.chars().count() {
            let b = self.byte(self.cursor);
            self.text.remove(b);
        }
    }
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }
    pub fn home(&mut self) {
        self.cursor = 0;
    }
    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }
    pub fn kill_line(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
    pub fn kill_word(&mut self) {
        while self.cursor > 0 && self.text.chars().nth(self.cursor - 1) == Some(' ') {
            self.backspace();
        }
        while self.cursor > 0 && self.text.chars().nth(self.cursor - 1) != Some(' ') {
            self.backspace();
        }
    }
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

pub struct App {
    pub base_url: String,
    pub tasks: Vec<TaskRow>,
    pub selected: usize,
    pub detail: Option<TaskRecord>,
    pub events: Vec<Event>,
    pub tab: Tab,
    pub input: InputBox,
    pub mode: ModeRequest,
    /// Subagent delegation: `None` = automatic, `Some(false)` = off.
    pub delegate: Option<bool>,
    pub auto_approve: bool,
    pub session_id: String,
    pub dispatch: Option<Value>,
    pub health: Option<Value>,
    pub info: Option<Value>,
    pub stats: Option<Value>,
    pub memory_query: String,
    pub memory_hits: Vec<Value>,
    pub scroll: usize,
    pub plan_sel: usize,
    pub connected: bool,
    pub flash: Option<(String, Instant, bool)>,
    pub show_help: bool,
    pub tick: u64,
    pub quit: bool,
    /// Select this task as soon as it shows up in the list (just submitted).
    pub want_select: Option<String>,
    history: Vec<String>,
    history_pos: Option<usize>,
}

impl App {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.into(),
            tasks: vec![],
            selected: 0,
            detail: None,
            events: vec![],
            tab: Tab::Timeline,
            input: InputBox::default(),
            mode: ModeRequest::Auto,
            delegate: None,
            auto_approve: false,
            session_id: new_id(),
            dispatch: None,
            health: None,
            info: None,
            stats: None,
            memory_query: String::new(),
            memory_hits: vec![],
            scroll: 0,
            plan_sel: 0,
            connected: false,
            flash: None,
            show_help: false,
            tick: 0,
            quit: false,
            want_select: None,
            history: vec![],
            history_pos: None,
        }
    }

    pub fn selected_id(&self) -> Option<String> {
        self.tasks.get(self.selected).map(|t| t.task_id.clone())
    }

    pub fn notify(&mut self, msg: impl Into<String>, error: bool) {
        self.flash = Some((msg.into(), Instant::now(), error));
    }

    pub fn flash_text(&self) -> Option<(&str, bool)> {
        self.flash.as_ref().filter(|(_, t, _)| t.elapsed() < Duration::from_secs(6)).map(|(m, _, e)| (m.as_str(), *e))
    }

    /// A pending approval on the selected task, if any.
    pub fn pending_approval(&self) -> Option<(&TaskRecord, &Approval)> {
        let d = self.detail.as_ref()?;
        if Some(&d.task_id) != self.tasks.get(self.selected).map(|t| &t.task_id) {
            return None;
        }
        d.approvals.iter().find(|a| a.status == ApprovalStatus::Pending).map(|a| (d, a))
    }

    pub fn total_pending_approvals(&self) -> usize {
        self.tasks.iter().map(|t| t.pending_approvals).sum()
    }

    // ------------------------------------------------------ data merging ----

    pub fn apply_tasks(&mut self, rows: Vec<TaskRow>) {
        let before = self.selected_id();
        let rows = tree_order(rows);
        // keep a stable selection (by id) across refreshes
        let wanted = self.want_select.as_ref().and_then(|id| rows.iter().position(|r| &r.task_id == id));
        let new_sel = match wanted {
            Some(i) => {
                self.want_select = None;
                i
            }
            None => before.as_ref().and_then(|id| rows.iter().position(|r| &r.task_id == id)).unwrap_or(0),
        };
        self.tasks = rows;
        self.selected = new_sel.min(self.tasks.len().saturating_sub(1));
        if self.selected_id() != before {
            self.reset_view();
        }
    }

    pub fn apply_detail(&mut self, rec: TaskRecord) {
        if Some(&rec.task_id) == self.selected_id().as_ref() {
            self.detail = Some(rec);
        }
    }

    pub fn push_event(&mut self, e: Event) {
        if Some(&e.task_id) != self.selected_id().as_ref() {
            return;
        }
        if self.events.iter().any(|x| x.seq == e.seq) {
            return;
        }
        self.events.push(e);
        self.events.sort_by_key(|e| e.seq);
    }

    fn reset_view(&mut self) {
        self.detail = None;
        self.events.clear();
        self.scroll = 0;
        self.plan_sel = 0;
    }

    pub fn select(&mut self, idx: usize) -> bool {
        if self.tasks.is_empty() {
            return false;
        }
        let idx = idx.min(self.tasks.len() - 1);
        if idx == self.selected {
            return false;
        }
        self.selected = idx;
        self.reset_view();
        true
    }

    // -------------------------------------------------------------- keys ----

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.show_help {
            self.show_help = false;
            return None;
        }
        // approval modal captures y/n while the input is empty
        if let Some((task, a)) = self.pending_approval().map(|(t, a)| (t.task_id.clone(), a.id.clone()))
            && self.input.text.is_empty()
        {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return Some(Action::Decide { task, approval: a, approve: true }),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Some(Action::Decide { task, approval: a, approve: false }),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.quit = true;
                return Some(Action::Quit);
            }
            KeyCode::Char('d') if ctrl && self.input.text.is_empty() => {
                self.quit = true;
                return Some(Action::Quit);
            }
            KeyCode::Char('x') if ctrl => return self.selected_id().map(Action::Cancel),
            KeyCode::Char('u') if ctrl => self.input.kill_line(),
            KeyCode::Char('w') if ctrl => self.input.kill_word(),
            KeyCode::Char('a') if ctrl => {
                // jump to the first task waiting for approval
                if let Some(i) = self.tasks.iter().position(|t| t.pending_approvals > 0) {
                    return if self.select(i) { Some(Action::SelectionChanged) } else { None };
                }
            }
            KeyCode::F(1) => self.show_help = true,
            KeyCode::F(2) => {
                self.mode = match self.mode {
                    ModeRequest::Auto => ModeRequest::Direct,
                    ModeRequest::Direct => ModeRequest::Assisted,
                    ModeRequest::Assisted => ModeRequest::Escalated,
                    ModeRequest::Escalated => ModeRequest::Auto,
                };
            }
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()],
            KeyCode::Char(c @ '1'..='7') if self.input.text.is_empty() && key.modifiers.contains(KeyModifiers::ALT) => {
                self.tab = Tab::ALL[(c as usize) - ('1' as usize)];
            }
            KeyCode::Up => {
                if self.tab == Tab::Plan && ctrl {
                    self.plan_sel = self.plan_sel.saturating_sub(1);
                } else if !self.input.text.is_empty() || self.history_pos.is_some() {
                    self.history_prev();
                } else if self.select(self.selected.saturating_sub(1)) {
                    return Some(Action::SelectionChanged);
                }
            }
            KeyCode::Down => {
                if self.tab == Tab::Plan && ctrl {
                    self.plan_sel += 1;
                } else if self.history_pos.is_some() {
                    self.history_next();
                } else if self.select(self.selected + 1) {
                    return Some(Action::SelectionChanged);
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(8),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::End if self.input.text.is_empty() => self.scroll = 0,
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Esc => {
                self.input.kill_line();
                self.history_pos = None;
            }
            KeyCode::Enter => {
                let line = self.input.take();
                self.history_pos = None;
                return self.run_line(line.trim());
            }
            KeyCode::Char(c) if !ctrl => self.input.insert(c),
            _ => {}
        }
        None
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let p = match self.history_pos {
            None => self.history.len() - 1,
            Some(p) => p.saturating_sub(1),
        };
        self.history_pos = Some(p);
        self.input.text = self.history[p].clone();
        self.input.end();
    }

    fn history_next(&mut self) {
        match self.history_pos {
            Some(p) if p + 1 < self.history.len() => {
                self.history_pos = Some(p + 1);
                self.input.text = self.history[p + 1].clone();
                self.input.end();
            }
            _ => {
                self.history_pos = None;
                self.input.kill_line();
            }
        }
    }

    fn run_line(&mut self, line: &str) -> Option<Action> {
        if line.is_empty() {
            return None;
        }
        self.history.push(line.to_string());
        if let Some(cmd) = line.strip_prefix('/') {
            return self.run_command(cmd);
        }
        self.scroll = 0;
        Some(Action::Submit(TaskRequest {
            goal: line.to_string(),
            session_id: Some(self.session_id.clone()),
            mode: self.mode,
            profile: None,
            auto_approve: self.auto_approve,
            delegate: self.delegate,
        }))
    }

    fn run_command(&mut self, cmd: &str) -> Option<Action> {
        let (name, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd, ""));
        match name {
            "quit" | "q" | "exit" => {
                self.quit = true;
                Some(Action::Quit)
            }
            "help" | "?" => {
                self.show_help = true;
                None
            }
            "mode" => {
                match arg {
                    "auto" => self.mode = ModeRequest::Auto,
                    "direct" => self.mode = ModeRequest::Direct,
                    "assisted" => self.mode = ModeRequest::Assisted,
                    "escalated" => self.mode = ModeRequest::Escalated,
                    _ => self.notify("usage: /mode auto|direct|assisted|escalated", true),
                }
                None
            }
            "team" => {
                match arg {
                    "auto" | "" => self.delegate = None,
                    "off" => self.delegate = Some(false),
                    "on" => self.delegate = Some(true),
                    _ => self.notify("usage: /team auto|on|off", true),
                }
                self.tab = Tab::Team;
                None
            }
            "autoapprove" => {
                self.auto_approve = matches!(arg, "on" | "true" | "1");
                self.notify(
                    format!("auto-approve {} (the server must allow it)", if self.auto_approve { "ON — approvals are skipped" } else { "off" }),
                    self.auto_approve,
                );
                None
            }
            "new" => {
                self.session_id = new_id();
                self.notify("new session: earlier tasks no longer feed the context", false);
                None
            }
            "cancel" => self.selected_id().map(Action::Cancel),
            "memory" | "m" => {
                if arg.is_empty() {
                    self.tab = Tab::Memory;
                    None
                } else {
                    self.tab = Tab::Memory;
                    self.memory_query = arg.to_string();
                    Some(Action::MemorySearch(arg.to_string()))
                }
            }
            "remember" => {
                if arg.is_empty() {
                    self.notify("usage: /remember <fact>", true);
                    None
                } else {
                    Some(Action::Remember(arg.to_string()))
                }
            }
            "tools" => {
                self.tab = Tab::Tools;
                None
            }
            other => {
                self.notify(format!("unknown command /{other} — try /help"), true);
                None
            }
        }
    }
}

/// Parents first, each followed by its agents in the order they were dispatched.
/// (The server lists newest first; agents whose parent is not in the list stay top-level.)
pub fn tree_order(rows: Vec<TaskRow>) -> Vec<TaskRow> {
    let ids: std::collections::HashSet<String> = rows.iter().map(|r| r.task_id.clone()).collect();
    let is_child = |r: &TaskRow| r.parent_id.as_ref().map(|p| ids.contains(p)).unwrap_or(false);
    let mut out = Vec::with_capacity(rows.len());
    for top in rows.iter().filter(|r| !is_child(r)) {
        out.push(top.clone());
        let mut kids: Vec<&TaskRow> = rows.iter().filter(|c| c.parent_id.as_deref() == Some(top.task_id.as_str())).collect();
        kids.sort_by_key(|c| c.created_at);
        out.extend(kids.into_iter().cloned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent { code: c, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent { code: KeyCode::Char(c), modifiers: KeyModifiers::CONTROL, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }
    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn editor_handles_unicode_and_cursor() {
        let mut i = InputBox::default();
        for c in "héllo wörld".chars() {
            i.insert(c);
        }
        i.left();
        i.left();
        i.backspace();
        assert_eq!(i.text, "héllo wöld");
        i.home();
        i.delete();
        assert_eq!(i.text, "éllo wöld");
        i.end();
        i.kill_word();
        assert_eq!(i.text, "éllo ");
    }

    #[test]
    fn enter_submits_a_task_in_the_current_session_and_mode() {
        let mut app = App::new("http://x");
        app.mode = ModeRequest::Assisted;
        type_str(&mut app, "  what is 2+2  ");
        let a = app.handle_key(key(KeyCode::Enter)).unwrap();
        match a {
            Action::Submit(r) => {
                assert_eq!(r.goal, "what is 2+2");
                assert_eq!(r.mode, ModeRequest::Assisted);
                assert_eq!(r.session_id.as_deref(), Some(app.session_id.as_str()));
                assert!(!r.auto_approve);
            }
            other => panic!("{other:?}"),
        }
        assert!(app.input.text.is_empty());
    }

    #[test]
    fn slash_commands() {
        let mut app = App::new("http://x");
        type_str(&mut app, "/mode direct");
        assert!(app.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(app.mode, ModeRequest::Direct);
        type_str(&mut app, "/memory staging db");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::MemorySearch("staging db".into())));
        assert_eq!(app.tab, Tab::Memory);
        type_str(&mut app, "/bogus");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.flash_text().unwrap().1, "unknown commands flash an error");
        type_str(&mut app, "/quit");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(Action::Quit));
    }

    fn row(id: &str, parent: Option<&str>, secs: i64) -> TaskRow {
        TaskRow {
            task_id: id.into(),
            goal: id.into(),
            status: TaskStatus::Running,
            mode: None,
            verification: None,
            pending_approvals: 0,
            ledger: Default::default(),
            session_id: "s".into(),
            created_at: Some(chrono::DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()),
            role: None,
            parent_id: parent.map(String::from),
            agents: 0,
        }
    }

    #[test]
    fn agents_are_listed_under_their_coordinator_in_dispatch_order() {
        // server order: newest first
        let rows = vec![row("k2", Some("p"), 12), row("k1", Some("p"), 11), row("other", None, 20), row("p", None, 10), row("orphan", Some("gone"), 5)];
        let ids: Vec<_> = tree_order(rows).into_iter().map(|r| r.task_id).collect();
        assert_eq!(ids, vec!["other", "p", "k1", "k2", "orphan"]);
    }

    #[test]
    fn selection_follows_the_task_even_when_the_tree_reorders() {
        let mut app = App::new("http://x");
        app.apply_tasks(vec![row("p", None, 1)]);
        assert_eq!(app.selected_id().as_deref(), Some("p"));
        app.apply_tasks(vec![row("k1", Some("p"), 3), row("p", None, 1)]);
        assert_eq!(app.selected_id().as_deref(), Some("p"), "the new agent appears below its parent without moving the selection");
        assert_eq!(app.tasks[1].task_id, "k1");
    }

    #[test]
    fn team_command_sets_delegation() {
        let mut app = App::new("http://x");
        type_str(&mut app, "/team off");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.delegate, Some(false));
        assert_eq!(app.tab, Tab::Team);
        type_str(&mut app, "list files");
        match app.handle_key(key(KeyCode::Enter)) {
            Some(Action::Submit(r)) => assert_eq!(r.delegate, Some(false)),
            other => panic!("{other:?}"),
        }
        type_str(&mut app, "/team auto");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.delegate, None);
    }

    #[test]
    fn tabs_and_quit_keys() {
        let mut app = App::new("http://x");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Plan);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Team);
        app.handle_key(key(KeyCode::BackTab));
        app.handle_key(key(KeyCode::BackTab));
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Tools);
        assert_eq!(app.handle_key(ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn y_and_n_answer_a_pending_approval_only_when_the_input_is_empty() {
        let mut app = App::new("http://x");
        let mut rec = TaskRecord::new(TaskRequest { goal: "g".into(), ..Default::default() }, Budget::default());
        let now = chrono::Utc::now();
        rec.approvals.push(Approval {
            id: "a1".into(),
            step_id: "s1".into(),
            tool: "fs_write".into(),
            args: serde_json::json!({}),
            args_hash: "h".into(),
            reason: "writes a file".into(),
            risk: Risk::Moderate,
            status: ApprovalStatus::Pending,
            created_at: now,
            expires_at: now,
            decided_by: None,
        });
        app.tasks = vec![TaskRow {
            task_id: rec.task_id.clone(),
            goal: "g".into(),
            status: TaskStatus::WaitingForApproval,
            mode: None,
            verification: None,
            pending_approvals: 1,
            ledger: Default::default(),
            session_id: "s".into(),
            created_at: None,
            role: None,
            parent_id: None,
            agents: 0,
        }];
        let tid = rec.task_id.clone();
        app.apply_detail(rec);
        // typing a message that starts with y must not approve anything
        type_str(&mut app, "x");
        assert!(app.handle_key(key(KeyCode::Char('y'))).is_none());
        app.input.kill_line();
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Some(Action::Decide { task: tid.clone(), approval: "a1".into(), approve: true }));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Some(Action::Decide { task: tid, approval: "a1".into(), approve: false }));
    }
}
