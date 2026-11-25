//! The tools a coding agent lives on: precise edits, finding files, a real shell, background jobs.
//!
//! Every file tool resolves its paths through the policy's `PathGuard` (workspace roots, symlink-safe,
//! protected names) both when the policy is evaluated and again here. Descriptions say *how* to use each
//! tool because they are all a model sees of it; error messages say how to fix the call, because on a
//! failure they are what the planner reads next.

use crate::native::{FILE_EXT, enforce, kw, meta, pats};
use crate::policy::{PolicyEngine, ShellMode};
use crate::sandbox;
use async_trait::async_trait;
use dfae_core::*;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Directories nobody wants in a search or a listing.
pub(crate) fn skipped_dir(name: &str) -> bool {
    matches!(name, ".git" | "target" | "node_modules" | ".venv" | "__pycache__" | ".mypy_cache" | ".pytest_cache" | ".next" | "dist" | "build" | ".env")
}

fn rel(guard_ws: &Path, p: &Path) -> String {
    p.strip_prefix(guard_ws).unwrap_or(p).to_string_lossy().into_owned()
}

// ------------------------------------------------------------------ glob ----

/// `**/*.rs`, `src/**/test_*.py`, `*.{ts,tsx}`, `docs/?.md` → an anchored regex over `/`-separated relative paths.
/// A pattern without a `/` matches the file name at any depth, like a `.gitignore` entry.
pub(crate) fn glob_to_regex(glob: &str) -> Result<Regex, String> {
    let g = glob.trim().trim_start_matches("./");
    let g = if g.contains('/') { g.to_string() } else { format!("**/{g}") };
    let c: Vec<char> = g.chars().collect();
    let mut re = String::from("^");
    let (mut i, mut brace) = (0, 0);
    while i < c.len() {
        match c[i] {
            '*' if c.get(i + 1) == Some(&'*') => {
                if c.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '{' => {
                brace += 1;
                re.push_str("(?:");
            }
            '}' if brace > 0 => {
                brace -= 1;
                re.push(')');
            }
            ',' if brace > 0 => re.push('|'),
            '[' => {
                let end = c[i..].iter().position(|x| *x == ']').map(|p| i + p);
                match end {
                    Some(e) => {
                        let class: String = c[i..=e].iter().collect();
                        re.push_str(&class.replacen("[!", "[^", 1));
                        i = e + 1;
                        continue;
                    }
                    None => re.push_str("\\["),
                }
            }
            ch => re.push_str(&regex::escape(&ch.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).map_err(|e| format!("bad glob '{glob}': {e}"))
}

/// Walk `root` (skipping vendored/build directories and anything the guard refuses) and call `f(path, metadata)` per file.
pub(crate) fn walk_files(
    guard: &crate::policy::PathGuard,
    root: &Path,
    max_depth: usize,
    max_entries: usize,
    mut f: impl FnMut(&Path, &std::fs::Metadata) -> bool,
) {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
        for e in entries {
            seen += 1;
            if seen > max_entries {
                return;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
            if md.is_dir() {
                if depth < max_depth && !skipped_dir(&name) && guard.read_path(&path.to_string_lossy()).is_ok() {
                    stack.push((path, depth + 1));
                }
            } else if md.is_file() && !skipped_dir(&name) && guard.read_path(&path.to_string_lossy()).is_ok() && !f(&path, &md) {
                return;
            }
        }
    }
}

pub struct FsGlob {
    pub(crate) policy: Arc<PolicyEngine>,
}

#[async_trait]
impl Tool for FsGlob {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_glob",
            "find files by glob pattern, newest first: `**/*.rs`, `src/**/test_*.py`, `*.{ts,tsx}`; a pattern without `/` matches names at any depth. Use it to locate files before fs_read/fs_edit",
            "filesystem",
            vec![
                ParamSpec::new("pattern", ParamType::String, true, "glob pattern"),
                ParamSpec::new("path", ParamType::String, false, "directory to search (default: workspace)").hints(&["directory"]).default(json!(".")),
                ParamSpec::new("max_results", ParamType::Integer, false, "default 200"),
            ],
        );
        m.cacheable = true;
        kw(&mut m, &["find", "files", "glob", "locate", "pattern", "extension", "matching", "list", "all"]);
        pats(
            &mut m,
            &[
                r#"(?i)\b(?:find|list|show|glob)\b[^\n]*?\bfiles?\b[^\n]*?\b(?:matching|named|called|like|ending\s+in|with\s+extension)\s+["'`]?(?P<pattern>[\w*?./{},\[\]\-]+)["'`]?"#.into(),
                r#"(?i)\bglob\s+["'`]?(?P<pattern>[\w*?./{},\[\]\-]+)["'`]?"#.into(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let pat = arg_str(&input, "pattern")?;
        let re = glob_to_regex(pat).map_err(ToolError::args)?;
        let raw = input.get("path").and_then(|v| v.as_str()).filter(|p| !p.is_empty()).unwrap_or(".");
        let max = input.get("max_results").and_then(|v| v.as_u64()).unwrap_or(200).clamp(1, 2000) as usize;
        let guard = self.policy.guard();
        let root = guard.read_path(raw)?;
        let ws = guard.workspace.clone();
        let hits = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<(std::time::SystemTime, String)> = Vec::new();
            walk_files(&guard, &root, 14, 50_000, |p, md| {
                let r = p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/");
                if re.is_match(&r) {
                    hits.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), rel(&ws, p)));
                }
                true
            });
            hits
        })
        .await
        .map_err(|e| ToolError::other(e.to_string()))?;
        let mut hits = hits;
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = hits.len();
        if total == 0 {
            return Ok(ToolOutput::ok(format!("no files match '{pat}' in {raw}")));
        }
        let shown: Vec<String> = hits.into_iter().take(max).map(|(_, p)| p).collect();
        let more = if total > shown.len() { format!("\n… {} more", total - shown.len()) } else { String::new() };
        Ok(ToolOutput::ok(format!("{total} file(s)\n{}{more}", shown.join("\n"))))
    }
}

// --------------------------------------------------------------- fs_edit ----

pub struct FsEdit {
    pub(crate) policy: Arc<PolicyEngine>,
}

#[async_trait]
impl Tool for FsEdit {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_edit",
            "change part of an existing file: replace old_string (copied exactly from fs_read, including indentation) with new_string. Fails if old_string is missing or occurs more than once (add surrounding lines, or set replace_all=true). Prefer it to rewriting the file with fs_write",
            "filesystem",
            vec![
                ParamSpec::new("path", ParamType::String, true, "file to edit").hints(&["file path"]),
                ParamSpec::new("old_string", ParamType::String, true, "exact text to replace"),
                ParamSpec::new("new_string", ParamType::String, true, "replacement text"),
                ParamSpec::new("replace_all", ParamType::Boolean, false, "replace every occurrence (default: must be unique)"),
            ],
        );
        m.risk = Risk::Moderate;
        m.side_effects = true;
        m.reversible = false;
        kw(
            &mut m,
            &[
                "edit",
                "replace",
                "change",
                "modify",
                "update",
                "fix",
                "rename",
                "refactor",
                "patch",
                "substitute",
                "swap",
                "alter",
                "correct",
                "word",
                "text",
                "occurrence",
                "occurrences",
                "string",
            ],
        );
        // "replace [all] [the word] "A" with "B" in FILE" — quoted or bare words, FILE before or after;
        // a phrase with all/every/each replaces every occurrence, otherwise the text must be unique
        let verb = r#"(?:replace|change|substitute|swap)"#;
        let with = r#"(?:with|to|by|for|into)"#;
        let kind = r#"(?:the\s+)?(?:(?:word|text|string|phrase|term|name)\s+)?"#;
        let place = format!(r#"(?:in|inside|within|from|of)\s+(?:the\s+)?(?:file\s+)?["'`]?(?P<path>[\w./\-]+\.(?:{FILE_EXT}))"#);
        let quoted = |all: bool| {
            let all = if all { r#"(?P<replace_all>all|every|each)\s+(?:the\s+)?(?:occurrences?\s+of\s+)?"# } else { "" };
            format!(r#"(?i)\b{verb}\s+{all}{kind}["'`](?P<old_string>[^"'`]+)["'`]\s+{with}\s+["'`](?P<new_string>[^"'`]+)["'`]\s+{place}"#)
        };
        pats(
            &mut m,
            &[
                quoted(true),
                quoted(false),
                // file first: `in a.txt, replace "A" with "B"`
                format!(
                    r#"(?i)\b(?:in|inside|within)\s+(?:the\s+)?(?:file\s+)?["'`]?(?P<path>[\w./\-]+\.(?:{FILE_EXT}))["'`]?\s*[,:]?\s*{verb}\s+{kind}["'`](?P<old_string>[^"'`]+)["'`]\s+{with}\s+["'`](?P<new_string>[^"'`]+)["'`]"#
                ),
                // bare single words: `replace foo with bar in a.txt`
                format!(r#"(?i)\b{verb}\s+{kind}(?P<old_string>[^\s"'`]+)\s+{with}\s+(?P<new_string>[^\s"'`]+)\s+{place}"#),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let raw = arg_str(&input, "path")?;
        let old = arg_str(&input, "old_string")?;
        let new = arg_str(&input, "new_string")?;
        let all = input.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        if old.is_empty() {
            return Err(ToolError::args("old_string is empty; to create or overwrite a whole file use fs_write"));
        }
        if old == new {
            return Err(ToolError::args("old_string and new_string are identical: nothing to change"));
        }
        let p = self.policy.guard().write_path(raw)?;
        let text = tokio::fs::read_to_string(&p).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::new(ErrorKind::NotFound, format!("no such file: {raw} (create it with fs_write)")),
            _ => ToolError::io(format!("{raw}: {e}")),
        })?;
        let n = text.matches(old).count();
        if n == 0 {
            return Err(ToolError::args(format!(
                "old_string was not found in {raw}: it must match the file exactly, whitespace included — fs_read the file and copy the lines"
            )));
        }
        if n > 1 && !all {
            return Err(ToolError::args(format!(
                "old_string occurs {n} times in {raw}: include more surrounding lines to make it unique, or set replace_all=true"
            )));
        }
        let first_line = text[..text.find(old).unwrap_or(0)].matches('\n').count() + 1;
        let updated = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        tokio::fs::write(&p, &updated).await.map_err(|e| ToolError::io(e.to_string()))?;
        Ok(ToolOutput::ok(format!("edited {raw}: {} replacement(s), first at line {first_line}", if all { n } else { 1 })))
    }
    async fn verify(&self, input: &Value, _o: &ToolOutput, _c: &ToolContext) -> Option<Result<String, String>> {
        let (raw, new) = (input.get("path")?.as_str()?, input.get("new_string")?.as_str()?);
        let p = self.policy.guard().read_path(raw).ok()?;
        Some(match tokio::fs::read_to_string(&p).await {
            Ok(t) if new.is_empty() || t.contains(new) => Ok(format!("{raw} contains the new text")),
            Ok(_) => Err(format!("{raw} does not contain the new text after the edit")),
            Err(e) => Err(format!("read-back failed: {e}")),
        })
    }
}

// ----------------------------------------------------- mkdir / mv / cp / rm ----

fn dir_copy(from: &Path, to: &Path) -> std::io::Result<u64> {
    let mut n = 0;
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let t = e.file_type()?;
        if t.is_symlink() {
            continue; // a link could point out of the workspace
        } else if t.is_dir() {
            n += dir_copy(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
            n += 1;
        }
    }
    Ok(n)
}

macro_rules! simple_fs_tool {
    ($name:ident) => {
        pub struct $name {
            pub(crate) policy: Arc<PolicyEngine>,
        }
    };
}
simple_fs_tool!(FsMkdir);
simple_fs_tool!(FsMove);
simple_fs_tool!(FsCopy);
simple_fs_tool!(FsDelete);

#[async_trait]
impl Tool for FsMkdir {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_mkdir",
            "create a directory (and missing parents); fine if it already exists",
            "filesystem",
            vec![ParamSpec::new("path", ParamType::String, true, "directory to create").hints(&["directory", "file path"])],
        );
        m.risk = Risk::Moderate;
        m.side_effects = true;
        kw(&mut m, &["mkdir", "directory", "folder", "create", "make"]);
        pats(
            &mut m,
            &[
                r#"(?i)\b(?:create|make|add)\s+(?:(?:a|the|new)\s+)*(?:directory|folder|dir)\s+(?:called\s+|named\s+)?["'`]?(?P<path>[\w./\-]+)["'`]?"#.into(),
                r#"(?i)\bmkdir\s+(?:-p\s+)?["'`]?(?P<path>[\w./\-]+)["'`]?"#.into(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let raw = arg_str(&input, "path")?;
        let p = self.policy.guard().write_path(raw)?;
        tokio::fs::create_dir_all(&p).await.map_err(|e| ToolError::io(format!("{raw}: {e}")))?;
        Ok(ToolOutput::ok(format!("created directory {raw}")))
    }
}

#[async_trait]
impl Tool for FsMove {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_move",
            "move or rename a file or directory (parents of `to` are created); fails if `to` exists",
            "filesystem",
            vec![ParamSpec::new("from", ParamType::String, true, "existing path"), ParamSpec::new("to", ParamType::String, true, "new path")],
        );
        m.risk = Risk::Moderate;
        m.side_effects = true;
        m.reversible = false;
        kw(&mut m, &["move", "rename", "mv", "relocate"]);
        pats(
            &mut m,
            &[r#"(?i)\b(?:move|rename|mv)\s+(?:the\s+)?(?:file\s+|directory\s+|folder\s+)?["'`]?(?P<from>[\w./\-]+)["'`]?\s+(?:to|into|as)\s+["'`]?(?P<to>[\w./\-]+)["'`]?"#.into()],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let (from, to) = (arg_str(&input, "from")?, arg_str(&input, "to")?);
        let g = self.policy.guard();
        let (src, dst) = (g.write_path(from)?, g.write_path(to)?);
        if !src.exists() {
            return Err(ToolError::new(ErrorKind::NotFound, format!("no such file or directory: {from}")));
        }
        if dst.exists() {
            return Err(ToolError::args(format!("{to} already exists: delete it first (fs_delete) or choose another name")));
        }
        if let Some(parent) = dst.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| ToolError::io(e.to_string()))?;
        }
        tokio::fs::rename(&src, &dst).await.map_err(|e| ToolError::io(format!("{from} -> {to}: {e}")))?;
        Ok(ToolOutput::ok(format!("moved {from} to {to}")))
    }
}

#[async_trait]
impl Tool for FsCopy {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_copy",
            "copy a file, or a directory with everything in it (symlinks are skipped); fails if `to` exists",
            "filesystem",
            vec![ParamSpec::new("from", ParamType::String, true, "existing path"), ParamSpec::new("to", ParamType::String, true, "destination path")],
        );
        m.risk = Risk::Moderate;
        m.side_effects = true;
        kw(&mut m, &["copy", "cp", "duplicate", "backup"]);
        pats(
            &mut m,
            &[r#"(?i)\b(?:copy|cp|duplicate)\s+(?:the\s+)?(?:file\s+|directory\s+|folder\s+)?["'`]?(?P<from>[\w./\-]+)["'`]?\s+(?:to|into|as)\s+["'`]?(?P<to>[\w./\-]+)["'`]?"#.into()],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let (from, to) = (arg_str(&input, "from")?, arg_str(&input, "to")?);
        let g = self.policy.guard();
        let (src, dst) = (g.read_path(from)?, g.write_path(to)?);
        if !src.exists() {
            return Err(ToolError::new(ErrorKind::NotFound, format!("no such file or directory: {from}")));
        }
        if dst.exists() {
            return Err(ToolError::args(format!("{to} already exists: delete it first (fs_delete) or choose another name")));
        }
        let n = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if src.is_dir() { dir_copy(&src, &dst) } else { std::fs::copy(&src, &dst).map(|_| 1) }
        })
        .await
        .map_err(|e| ToolError::other(e.to_string()))?
        .map_err(|e| ToolError::io(format!("{from} -> {to}: {e}")))?;
        Ok(ToolOutput::ok(format!("copied {from} to {to} ({n} file(s))")))
    }
}

#[async_trait]
impl Tool for FsDelete {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "fs_delete",
            "delete a file; a directory needs recursive=true (an empty one does not). Cannot delete the workspace root",
            "filesystem",
            vec![
                ParamSpec::new("path", ParamType::String, true, "file or directory to delete").hints(&["file path", "directory"]),
                ParamSpec::new("recursive", ParamType::Boolean, false, "delete a directory and everything in it"),
            ],
        );
        m.risk = Risk::High;
        m.side_effects = true;
        m.reversible = false;
        kw(&mut m, &["delete", "remove", "rm", "erase", "clean"]);
        pats(
            &mut m,
            &[
                format!(r#"(?i)\b(?:delete|remove|rm)\s+(?:the\s+)?(?:file\s+)?["'`]?(?P<path>[\w./\-]+\.(?:{FILE_EXT}))["'`]?\s*[.!?]?\s*$"#),
                r#"(?i)\b(?:delete|remove)\s+(?:the\s+)?(?:directory|folder|dir)\s+["'`]?(?P<path>[\w./\-]+)["'`]?\s*[.!?]?\s*$"#.into(),
            ],
        );
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        enforce(&self.policy, &self.metadata(), &input)?;
        let raw = arg_str(&input, "path")?;
        let recursive = input.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
        let g = self.policy.guard();
        let p = g.write_path(raw)?;
        if p == g.workspace {
            return Err(ToolError::policy("the workspace root cannot be deleted"));
        }
        let md = tokio::fs::symlink_metadata(&p).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::new(ErrorKind::NotFound, format!("no such file or directory: {raw}")),
            _ => ToolError::io(format!("{raw}: {e}")),
        })?;
        if md.is_dir() {
            if !recursive {
                return Err(ToolError::args(format!("{raw} is a directory: set recursive=true to delete it and its contents")));
            }
            tokio::fs::remove_dir_all(&p).await.map_err(|e| ToolError::io(e.to_string()))?;
        } else {
            tokio::fs::remove_file(&p).await.map_err(|e| ToolError::io(e.to_string()))?;
        }
        Ok(ToolOutput::ok(format!("deleted {raw}")))
    }
    async fn verify(&self, input: &Value, _o: &ToolOutput, _c: &ToolContext) -> Option<Result<String, String>> {
        let raw = input.get("path")?.as_str()?;
        let p = self.policy.guard().write_path(raw).ok()?;
        Some(if p.exists() { Err(format!("{raw} still exists")) } else { Ok(format!("{raw} is gone")) })
    }
}

// ------------------------------------------------------------------ shell ----

fn shell_bin() -> &'static str {
    if Path::new("/bin/bash").exists() || Path::new("/usr/bin/bash").exists() { "bash" } else { "sh" }
}

/// Run `script` with the shell in `cwd`: pipes, redirects, `&&`, globbing — the lot.
pub(crate) async fn run_script(script: &str, cwd: &Path, timeout: Duration, max_bytes: usize, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let argv = vec![shell_bin().to_string(), "-c".into(), script.to_string()];
    let r = sandbox::run_argv(&argv, cwd, timeout, max_bytes, &ctx.cancel).await?;
    Ok(format_exec(r))
}

pub(crate) fn format_exec(r: sandbox::ExecResult) -> ToolOutput {
    let mut out = r.stdout;
    if !r.stderr.trim().is_empty() {
        out.push_str(&format!("\n[stderr] {}", r.stderr.trim()));
    }
    if r.truncated {
        out.push_str("\n…[output truncated]");
    }
    let code = r.code.unwrap_or(-1);
    if code != 0 {
        out.push_str(&format!("\n[exit code {code}]"));
    }
    ToolOutput { content: out.trim().to_string(), ok: code == 0, exit_code: Some(code), evidence: vec![format!("exit code {code}")] }
}

// ------------------------------------------------------ background jobs ----

struct Job {
    command: String,
    out: Mutex<String>,
    read_to: Mutex<usize>,
    exit: Mutex<Option<i32>>,
    kill: tokio::sync::Notify,
}

static JOBS: LazyLock<Mutex<HashMap<u32, Arc<Job>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_JOB: AtomicU32 = AtomicU32::new(1);
const JOB_BUFFER: usize = 200_000;

fn append(job: &Job, chunk: &[u8]) {
    let mut o = job.out.lock();
    o.push_str(&String::from_utf8_lossy(chunk));
    if o.len() > JOB_BUFFER {
        // keep the tail; remember that the reader's offset moved with it
        let mut cut = o.len() - JOB_BUFFER / 2;
        while !o.is_char_boundary(cut) {
            cut += 1;
        }
        o.drain(..cut);
        let mut r = job.read_to.lock();
        *r = r.saturating_sub(cut);
    }
}

pub(crate) fn spawn_job(script: &str, cwd: &Path) -> Result<u32, ToolError> {
    let mut cmd = tokio::process::Command::new(shell_bin());
    cmd.arg("-c")
        .arg(script)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", "/tmp")
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| ToolError::io(format!("spawn failed: {e}")))?;
    let id = NEXT_JOB.fetch_add(1, Ordering::SeqCst);
    let job = Arc::new(Job {
        command: script.chars().take(80).collect(),
        out: Mutex::new(String::new()),
        read_to: Mutex::new(0),
        exit: Mutex::new(None),
        kill: tokio::sync::Notify::new(),
    });
    JOBS.lock().insert(id, job.clone());
    let (mut so, mut se) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    let (j1, j2) = (job.clone(), job.clone());
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = so.read(&mut buf).await {
            if n == 0 {
                break;
            }
            append(&j1, &buf[..n]);
        }
    });
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = se.read(&mut buf).await {
            if n == 0 {
                break;
            }
            append(&j2, &buf[..n]);
        }
    });
    tokio::spawn(async move {
        let code = tokio::select! {
            s = child.wait() => s.ok().and_then(|s| s.code()).unwrap_or(-1),
            _ = job.kill.notified() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                -9
            }
        };
        *job.exit.lock() = Some(code);
    });
    Ok(id)
}

pub struct ShellOutput;
pub struct ShellKill;

fn job(id: &Value) -> Result<(u32, Arc<Job>), ToolError> {
    let n =
        id.get("job_id").and_then(|v| v.as_u64()).ok_or_else(|| ToolError::args("missing 'job_id' (the number `shell` returned for a background command)"))?
            as u32;
    JOBS.lock().get(&n).cloned().map(|j| (n, j)).ok_or_else(|| ToolError::new(ErrorKind::NotFound, format!("no background job {n}")))
}

#[async_trait]
impl Tool for ShellOutput {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta(
            "shell_output",
            "show the new output of a background shell job (started with shell background=true) and whether it is still running; all=true re-reads everything",
            "system",
            vec![ParamSpec::new("job_id", ParamType::Integer, true, "job number"), ParamSpec::new("all", ParamType::Boolean, false, "from the beginning")],
        );
        kw(&mut m, &["output", "logs", "background", "job", "server", "status"]);
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        let (id, j) = job(&input)?;
        let all = input.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
        let out = j.out.lock().clone();
        let mut from = j.read_to.lock();
        let start = if all { 0 } else { (*from).min(out.len()) };
        let mut fresh = out[start..].to_string();
        *from = out.len();
        if fresh.len() > 20_000 {
            let mut cut = fresh.len() - 20_000;
            while !fresh.is_char_boundary(cut) {
                cut += 1;
            }
            fresh = format!("…[earlier output skipped]\n{}", &fresh[cut..]);
        }
        let status = match *j.exit.lock() {
            None => "running".to_string(),
            Some(c) => format!("exited with code {c}"),
        };
        Ok(ToolOutput::ok(format!("job {id} `{}`: {status}\n{}", j.command, fresh.trim_end())))
    }
}

#[async_trait]
impl Tool for ShellKill {
    fn metadata(&self) -> ToolMetadata {
        let mut m = meta("shell_kill", "stop a background shell job", "system", vec![ParamSpec::new("job_id", ParamType::Integer, true, "job number")]);
        m.risk = Risk::Moderate;
        m.side_effects = true;
        kw(&mut m, &["kill", "stop", "terminate", "background", "job"]);
        m
    }
    async fn execute(&self, input: Value, _c: ToolContext) -> Result<ToolOutput, ToolError> {
        let (id, j) = job(&input)?;
        if j.exit.lock().is_some() {
            return Ok(ToolOutput::ok(format!("job {id} had already finished")));
        }
        j.kill.notify_one();
        for _ in 0..20 {
            if j.exit.lock().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(ToolOutput::ok(format!("stopped job {id}")))
    }
}

/// Whether `shell` runs a full bash (the sandbox mode).
pub(crate) fn sandboxed(policy: &PolicyEngine) -> bool {
    policy.config().shell.mode == ShellMode::Sandbox
}

pub(crate) fn resolve_cwd(policy: &PolicyEngine, input: &Value) -> Result<PathBuf, ToolError> {
    match input.get("cwd").and_then(|v| v.as_str()).filter(|c| !c.is_empty()) {
        Some(c) => {
            // A model told "the workspace" writes `workspace` or `/workspace` for it. When no such directory exists inside the
            // workspace, that names the workspace itself (and `workspace/site` means `site`).
            let name = policy.workspace().file_name().and_then(|n| n.to_str()).unwrap_or("workspace").to_string();
            let trimmed = c.trim().trim_start_matches("./").trim_matches('/');
            let alias = [name.as_str(), "workspace"]
                .into_iter()
                .find_map(|w| (trimmed == w).then(String::new).or_else(|| trimmed.strip_prefix(&format!("{w}/")).map(str::to_string)));
            let c: &str = match &alias {
                Some(rest) if !policy.workspace().join(trimmed.split('/').next().unwrap_or("")).is_dir() => {
                    if rest.is_empty() {
                        return Ok(policy.workspace().to_path_buf());
                    }
                    rest
                }
                _ => c,
            };
            let p = policy.guard().read_path(c)?;
            // otherwise the OS answers "No such file or directory" about the command, which sends everyone looking for the wrong thing
            if !p.is_dir() {
                return Err(ToolError::new(
                    ErrorKind::NotFound,
                    format!(
                        "the working directory '{c}' does not exist: create it first (fs_mkdir or the step that writes into it), and run this command after that step"
                    ),
                ));
            }
            Ok(p)
        }
        None => Ok(policy.workspace().to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_working_directory_called_workspace_means_the_workspace() {
        let ws = std::env::temp_dir().join(format!("dfae-cwd-{}", std::process::id()));
        std::fs::create_dir_all(ws.join("site")).unwrap();
        let policy = PolicyEngine::new(Default::default(), ws.clone());
        let cwd = |c: &str| resolve_cwd(&policy, &serde_json::json!({"cwd": c}));
        let root = policy.workspace().to_path_buf();
        assert_eq!(cwd("workspace").unwrap(), root);
        assert_eq!(cwd("./workspace/").unwrap(), root);
        assert_eq!(cwd("workspace/site").unwrap(), root.join("site"));
        assert_eq!(cwd("site").unwrap(), root.join("site"));
        assert!(cwd("missing").unwrap_err().message.contains("does not exist"));
        // a real directory of that name wins
        std::fs::create_dir_all(ws.join("workspace")).unwrap();
        assert_eq!(cwd("workspace").unwrap(), root.join("workspace"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn globs_match_like_a_coding_agent_expects() {
        let m = |g: &str, p: &str| glob_to_regex(g).unwrap().is_match(p);
        assert!(m("**/*.rs", "src/main.rs") && m("**/*.rs", "main.rs") && !m("**/*.rs", "src/main.py"));
        assert!(m("*.rs", "a/b/c.rs"), "no slash: any depth");
        assert!(m("src/**/test_*.py", "src/a/b/test_x.py") && m("src/**/test_*.py", "src/test_x.py") && !m("src/**/test_*.py", "lib/test_x.py"));
        assert!(m("*.{ts,tsx}", "web/app.tsx") && !m("*.{ts,tsx}", "web/app.js"));
        assert!(m("docs/?.md", "docs/a.md") && !m("docs/?.md", "docs/ab.md"));
        assert!(m("src/*", "src/a") && !m("src/*", "src/a/b"), "a single star stays inside one directory");
        assert!(m("data/[0-9].csv", "data/5.csv"));
        assert!(m("a.b", "a.b") && !m("a.b", "aXb"), "dots are literal");
    }
}
