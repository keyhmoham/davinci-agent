//! The coding-agent tool set: edit, glob, search, tree, file operations, a real shell and background jobs.

use dfae_core::*;
use dfae_memory::InMemoryStore;
use dfae_tools::{PolicyConfig, PolicyDecision, PolicyEngine, ToolRegistry, register_native};
use serde_json::{Value, json};
use std::sync::Arc;

struct Env {
    reg: ToolRegistry,
    policy: Arc<PolicyEngine>,
    ws: tempfile::TempDir,
    ctx: ToolContext,
}

fn env(toml: &str) -> Env {
    let ws = tempfile::tempdir().unwrap();
    let policy = Arc::new(PolicyEngine::new(PolicyConfig::from_toml(toml).unwrap(), ws.path().to_path_buf()));
    let mut reg = ToolRegistry::new();
    register_native(&mut reg, policy.clone(), Arc::new(InMemoryStore::new()));
    let ctx = ToolContext {
        task_id: "t".into(),
        step_id: "s".into(),
        workspace: policy.workspace().to_path_buf(),
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    Env { reg, policy, ws, ctx }
}

fn sandbox() -> Env {
    env("[shell]\nmode = \"sandbox\"\n[filesystem]\nwrite_requires_approval = false\n")
}

impl Env {
    async fn run(&self, tool: &str, args: Value) -> Result<ToolOutput, ToolError> {
        // the way the executor does it: the policy first, then the tool
        let m = self.reg.meta(tool).unwrap_or_else(|| panic!("no tool {tool}"));
        if let PolicyDecision::Deny(why) = self.policy.check(&m, &args, false) {
            return Err(ToolError::policy(why));
        }
        self.reg.get(tool).unwrap().execute(args, self.ctx.clone()).await
    }
    fn write(&self, rel: &str, text: &str) {
        let p = self.ws.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.ws.path().join(rel)).unwrap()
    }
}

#[tokio::test]
async fn fs_edit_changes_exactly_what_it_is_told_to_and_explains_failures() {
    let e = sandbox();
    e.write("src/lib.rs", "fn a() {}\nfn b() {}\nfn a_again() {}\n");
    let ok = e.run("fs_edit", json!({"path": "src/lib.rs", "old_string": "fn b() {}", "new_string": "fn b() -> u8 { 1 }"})).await.unwrap();
    assert!(ok.content.contains("1 replacement") && ok.content.contains("line 2"), "{}", ok.content);
    assert_eq!(e.read("src/lib.rs"), "fn a() {}\nfn b() -> u8 { 1 }\nfn a_again() {}\n");

    let amb = e.run("fs_edit", json!({"path": "src/lib.rs", "old_string": "fn a", "new_string": "fn x"})).await.unwrap_err();
    assert!(amb.message.contains("occurs 2 times") && amb.message.contains("replace_all"), "{}", amb.message);
    assert!(e.read("src/lib.rs").contains("fn a() {}"), "an ambiguous edit changes nothing");
    let all = e.run("fs_edit", json!({"path": "src/lib.rs", "old_string": "fn a", "new_string": "fn x", "replace_all": true})).await.unwrap();
    assert!(all.content.contains("2 replacement"));

    let missing = e.run("fs_edit", json!({"path": "src/lib.rs", "old_string": "nope", "new_string": "x"})).await.unwrap_err();
    assert!(missing.message.contains("not found") && missing.message.contains("fs_read"), "{}", missing.message);
    assert!(e.run("fs_edit", json!({"path": "src/lib.rs", "old_string": "", "new_string": "x"})).await.unwrap_err().message.contains("fs_write"));
    assert!(e.run("fs_edit", json!({"path": "src/lib.rs", "old_string": "fn x", "new_string": "fn x"})).await.is_err(), "no-op edits are refused");
    let nofile = e.run("fs_edit", json!({"path": "nope.rs", "old_string": "a", "new_string": "b"})).await.unwrap_err();
    assert_eq!(nofile.kind, ErrorKind::NotFound);
    let escape = e.run("fs_edit", json!({"path": "../outside.rs", "old_string": "a", "new_string": "b"})).await.unwrap_err();
    assert_eq!(escape.kind, ErrorKind::Policy);

    let tool = e.reg.get("fs_edit").unwrap();
    let v = tool.verify(&json!({"path": "src/lib.rs", "new_string": "fn x"}), &ok, &e.ctx).await.unwrap();
    assert!(v.is_ok());
}

#[tokio::test]
async fn glob_finds_files_newest_first_and_skips_vendored_directories() {
    let e = sandbox();
    e.write("src/a.rs", "");
    e.write("src/deep/b.rs", "");
    e.write("src/c.py", "");
    e.write("node_modules/x/d.rs", "");
    e.write("target/e.rs", "");
    let r = e.run("fs_glob", json!({"pattern": "**/*.rs"})).await.unwrap().content;
    assert!(r.starts_with("2 file(s)") && r.contains("src/a.rs") && r.contains("src/deep/b.rs"), "{r}");
    assert!(!r.contains("node_modules") && !r.contains("target"), "{r}");
    let only_deep = e.run("fs_glob", json!({"pattern": "*.rs", "path": "src/deep"})).await.unwrap().content;
    assert!(only_deep.starts_with("1 file(s)"), "{only_deep}");
    assert!(e.run("fs_glob", json!({"pattern": "*.zzz"})).await.unwrap().content.contains("no files match"));
    assert_eq!(e.run("fs_glob", json!({"pattern": "*", "path": "../.."})).await.unwrap_err().kind, ErrorKind::Policy);
}

#[tokio::test]
async fn search_filters_by_glob_shows_context_and_lists_files() {
    let e = sandbox();
    e.write("a.rs", "one\nTODO: first\nthree\nfour\nfive\nTODO: second\n");
    e.write("b.py", "TODO: python\n");
    let all = e.run("fs_search", json!({"pattern": "todo"})).await.unwrap().content;
    assert!(all.starts_with("3 match(es)"), "{all}");
    let rs = e.run("fs_search", json!({"pattern": "TODO", "glob": "*.rs"})).await.unwrap().content;
    assert!(rs.starts_with("2 match(es)") && !rs.contains("b.py"), "{rs}");
    let ctx = e.run("fs_search", json!({"pattern": "first", "context": 1, "glob": "*.rs"})).await.unwrap().content;
    assert!(ctx.contains("a.rs-1- one") && ctx.contains("a.rs:2: TODO: first") && ctx.contains("a.rs-3- three"), "{ctx}");
    let files = e.run("fs_search", json!({"pattern": "TODO", "files_only": true})).await.unwrap().content;
    assert!(files.starts_with("2 file(s)") && files.contains("a.rs") && files.contains("b.py") && !files.contains(':'), "{files}");
    let case = e.run("fs_search", json!({"pattern": "todo", "case_sensitive": true})).await.unwrap().content;
    assert!(case.contains("no matches"), "{case}");
    assert!(e.run("fs_search", json!({"pattern": "("})).await.unwrap_err().message.contains("bad pattern"));
}

#[tokio::test]
async fn reading_ranges_and_trees() {
    let e = sandbox();
    let text: String = (1..=10).map(|i| format!("line{i}\n")).collect();
    e.write("big.txt", &text);
    let r = e.run("fs_read", json!({"path": "big.txt", "offset": 3, "limit": 2, "numbered": true})).await.unwrap().content;
    assert!(r.starts_with("    3\tline3\n    4\tline4") && r.contains("lines 3-4 of 10") && r.contains("offset=5"), "{r}");
    assert_eq!(e.run("fs_read", json!({"path": "big.txt"})).await.unwrap().content, text, "a plain read is unchanged");
    assert!(e.run("fs_read", json!({"path": "big.txt", "offset": 99})).await.unwrap_err().message.contains("past the end"));
    e.write("proj/src/main.rs", "x");
    e.write("proj/node_modules/pkg/i.js", "x");
    let t = e.run("fs_list", json!({"path": "proj", "depth": 3})).await.unwrap().content;
    assert!(t.contains("d src/") && t.contains("  f main.rs 1B") && t.contains("d node_modules/"), "{t}");
    assert!(!t.contains("i.js"), "vendored directories are listed but not entered: {t}");
}

#[tokio::test]
async fn file_operations_are_guarded_and_say_what_to_do_instead() {
    let e = sandbox();
    e.write("a.txt", "A");
    e.run("fs_mkdir", json!({"path": "out/deep"})).await.unwrap();
    assert!(e.ws.path().join("out/deep").is_dir());
    e.run("fs_copy", json!({"from": "a.txt", "to": "out/b.txt"})).await.unwrap();
    assert_eq!(e.read("out/b.txt"), "A");
    assert!(e.run("fs_copy", json!({"from": "a.txt", "to": "out/b.txt"})).await.unwrap_err().message.contains("already exists"));
    e.run("fs_move", json!({"from": "out/b.txt", "to": "moved/c.txt"})).await.unwrap();
    assert!(!e.ws.path().join("out/b.txt").exists() && e.read("moved/c.txt") == "A");
    e.run("fs_copy", json!({"from": "out", "to": "out2"})).await.unwrap();
    assert!(e.ws.path().join("out2/deep").is_dir());

    let dir = e.run("fs_delete", json!({"path": "out"})).await.unwrap_err();
    assert!(dir.message.contains("recursive=true"), "{}", dir.message);
    e.run("fs_delete", json!({"path": "out", "recursive": true})).await.unwrap();
    assert!(!e.ws.path().join("out").exists());
    e.run("fs_delete", json!({"path": "a.txt"})).await.unwrap();
    assert_eq!(e.run("fs_delete", json!({"path": "a.txt"})).await.unwrap_err().kind, ErrorKind::NotFound);
    assert_eq!(e.run("fs_delete", json!({"path": ".", "recursive": true})).await.unwrap_err().kind, ErrorKind::Policy, "never the workspace itself");
    for (tool, args) in [
        ("fs_delete", json!({"path": "../x"})),
        ("fs_move", json!({"from": "moved/c.txt", "to": "/etc/c.txt"})),
        ("fs_copy", json!({"from": "/etc/hostname", "to": "h.txt"})),
        ("fs_mkdir", json!({"path": "/tmp/x"})),
    ] {
        assert_eq!(e.run(tool, args).await.unwrap_err().kind, ErrorKind::Policy, "{tool}");
    }
}

#[tokio::test]
async fn the_sandbox_shell_is_a_real_shell_with_exit_codes_timeouts_and_a_cwd() {
    let e = sandbox();
    e.write("sub/f.txt", "hello");
    let piped = e.run("shell", json!({"command": "echo hello world | tr a-z A-Z && ls | wc -l"})).await.unwrap();
    assert!(piped.ok && piped.content.starts_with("HELLO WORLD"), "{}", piped.content);
    let in_sub = e.run("shell", json!({"command": "cat f.txt; pwd", "cwd": "sub"})).await.unwrap().content;
    assert!(in_sub.starts_with("hello") && in_sub.trim_end().ends_with("/sub"), "{in_sub}");
    let fail = e.run("shell", json!({"command": "echo out; echo err >&2; exit 3"})).await.unwrap();
    assert!(
        !fail.ok
            && fail.exit_code == Some(3)
            && fail.content.contains("out")
            && fail.content.contains("[stderr] err")
            && fail.content.contains("[exit code 3]"),
        "{}",
        fail.content
    );
    let slow = e.run("shell", json!({"command": "sleep 5", "timeout_secs": 1})).await.unwrap_err();
    assert_eq!(slow.kind, ErrorKind::Timeout);
    // a redirect writes into the workspace; the cwd cannot leave it
    e.run("shell", json!({"command": "echo made > made.txt"})).await.unwrap();
    assert_eq!(e.read("made.txt").trim(), "made");
    assert_eq!(e.run("shell", json!({"command": "ls", "cwd": "/"})).await.unwrap_err().kind, ErrorKind::Policy);
    assert_eq!(e.run("shell", json!({"command": "  "})).await.unwrap_err().kind, ErrorKind::Policy);
    let m = e.reg.meta("shell").unwrap();
    assert!(m.description.contains("background=true") && m.params.iter().any(|p| p.name == "timeout_secs"));
}

#[tokio::test]
async fn background_jobs_can_be_read_and_stopped() {
    let e = sandbox();
    let started = e.run("shell", json!({"command": "echo booted; sleep 30", "background": true})).await.unwrap().content;
    let id: u64 = started.split_whitespace().find_map(|w| w.trim_end_matches(':').parse().ok()).expect(&started);
    let mut seen = String::new();
    for _ in 0..40 {
        seen = e.run("shell_output", json!({"job_id": id})).await.unwrap().content;
        if seen.contains("booted") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(seen.contains("running") && seen.contains("booted"), "{seen}");
    let again = e.run("shell_output", json!({"job_id": id})).await.unwrap().content;
    let body = again.split_once('\n').map(|x| x.1).unwrap_or("");
    assert!(!body.contains("booted"), "only new output is shown: {again}");
    assert!(e.run("shell_output", json!({"job_id": id, "all": true})).await.unwrap().content.contains("booted"));
    e.run("shell_kill", json!({"job_id": id})).await.unwrap();
    let after = e.run("shell_output", json!({"job_id": id})).await.unwrap().content;
    assert!(after.contains("exited with code -9"), "{after}");
    assert_eq!(e.run("shell_output", json!({"job_id": 9999})).await.unwrap_err().kind, ErrorKind::NotFound);
}

#[tokio::test]
async fn the_default_shell_stays_a_restricted_allowlist() {
    let e = env("");
    let m = e.reg.meta("shell").unwrap();
    assert!(m.description.contains("allowlisted") && m.params.len() == 1);
    let denied = e.run("shell", json!({"command": "echo hi | tr a-z A-Z"})).await;
    assert!(denied.is_err() || !denied.unwrap().content.contains("HI"), "no pipes without the sandbox mode");
}
