//! Native tools. Each tool re-checks the policy at execution time (defence in
//! depth) and returns compact, deterministic output.

use crate::policy::{PathGuard, PolicyDecision, PolicyEngine};
use crate::registry::ToolRegistry;
use crate::sandbox;
use async_trait::async_trait;
use dfae_core::*;
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Register every native tool.
pub fn register_native(reg: &mut ToolRegistry, policy: Arc<PolicyEngine>, memory: Arc<dyn MemoryStore>) {
    reg.register(Arc::new(TimeNow));
    reg.register(Arc::new(Calc));
    reg.register(Arc::new(SystemInfo { policy: policy.clone() }));
    reg.register(Arc::new(FsList { policy: policy.clone() }));
    reg.register(Arc::new(FsRead { policy: policy.clone() }));
    reg.register(Arc::new(FsWrite { policy: policy.clone() }));
    reg.register(Arc::new(FsSearch { policy: policy.clone() }));
    reg.register(Arc::new(Shell { policy: policy.clone() }));
    reg.register(Arc::new(HttpGet { policy: policy.clone() }));
    reg.register(Arc::new(crate::web::WebSearch { policy: policy.clone() }));
    reg.register(Arc::new(crate::web::Weather { policy }));
    reg.register(Arc::new(MemorySearch { memory: memory.clone() }));
    reg.register(Arc::new(MemoryStoreTool { memory }));
}

fn enforce(policy: &PolicyEngine, meta: &ToolMetadata, args: &Value) -> Result<(), ToolError> {
    match policy.check(meta, args, false) {
        PolicyDecision::Deny(m) => Err(ToolError::policy(m)),
        _ => Ok(()),
    }
}

const FILE_EXT: &str = "txt|md|json|toml|ya?ml|rs|py|js|ts|csv|log|conf|cfg|ini|sh|sql|html|xml|lock|env|c|h|go|java|rb|php|css";

fn meta(name: &str, description: &str, category: &str, params: Vec<ParamSpec>) -> ToolMetadata {
    ToolMetadata {
        name: name.into(),
        description: description.into(),
        category: category.into(),
        risk: Risk::Low,
        requires_approval: false,
        side_effects: false,
        reversible: true,
        cost_class: "free".into(),
        params,
        keywords: vec![],
        direct_patterns: vec![],
        cacheable: false,
        contract: vec![],
    }
}

fn pats(m: &mut ToolMetadata, p: &[String]) {
    m.direct_patterns = p.to_vec();
}

fn kw(m: &mut ToolMetadata, k: &[&str]) {
    m.keywords = k.iter().map(|s| s.to_string()).collect();
}

// ------------------------------------------------------------------ time ----

pub struct TimeNow;

#[async_trait]
impl Tool for TimeNow {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta("time_now", "current date and time (UTC)", "system", vec![]);
        m.contract = vec![Check::Matches { pattern: r"unix \d+".into() }];
        kw(&mut m, &["time", "date", "clock", "today", "now", "day"]);
        pats(&mut m, &[r"(?i)\b(?:current\s+(?:time|date)|what(?:'s|\s+is)\s+(?:the\s+)?(?:current\s+)?(?:time|date)|what\s+time\s+is\s+it|what\s+day\s+is\s+(?:it|today)|today'?s\s+date|time\s+now)\b".into()]);
        m
    }
    async fn execute(&self, _i: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        let n = chrono::Utc::now();
        Ok(ToolOutput::ok(format!("{} ({}, unix {})", n.format("%Y-%m-%dT%H:%M:%SZ"), n.format("%A"), n.timestamp())))
    }
}

// ------------------------------------------------------------------ calc ----

pub struct Calc;

#[async_trait]
impl Tool for Calc {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "calc",
            "evaluate an arithmetic expression exactly (+ - * / % ^, parentheses, sqrt, abs, round, pi)",
            "math",
            vec![ParamSpec::new("expression", ParamType::String, true, "arithmetic expression").hints(&["expression"])],
        );
        m.cacheable = true;
        m.contract = vec![Check::Matches { pattern: r" = -?\d".into() }];
        kw(&mut m, &["calculate", "compute", "math", "arithmetic", "sum", "multiply", "divide", "evaluate"]);
        let f = "sqrt|abs|sin|cos|tan|ln|log|round|floor|ceil|pi";
        pats(
            &mut m,
            &[
                format!(
                    r"(?i)\b(?:calculate|compute|evaluate|solve|what(?:'s|\s+is)|how\s+much\s+is)\s*:?\s*(?P<expression>(?:\d|\(|{f})(?:[\d\s.+\-*/^%x×÷()]|{f})*)\s*[?.!]?\s*$"
                ),
                r"^\s*(?P<expression>[\d.(][\d\s.()]*[+\-*/^%x×÷][\d\s.+\-*/^%()x×÷]*[\d)])\s*=?\s*\??\s*$".to_string(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        let e = arg_str(&input, "expression")?;
        match crate::calc::evaluate(e) {
            Ok(v) => Ok(ToolOutput::ok(format!("{} = {}", e.trim(), crate::calc::format_number(v)))),
            Err(msg) => Err(ToolError::args(format!("cannot evaluate '{e}': {msg}"))),
        }
    }
}

// ----------------------------------------------------------- system info ----

pub struct SystemInfo {
    policy: Arc<PolicyEngine>,
}

fn read_proc(p: &str) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

#[async_trait]
impl Tool for SystemInfo {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta("system_info", "host diagnostics: load, memory, uptime, disk", "system", vec![]);
        m.contract = vec![Check::Contains { value: "host:".into() }];
        kw(&mut m, &["system", "memory", "cpu", "disk", "uptime", "load", "resources", "diagnostics", "health", "server", "status"]);
        pats(&mut m, &[r"(?i)\b(?:system\s+(?:info|information|status|health|resources|diagnostics)|disk\s+(?:usage|space)|memory\s+usage|cpu\s+(?:load|usage)|(?:server|system)\s+load|uptime|resource\s+usage|how\s+much\s+(?:disk|memory|ram))\b".into()]);
        m
    }
    async fn execute(&self, _i: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let mut lines = Vec::new();
        let host = read_proc("/etc/hostname").or_else(|| read_proc("/proc/sys/kernel/hostname")).unwrap_or_default();
        let kernel = read_proc("/proc/sys/kernel/osrelease").unwrap_or_default();
        lines.push(format!("host: {} | kernel: {}", host.trim(), kernel.trim()));
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        if let Some(l) = read_proc("/proc/loadavg") {
            let p: Vec<&str> = l.split_whitespace().take(3).collect();
            lines.push(format!("load: {} (cpus: {cpus})", p.join(" ")));
        }
        if let Some(mi) = read_proc("/proc/meminfo") {
            let get = |k: &str| mi.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok());
            if let (Some(t), Some(a)) = (get("MemTotal:"), get("MemAvailable:")) {
                lines.push(format!("mem: total {:.1}G, available {:.1}G ({:.0}%)", t / 1048576.0, a / 1048576.0, 100.0 * a / t));
            }
        }
        if let Some(u) = read_proc("/proc/uptime").and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok())) {
            let s = u as u64;
            lines.push(format!("uptime: {}d {}h {}m", s / 86400, (s % 86400) / 3600, (s % 3600) / 60));
        }
        let ws = self.policy.workspace().to_string_lossy().to_string();
        if let Ok(r) = sandbox::run_argv(&["df".into(), "-h".into(), ws], self.policy.workspace(), Duration::from_secs(5), 8192, &ctx.cancel).await
            && let Some(l) = r.stdout.lines().nth(1)
        {
            lines.push(format!("disk(workspace): {}", l.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

// ------------------------------------------------------------ filesystem ----

pub struct FsList {
    policy: Arc<PolicyEngine>,
}

#[async_trait]
impl Tool for FsList {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_list",
            "list files and directories in a directory",
            "filesystem",
            vec![
                ParamSpec::new("path", ParamType::String, false, "directory (default: workspace root)").hints(&["file path", "directory"]).default(json!(".")),
            ],
        );
        m.cacheable = true;
        m.contract = vec![Check::Contains { value: "entries)".into() }];
        kw(&mut m, &["files", "directory", "folder", "list", "ls", "contents"]);
        let path = r"(?P<path>(?:\.{1,2}|~)?/[\w./\-]*|[\w\-]+/[\w./\-]*|\.)";
        pats(
            &mut m,
            &[
                format!(
                    r"(?i)\b(?:list|show|display|enumerate|ls)\b[^\n]*?\b(?:files|directory|directories|folder|folders|dir|contents)\b[^\n]*?\b(?:in|of|under|inside|at|from)\s+(?:the\s+)?(?:(?:folder|directory|dir)\s+)?{path}(?:\s|$|[?.!,])"
                ),
                r"(?i)\bls\s+(?P<path>[\w./~\-]+)".into(),
                r"(?i)\b(?:list|show|display|enumerate)\b[^\n]*?\b(?:files|directory|directories|folders?|contents)\b".into(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let raw = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let p = self.policy.guard().read_path(raw)?;
        let rd = tokio::fs::read_dir(&p).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::new(ErrorKind::NotFound, format!("no such directory: {raw}")),
            _ => ToolError::io(format!("{raw}: {e}")),
        })?;
        let mut rd = rd;
        let mut rows: Vec<(bool, String, u64)> = Vec::new();
        while let Some(e) = rd.next_entry().await.map_err(|e| ToolError::io(e.to_string()))? {
            let md = e.metadata().await.map_err(|e| ToolError::io(e.to_string()))?;
            rows.push((md.is_dir(), e.file_name().to_string_lossy().into_owned(), md.len()));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let total = rows.len();
        let mut out = vec![format!("{} ({total} entries)", raw)];
        for (d, n, sz) in rows.into_iter().take(200) {
            out.push(if d { format!("d {n}/") } else { format!("f {n} {sz}B") });
        }
        if total > 200 {
            out.push(format!("… {} more", total - 200));
        }
        Ok(ToolOutput::ok(out.join("\n")))
    }
}

pub struct FsRead {
    policy: Arc<PolicyEngine>,
}

#[async_trait]
impl Tool for FsRead {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_read",
            "read a UTF-8 text file",
            "filesystem",
            vec![
                ParamSpec::new("path", ParamType::String, true, "file path").hints(&["file path"]),
                ParamSpec::new("max_bytes", ParamType::Integer, false, "read at most N bytes (default 20000)"),
            ],
        );
        m.cacheable = true;
        kw(&mut m, &["read", "file", "cat", "open", "contents", "text", "view"]);
        pats(
            &mut m,
            &[format!(r"(?i)\b(?:read|cat|open|show|print|display|view|get)\b[^\n]*?(?P<path>(?:\.{{0,2}}/)?(?:[\w.\-]+/)*[\w\-]+\.(?:{FILE_EXT}))\b")],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let raw = arg_str(&input, "path")?;
        let max = input.get("max_bytes").and_then(|v| v.as_u64()).unwrap_or(20_000).min(200_000) as usize;
        let p = self.policy.guard().read_path(raw)?;
        let bytes = tokio::fs::read(&p).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::new(ErrorKind::NotFound, format!("no such file: {raw}")),
            _ => ToolError::io(format!("{raw}: {e}")),
        })?;
        if bytes.iter().take(4096).any(|b| *b == 0) {
            return Err(ToolError::args(format!("{raw} looks like a binary file")));
        }
        let truncated = bytes.len() > max;
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(max)]).into_owned();
        Ok(ToolOutput::ok(if truncated { format!("{text}\n…[truncated at {max} of {} bytes]", bytes.len()) } else { text }))
    }
}

pub struct FsWrite {
    policy: Arc<PolicyEngine>,
}

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

#[async_trait]
impl Tool for FsWrite {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_write",
            "write (or append) text to a file in the workspace; requires approval",
            "filesystem",
            vec![
                ParamSpec::new("path", ParamType::String, true, "file path").hints(&["file path"]),
                ParamSpec::new("content", ParamType::String, true, "text to write").hints(&["quoted"]),
                ParamSpec::new("append", ParamType::Boolean, false, "append instead of overwrite"),
            ],
        );
        m.risk = Risk::Moderate;
        m.side_effects = true;
        m.reversible = false;
        kw(&mut m, &["write", "save", "create", "file", "append", "store"]);
        pats(
            &mut m,
            &[
                format!(
                    r#"(?i)\b(?:write|save|put|store)\s+["'`](?P<content>[^"'`]+)["'`]\s+(?:to|into|in|as)\s+(?:the\s+)?(?:file\s+)?(?P<path>[\w./\-]+\.(?:{FILE_EXT}))"#
                ),
                // "create a file named "x.txt" with content 'hi'": the path is named here, the content is the quoted entity
                format!(
                    r#"(?i)\b(?:create|make|write|generate|add)\s+(?:(?:a|an|new|text)\s+)*file\s+(?:called\s+|named\s+)?["'`]?(?P<path>[\w./\-]+\.(?:{FILE_EXT}))["'`]?(?:\s|$|[.,;!?])"#
                ),
                // anything else that says "create/write ... file": path and content come from the entities
                format!(r#"(?i)\b(?:create|make|write|save|generate)\b[^\n]*?\b(?:file|[\w./\-]+\.(?:{FILE_EXT}))\b"#),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let raw = arg_str(&input, "path")?;
        let content = arg_str(&input, "content")?;
        let append = input.get("append").and_then(|v| v.as_bool()).unwrap_or(false);
        let p = self.policy.guard().write_path(raw)?;
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| ToolError::io(e.to_string()))?;
        }
        if append {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&p).await.map_err(|e| ToolError::io(e.to_string()))?;
            f.write_all(content.as_bytes()).await.map_err(|e| ToolError::io(e.to_string()))?;
        } else {
            tokio::fs::write(&p, content).await.map_err(|e| ToolError::io(e.to_string()))?;
        }
        Ok(ToolOutput::ok(format!("wrote {} bytes to {raw}", content.len())))
    }
    async fn verify(&self, input: &Value, _o: &ToolOutput, _c: &ToolContext) -> Option<Result<String, String>> {
        let raw = input.get("path")?.as_str()?;
        let content = input.get("content")?.as_str()?;
        let append = input.get("append").and_then(|v| v.as_bool()).unwrap_or(false);
        let p = self.policy.guard().read_path(raw).ok()?;
        Some(match tokio::fs::read(&p).await {
            Err(e) => Err(format!("read-back failed: {e}")),
            Ok(b) if append => {
                if String::from_utf8_lossy(&b).ends_with(content) {
                    Ok(format!("{raw} ends with the appended text ({} bytes total)", b.len()))
                } else {
                    Err(format!("{raw} does not end with the appended text"))
                }
            }
            Ok(b) if b == content.as_bytes() => Ok(format!("read-back sha256 {} matches ({} bytes)", &sha(&b)[..12], b.len())),
            Ok(_) => Err(format!("{raw} content differs from what was written")),
        })
    }
}

pub struct FsSearch {
    policy: Arc<PolicyEngine>,
}

fn walk_search(guard: &PathGuard, root: &Path, re: &Regex, max_results: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut scanned = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), ".git" | "target" | "node_modules" | ".env") {
                continue;
            }
            let path = e.path();
            if guard.read_path(&path.to_string_lossy()).is_err() {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                if depth < 8 {
                    stack.push((path, depth + 1));
                }
            } else if md.is_file() && md.len() < 1_000_000 {
                scanned += 1;
                if scanned > 5000 {
                    return out;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let rel = path.strip_prefix(&guard.workspace).unwrap_or(&path).to_string_lossy().into_owned();
                        out.push(format!("{rel}:{}: {}", i + 1, line.trim().chars().take(160).collect::<String>()));
                        if out.len() >= max_results {
                            return out;
                        }
                    }
                }
            }
        }
    }
    out
}

#[async_trait]
impl Tool for FsSearch {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_search",
            "search text files for a regex (grep -rn)",
            "filesystem",
            vec![
                ParamSpec::new("pattern", ParamType::String, true, "regular expression"),
                ParamSpec::new("path", ParamType::String, false, "directory to search (default: workspace)")
                    .hints(&["file path", "directory"])
                    .default(json!(".")),
                ParamSpec::new("max_results", ParamType::Integer, false, "default 50"),
            ],
        );
        m.cacheable = true;
        kw(&mut m, &["search", "grep", "find", "text", "occurrences", "match"]);
        pats(
            &mut m,
            &[
                r#"(?i)\b(?:search|grep|find|look)\b[^\n]*?\bfor\s+["'`](?P<pattern>[^"'`]+)["'`](?:[^\n]*?\b(?:in|under|inside)\s+(?:the\s+)?(?P<path>[\w./~\-]*[/.][\w./\-]*))?"#.into(),
                r#"(?i)\bgrep\s+["'`](?P<pattern>[^"'`]+)["'`]\s*(?P<path>[\w./~\-]*[/.][\w./\-]*)?"#.into(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let pat = arg_str(&input, "pattern")?;
        let re = Regex::new(&format!("(?i){pat}")).map_err(|e| ToolError::args(format!("bad pattern: {e}")))?;
        let raw = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let max = input.get("max_results").and_then(|v| v.as_u64()).unwrap_or(50).min(500) as usize;
        let guard = self.policy.guard();
        let root = guard.read_path(raw)?;
        let hits = tokio::task::spawn_blocking(move || walk_search(&guard, &root, &re, max)).await.map_err(|e| ToolError::other(e.to_string()))?;
        let _ = ctx;
        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!("no matches for '{pat}' in {raw}")));
        }
        Ok(ToolOutput::ok(format!("{} match(es)\n{}", hits.len(), hits.join("\n"))))
    }
}

// ----------------------------------------------------------------- shell ----

pub struct Shell {
    policy: Arc<PolicyEngine>,
}

#[async_trait]
impl Tool for Shell {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "shell",
            "run one allowlisted command (no shell, no pipes), e.g. `df -h`, `ls -la`, `grep -n x file`",
            "system",
            vec![ParamSpec::new("command", ParamType::String, true, "command line, e.g. 'df -h'")],
        );
        m.risk = Risk::Moderate;
        kw(&mut m, &["run", "command", "shell", "execute", "terminal", "bash"]);
        pats(
            &mut m,
            &[
                r"(?i)\b(?:run|execute|exec)\b\s*(?:the\s+)?(?:shell\s+)?(?:command\s*)?:?\s*`(?P<command>[^`]+)`".into(),
                r"^\s*\$\s*(?P<command>.+?)\s*$".into(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let meta = self.metadata();
        enforce(&self.policy, &meta, &input)?;
        let cmd = arg_str(&input, "command")?;
        let argv = shlex::split(cmd).ok_or_else(|| ToolError::args("unparseable command"))?;
        let cfg = &self.policy.config().shell;
        let r = sandbox::run_argv(&argv, self.policy.workspace(), Duration::from_secs(cfg.timeout_secs), cfg.max_output_bytes, &ctx.cancel).await?;
        let mut out = r.stdout;
        if !r.stderr.trim().is_empty() {
            out.push_str(&format!("\n[stderr] {}", r.stderr.trim()));
        }
        if r.truncated {
            out.push_str("\n…[output truncated]");
        }
        let code = r.code.unwrap_or(-1);
        Ok(ToolOutput { content: out.trim().to_string(), ok: code == 0, exit_code: Some(code), evidence: vec![format!("exit code {code}")] })
    }
}

// ------------------------------------------------------------------ http ----

pub struct HttpGet {
    policy: Arc<PolicyEngine>,
}

#[async_trait]
impl Tool for HttpGet {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "http_get",
            "HTTP GET a URL (allowlisted domains) and return its text",
            "web",
            vec![ParamSpec::new("url", ParamType::String, true, "http(s) URL").hints(&["url"])],
        );
        m.contract = vec![Check::Matches { pattern: r"^HTTP \d{3}".into() }];
        kw(&mut m, &["fetch", "download", "web", "url", "http", "website", "page", "internet", "browse"]);
        pats(
            &mut m,
            &[
                r#"(?i)\b(?:fetch|get|download|open|visit|read|curl|load|retrieve|check)\b[^\n]*?(?P<url>https?://[^\s"'<>)]+)"#.into(),
                r"^\s*(?P<url>https?://\S+)\s*$".into(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let url = arg_str(&input, "url")?.to_string();
        let f = crate::web::fetch(&self.policy, &self.metadata(), &url, &[], &ctx.cancel, false).await?;
        if f.status == 404 {
            return Err(ToolError::new(ErrorKind::NotFound, format!("HTTP 404 for {url}")));
        }
        if f.status == 429 || f.status >= 500 {
            return Err(ToolError::new(ErrorKind::Transient, format!("HTTP {} for {url}", f.status)));
        }
        let content = format!("HTTP {} {}\n{}{}", f.status, f.ctype, f.text, if f.truncated { "\n…[truncated]" } else { "" });
        Ok(ToolOutput { content, ok: (200..300).contains(&f.status), exit_code: Some(f.status as i32), evidence: vec![format!("HTTP {}", f.status)] })
    }
}

// ---------------------------------------------------------------- memory ----

pub struct MemorySearch {
    memory: Arc<dyn MemoryStore>,
}

#[async_trait]
impl Tool for MemorySearch {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "memory_search",
            "search the agent's long-term memory (facts, past tasks, skills)",
            "memory",
            vec![ParamSpec::new("query", ParamType::String, true, "what to look for"), ParamSpec::new("limit", ParamType::Integer, false, "default 5")],
        );
        kw(&mut m, &["memory", "recall", "remember", "know", "knowledge", "past"]);
        pats(&mut m, &[r"(?i)\b(?:what\s+do\s+you\s+(?:remember|know)\s+about|recall|search\s+(?:your\s+)?memory\s+for|remind\s+me\s+(?:about|of))\s+(?P<query>[^?\n]+)".into()]);
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        let q = arg_str(&input, "query")?;
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(5).min(20) as usize;
        let r = self.memory.retrieve(MemoryQuery { text: q.into(), kinds: vec![], limit, session_id: None }).await.map_err(|e| ToolError::io(e.to_string()))?;
        let r: Vec<_> = r.into_iter().filter(|m| m.similarity > 0.15).collect();
        if r.is_empty() {
            return Ok(ToolOutput::ok(format!("no relevant memories for '{q}'")));
        }
        let lines: Vec<String> =
            r.iter().map(|m| format!("[{}] {} (sim {:.2})", m.item.kind.as_str(), tokens::truncate_tokens(&m.item.text, 80), m.similarity)).collect();
        Ok(ToolOutput::ok(lines.join("\n")))
    }
}

pub struct MemoryStoreTool {
    memory: Arc<dyn MemoryStore>,
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "memory_store",
            "remember a fact in long-term memory",
            "memory",
            vec![ParamSpec::new("text", ParamType::String, true, "the fact to remember")],
        );
        m.side_effects = true;
        kw(&mut m, &["remember", "store", "memorize", "note", "save", "fact"]);
        pats(&mut m, &[r"(?i)^\s*(?:please\s+)?(?:remember|memorize|note)\s+(?:that\s+)?(?P<text>.{3,})$".into()]);
        m
    }
    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let text = arg_str(&input, "text")?;
        let mut item = MemoryItem::new(MemoryKind::Semantic, text, 0.6);
        item.metadata = json!({"source": "tool:memory_store", "task_id": ctx.task_id});
        let id = item.id.clone();
        self.memory.store(item).await.map_err(|e| ToolError::io(e.to_string()))?;
        Ok(ToolOutput::ok(format!("stored memory {id}")))
    }
    async fn verify(&self, input: &Value, output: &ToolOutput, _c: &ToolContext) -> Option<Result<String, String>> {
        let id = output.content.strip_prefix("stored memory ")?.trim().to_string();
        let text = input.get("text")?.as_str()?;
        let r = self.memory.retrieve(MemoryQuery { text: text.into(), kinds: vec![MemoryKind::Semantic], limit: 5, session_id: None }).await.ok()?;
        Some(if r.iter().any(|m| m.item.id == id) {
            Ok(format!("memory {} is retrievable", &id[..8]))
        } else {
            Err("stored memory could not be retrieved".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PolicyConfig;
    use dfae_memory::InMemoryStore;

    fn setup() -> (ToolRegistry, tempfile::TempDir, ToolContext) {
        let d = tempfile::tempdir().unwrap();
        let policy = Arc::new(PolicyEngine::new(PolicyConfig::default(), d.path().to_path_buf()));
        let mut r = ToolRegistry::new();
        register_native(&mut r, policy.clone(), Arc::new(InMemoryStore::new()));
        let ctx = ToolContext {
            task_id: "t".into(),
            step_id: "s".into(),
            workspace: policy.workspace().to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        (r, d, ctx)
    }

    #[tokio::test]
    async fn fs_roundtrip_and_verify() {
        let (r, d, ctx) = setup();
        let w = r.get("fs_write").unwrap();
        let a = json!({"path": "notes/a.txt", "content": "hello\nworld"});
        let out = w.execute(a.clone(), ctx.clone()).await.unwrap();
        assert!(w.verify(&a, &out, &ctx).await.unwrap().is_ok());
        assert!(d.path().join("notes/a.txt").exists());
        let read = r.get("fs_read").unwrap().execute(json!({"path": "notes/a.txt"}), ctx.clone()).await.unwrap();
        assert_eq!(read.content, "hello\nworld");
        let list = r.get("fs_list").unwrap().execute(json!({"path": "."}), ctx.clone()).await.unwrap();
        assert!(list.content.contains("d notes/"));
        let s = r.get("fs_search").unwrap().execute(json!({"pattern": "wor.d"}), ctx.clone()).await.unwrap();
        assert!(s.content.contains("notes/a.txt:2"), "{}", s.content);
    }

    #[tokio::test]
    async fn tools_enforce_policy_at_execution() {
        let (r, _d, ctx) = setup();
        let e = r.get("fs_read").unwrap().execute(json!({"path": "/etc/passwd"}), ctx.clone()).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Policy);
        let e = r.get("shell").unwrap().execute(json!({"command": "rm -rf /"}), ctx.clone()).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Policy);
        let e = r.get("http_get").unwrap().execute(json!({"url": "http://127.0.0.1:1/"}), ctx).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Policy);
    }

    #[tokio::test]
    async fn shell_calc_time_system() {
        let (r, _d, ctx) = setup();
        let o = r.get("shell").unwrap().execute(json!({"command": "echo hi there"}), ctx.clone()).await.unwrap();
        assert_eq!(o.content, "hi there");
        assert!(o.ok);
        let o = r.get("calc").unwrap().execute(json!({"expression": "(3+4)*12"}), ctx.clone()).await.unwrap();
        assert_eq!(o.content, "(3+4)*12 = 84");
        assert!(r.get("time_now").unwrap().execute(json!({}), ctx.clone()).await.unwrap().content.contains("unix"));
        let o = r.get("system_info").unwrap().execute(json!({}), ctx).await.unwrap();
        assert!(o.content.contains("load:") || o.content.contains("host:"), "{}", o.content);
    }

    #[tokio::test]
    async fn memory_tools_roundtrip() {
        let (r, _d, ctx) = setup();
        let o = r.get("memory_store").unwrap().execute(json!({"text": "the staging database lives on db-staging"}), ctx.clone()).await.unwrap();
        let v = r.get("memory_store").unwrap().verify(&json!({"text": "the staging database lives on db-staging"}), &o, &ctx).await.unwrap();
        assert!(v.is_ok());
        let s = r.get("memory_search").unwrap().execute(json!({"query": "where is the staging database"}), ctx).await.unwrap();
        assert!(s.content.contains("db-staging"), "{}", s.content);
    }

    #[test]
    fn html_is_stripped() {
        assert_eq!(crate::web::html_to_text("<html><script>x()</script><body><h1>Hi</h1> there &amp; you</body></html>"), "Hi there & you");
    }
}
