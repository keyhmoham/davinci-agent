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
            "change part of an existing file: replace old_string (copied exactly from fs_read, including indentation) with new_string. Fails if old_string is missing or occurs more than once (add surrounding lines, or set replace_all=true). Prefer it to rewriting the file with fs_write. With a DIRECTORY as path, every occurrence in every file under it is replaced (a rename across a project: path=src, old_string=calcTotal)",
            "filesystem",
            vec![
                ParamSpec::new("path", ParamType::String, true, "file to edit, or a directory: every file under it").hints(&["file path"]),
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
        if p.is_dir() {
            return self.edit_tree(raw, &p, old, new);
        }
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
        if p.is_dir() {
            // a tree edit: no file under it may still hold the old text
            let old = input.get("old_string")?.as_str()?;
            let mut left = Vec::new();
            walk_files(&self.policy.guard(), &p, 12, 20_000, |f, _| {
                if std::fs::read_to_string(f).is_ok_and(|t| t.contains(old)) {
                    left.push(f.to_string_lossy().into_owned());
                }
                true
            });
            return Some(if left.is_empty() {
                Ok(format!("no file under {raw} still contains the old text"))
            } else {
                Err(format!("still contains the old text: {}", left.join(", ")))
            });
        }
        Some(match tokio::fs::read_to_string(&p).await {
            Ok(t) if new.is_empty() || t.contains(new) => Ok(format!("{raw} contains the new text")),
            Ok(_) => Err(format!("{raw} does not contain the new text after the edit")),
            Err(e) => Err(format!("read-back failed: {e}")),
        })
    }
}

impl FsEdit {
    /// Replace every occurrence of `old` with `new` in each text file under `dir` that contains it (vendored and build directories
    /// are skipped; every file is checked by the policy as a write).
    fn edit_tree(&self, raw: &str, dir: &Path, old: &str, new: &str) -> Result<ToolOutput, ToolError> {
        let guard = self.policy.guard();
        let mut hits: Vec<(std::path::PathBuf, String, usize)> = Vec::new();
        walk_files(&guard, dir, 12, 20_000, |f, md| {
            if md.len() <= 2_000_000
                && let Ok(t) = std::fs::read_to_string(f)
            {
                let n = t.matches(old).count();
                if n > 0 {
                    hits.push((f.to_path_buf(), t, n));
                }
            }
            true
        });
        if hits.is_empty() {
            return Err(ToolError::args(format!("old_string was not found in any file under {raw}")));
        }
        if hits.len() > 200 {
            return Err(ToolError::args(format!("old_string occurs in {} files under {raw}: narrow the directory", hits.len())));
        }
        let mut done = Vec::new();
        for (f, text, n) in &hits {
            let w = guard.write_path(&f.to_string_lossy())?;
            std::fs::write(&w, text.replace(old, new)).map_err(|e| ToolError::io(format!("{}: {e}", f.display())))?;
            done.push(format!("{} ({n})", rel(&guard.workspace, f)));
        }
        let total: usize = hits.iter().map(|h| h.2).sum();
        Ok(ToolOutput::ok(format!("edited {} file(s) under {raw}, {total} replacement(s): {}", hits.len(), done.join(", "))))
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

/// Output that keeps its start and its end: a long install or build log says what it was doing at the top and what went wrong at
/// the bottom, and what is between is the part nobody needs.
struct Capture {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    head_max: usize,
    tail_max: usize,
    dropped: usize,
}

impl Capture {
    fn new(max: usize) -> Self {
        let max = max.max(1024);
        Self { head: Vec::new(), tail: Default::default(), head_max: max / 3, tail_max: max - max / 3, dropped: 0 }
    }

    fn push(&mut self, chunk: &[u8]) {
        let room = self.head_max.saturating_sub(self.head.len());
        let (h, rest) = chunk.split_at(room.min(chunk.len()));
        self.head.extend_from_slice(h);
        self.tail.extend(rest);
        while self.tail.len() > self.tail_max {
            self.tail.pop_front();
            self.dropped += 1;
        }
    }

    fn text(&self) -> String {
        let mut bytes = self.head.clone();
        if self.dropped > 0 {
            bytes.extend_from_slice(format!("\n… [{} bytes omitted] …\n", self.dropped).as_bytes());
        }
        bytes.extend(self.tail.iter());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// What a terminal would have shown: no colour codes, and a progress bar redrawn with `\r` is its last frame.
pub(crate) fn clean_output(s: &str) -> String {
    static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").unwrap());
    let s = ANSI.replace_all(s, "");
    s.lines().map(|l| l.rsplit('\r').find(|seg| !seg.is_empty()).unwrap_or("")).collect::<Vec<_>>().join("\n")
}

fn kill_group(pid: Option<u32>) {
    if let Some(p) = pid {
        // the command was started as the leader of its own process group: everything it started goes with it
        unsafe {
            libc::killpg(p as i32, libc::SIGKILL);
        }
    }
}

/// Kills a command's whole process group when it is dropped while still armed. `kill_on_drop` ends only the shell; an `npm install` it
/// started would live on and keep writing into `node_modules` after the executor gave up on the call (coding plan X2). Disarmed once
/// the command has exited normally.
struct GroupGuard(Option<u32>);

impl GroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        kill_group(self.0.take());
    }
}

/// A command that installs or builds takes minutes, not seconds.
pub(crate) fn default_timeout(policy: &PolicyEngine, command: &str) -> Duration {
    static SLOW: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?x)\b(?:pip3?|pipx|uv|poetry|conda|npm|npx|yarn|pnpm|bun|cargo|rustup|go\s+(?:install|build|get|mod)|apt(?:-get)?|dfae-apt|brew|gem|bundle|composer|mvn|gradle|make|cmake|ninja|git\s+clone|docker|curl|wget)\b").unwrap()
    });
    let sh = &policy.config().shell;
    Duration::from_secs(if SLOW.is_match(command) { sh.install_timeout_secs } else { sh.default_timeout_secs }.clamp(1, sh.max_timeout_secs.max(1)))
}

/// The timeout a shell command runs with: the `timeout_secs` it asks for (capped), or the default for its kind.
pub(crate) fn effective_timeout(policy: &PolicyEngine, input: &Value, command: &str) -> Duration {
    let sh = &policy.config().shell;
    match input.get("timeout_secs").and_then(|v| v.as_u64()) {
        Some(s) => Duration::from_secs(s.clamp(1, sh.max_timeout_secs.max(1))),
        None => default_timeout(policy, command),
    }
}

/// Run `script` with the shell in `cwd`: pipes, redirects, `&&`, globbing — the lot. stderr is merged into stdout (in the order the
/// command wrote them), the command leads its own process group (a timeout ends everything it started), and what it printed before
/// a timeout is not lost.
pub(crate) async fn run_script(
    policy: &PolicyEngine,
    script: &str,
    cwd: &Path,
    timeout: Duration,
    max_bytes: usize,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let mut cmd = tokio::process::Command::new(shell_bin());
    cmd.arg("-c")
        .arg(format!("exec 2>&1\n{script}"))
        .current_dir(cwd)
        .env_clear()
        .envs(crate::toolbox::shell_env(policy))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| ToolError::io(format!("spawn failed: {e}")))?;
    let pid = child.id();
    let mut guard = GroupGuard(pid);
    let mut stdout = child.stdout.take().expect("piped");
    let cap = Arc::new(Mutex::new(Capture::new(max_bytes)));
    let reader = {
        let cap = cap.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            while let Ok(n) = stdout.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                cap.lock().push(&buf[..n]);
            }
        })
    };
    let waited = tokio::select! {
        _ = ctx.cancel.cancelled() => None,
        r = tokio::time::timeout(timeout, child.wait()) => Some(r),
    };
    let (code, timed_out) = match waited {
        None => {
            kill_group(pid);
            let _ = child.wait().await;
            let note = if installs_node_modules(script) { format!(" {INTERRUPTED_INSTALL}") } else { String::new() };
            return Err(ToolError::new(ErrorKind::Cancelled, format!("cancelled{note}")));
        }
        Some(Ok(Ok(status))) => {
            use std::os::unix::process::ExitStatusExt;
            guard.disarm();
            (status.code().or(status.signal().map(|s| 128 + s)), false)
        }
        Some(Ok(Err(e))) => return Err(ToolError::io(e.to_string())),
        Some(Err(_)) => {
            kill_group(pid);
            let _ = child.wait().await;
            (None, true)
        }
    };
    // what is left in the pipe; a process the command left running keeps it open, which is not worth waiting for
    let _ = tokio::time::timeout(Duration::from_millis(400), reader).await;
    let raw = cap.lock().text();
    let mut text = clean_output(&raw).trim().to_string();
    let code = code.unwrap_or(-1);
    if timed_out {
        text.push_str(&format!(
            "\n[timed out after {}s and was killed: pass a larger timeout_secs (up to {}) if it needs longer; use background=true for something that keeps running]",
            timeout.as_secs(),
            policy.config().shell.max_timeout_secs
        ));
        if installs_node_modules(script) {
            text.push_str(&format!("\n{INTERRUPTED_INSTALL}"));
        }
    } else if code != 0 {
        text.push_str(&format!("\n[exit code {code}]"));
    }
    if (code != 0 || timed_out)
        && let Some(h) = hint(script, &text)
    {
        text.push_str(&format!("\n[hint] {h}"));
    }
    Ok(ToolOutput { content: text.trim().to_string(), ok: code == 0 && !timed_out, exit_code: Some(code), evidence: vec![format!("exit code {code}")] })
}

/// Said after an install that was stopped half-way: building on what it left is how a truncated native module (`ld.so` crashes in
/// `next build`) came about (coding plan X3).
const INTERRUPTED_INSTALL: &str = "[interrupted install: node_modules may be incomplete — rm -rf node_modules && npm install]";

/// Does this command install Node packages into a project (`npm install`, `npm ci`, `yarn add`, a `create-*` scaffold)?
pub(crate) fn installs_node_modules(script: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?:(?:npm|pnpm|bun)\s+(?:install|i|ci|add)\b|yarn(?:\s+(?:install|add)\b|\s*(?:$|&&|;|\|))|npx\s+(?:-y\s+|--yes\s+)?create-[\w-]+|npm\s+(?:init|create)\s+[\w@-]+)").unwrap()
    });
    RE.is_match(script)
}

/// A native module that cannot be loaded: almost always one an interrupted install left half-written.
fn broken_native_module(output: &str) -> bool {
    output.contains("Inconsistency detected by ld.so")
        || output.contains("invalid ELF header")
        || output.contains("ERR_DLOPEN_FAILED")
        || output.contains("wrong ELF class")
        || (output.contains("Bus error") && output.contains(".node"))
}

/// What a failed command usually means here, said once and plainly: the agent may install what is missing.
fn hint(script: &str, output: &str) -> Option<String> {
    if broken_native_module(output) {
        return Some(
            "a native module in node_modules is broken (usually an interrupted install), not the code: `rm -rf node_modules package-lock.json && npm install`, then run the command again."
                .into(),
        );
    }
    static NOT_FOUND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)(?:^|: )([\w.+-]+): (?:command )?not found").unwrap());
    static PY_MODULE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"No module named '?([\w.-]+)").unwrap());
    static NODE_MODULE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Cannot find module '([^']+)'").unwrap());
    if let Some(c) = NOT_FOUND.captures(output) {
        let tool = &c[1];
        return Some(format!(
            "`{tool}` is not installed. You are allowed to install it: `pip install <package>`, `npm install -g <package>`, `cargo install <crate>`, `go install <module>@latest`, \
             `dfae-apt <debian-package>` for a system package, or download a release binary with curl into $DFAE_TOOLS/bin (chmod +x). Install it, then run the command again."
        ));
    }
    if let Some(c) = PY_MODULE.captures(output) {
        let m = c[1].split('.').next().unwrap_or(&c[1]);
        return Some(format!(
            "the Python module `{m}` is missing: `pip install {m}` (the package name may differ from the module name), then run the command again."
        ));
    }
    if let Some(c) = NODE_MODULE.captures(output)
        && !c[1].starts_with('.')
        && !c[1].starts_with('/')
    {
        return Some(format!("the Node module `{}` is missing: `npm install {}` in the project directory, then run the command again.", &c[1], &c[1]));
    }
    if output.contains("no such option: -y") || output.contains("no such option: --yes") {
        return Some("pip does not take -y (it never asks): run the command again without it.".into());
    }
    if (output.contains("Permission denied") || output.contains("Read-only file system") || output.contains("EACCES"))
        && (output.contains("/usr") || output.contains("/etc") || output.contains("/var") || script.contains("sudo") || script.contains("apt"))
    {
        return Some("system directories are read-only and there is no root: install into the toolbox instead (`pip install` and `npm install -g` already do; `dfae-apt <package>` for Debian packages) and never use sudo.".into());
    }
    None
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
    /// Where it runs and the port it said it listens on: a new server for the same project on that port replaces it.
    cwd: PathBuf,
    port: Option<u16>,
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

pub(crate) fn spawn_job(policy: &PolicyEngine, script: &str, cwd: &Path) -> Result<u32, ToolError> {
    let mut cmd = tokio::process::Command::new(shell_bin());
    cmd.arg("-c")
        .arg(script)
        .current_dir(cwd)
        .env_clear()
        .envs(crate::toolbox::shell_env(policy))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| ToolError::io(format!("spawn failed: {e}")))?;
    let pid = child.id();
    let id = NEXT_JOB.fetch_add(1, Ordering::SeqCst);
    let job = Arc::new(Job {
        command: script.chars().take(80).collect(),
        cwd: cwd.to_path_buf(),
        port: port_in(script),
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
        // the engine shutting down drops this task: the job's whole group goes with it, not only its shell
        let mut guard = GroupGuard(pid);
        let code = tokio::select! {
            s = child.wait() => {
                guard.disarm();
                s.ok().and_then(|s| s.code()).unwrap_or(-1)
            }
            _ = job.kill.notified() => {
                // the job and whatever it started (a server's workers, a watcher's children)
                kill_group(pid);
                let _ = child.wait().await;
                -9
            }
        };
        *job.exit.lock() = Some(code);
    });
    Ok(id)
}

/// A server that dies at once (port in use, missing directory, syntax error) must not be reported as started: wait briefly,
/// and if the job has exited, say why. Otherwise it is up (what it printed so far comes with the answer).
pub(crate) async fn settle_job(id: u32, grace: Duration) -> Result<String, ToolError> {
    let job = JOBS.lock().get(&id).cloned().ok_or_else(|| ToolError::new(ErrorKind::NotFound, format!("no background job {id}")))?;
    let deadline = tokio::time::Instant::now() + grace;
    while job.exit.lock().is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // let the readers drain what the dying process wrote
    tokio::time::sleep(Duration::from_millis(50)).await;
    let out = job.out.lock().trim().to_string();
    let tail: String = out.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect();
    let code = *job.exit.lock();
    match code {
        Some(c) => Err(ToolError::new(ErrorKind::Other, format!("the background command exited at once [exit code {c}]: {tail}"))),
        None if tail.is_empty() => Ok(String::new()),
        None => Ok(format!("\nfirst output: {tail}")),
    }
}

/// The port a command says it listens on (`--port 8001`, `-p 8001`, `http.server 8001`, `PORT=8001`), if it says one.
pub(crate) fn port_in(command: &str) -> Option<u16> {
    static PORT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:--port|--listen|-p|-P|-l|PORT)[ =]+(?:[\w.]*:)?(\d{4,5})\b|http\.server\s+(\d{4,5})\b|--bind\s+[\w.]*:(\d{4,5})\b|\s(?:0\.0\.0\.0|127\.0\.0\.1|localhost):(\d{4,5})\b").unwrap()
    });
    let c = PORT.captures(command)?;
    (1..=4).find_map(|i| c.get(i)).and_then(|m| m.as_str().parse::<u16>().ok()).filter(|p| *p >= 1024)
}

async fn listening(port: u16) -> bool {
    tokio::time::timeout(Duration::from_millis(250), tokio::net::TcpStream::connect(("127.0.0.1", port))).await.is_ok_and(|r| r.is_ok())
}

/// Err when something already answers on `port`, naming the ports of 8000–8010 that are free.
pub(crate) async fn refuse_taken_port(port: u16) -> Result<(), ToolError> {
    if !listening(port).await {
        return Ok(());
    }
    let mut free = Vec::new();
    for p in 8000u16..=8010 {
        if !listening(p).await {
            free.push(p.to_string());
        }
    }
    Err(ToolError::new(
        ErrorKind::Other,
        format!(
            "port {port} is already in use by another process: start it on a free port instead (free in 8000-8010: {})",
            if free.is_empty() { "none".into() } else { free.join(", ") }
        ),
    ))
}

/// What to do about the port a new server names (coding plan X9). Free: start it as it is. Held by one of our own background jobs for the
/// same directory: that is the previous version of this server, which is stopped (a redeploy). Held by anything else: the first free
/// port of 8000–8010 is used instead, `Ok(Some(new))`, so the command is rewritten rather than the step failed.
pub(crate) async fn choose_port(port: u16, cwd: &Path) -> Result<Option<u16>, ToolError> {
    if !listening(port).await {
        return Ok(None);
    }
    let ours: Vec<Arc<Job>> = JOBS.lock().values().filter(|j| j.port == Some(port) && j.exit.lock().is_none() && same_dir(&j.cwd, cwd)).cloned().collect();
    if !ours.is_empty() {
        for j in &ours {
            j.kill.notify_one();
        }
        for _ in 0..40 {
            if !listening(port).await {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    for p in 8000u16..=8010 {
        if p != port && !listening(p).await {
            return Ok(Some(p));
        }
    }
    refuse_taken_port(port).await.map(|_| None)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let c = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    c(a) == c(b)
}

/// `command` with port `old` replaced by `new` wherever it stands as a number of its own (`--port 8000`, `-p 8000`, `:8000`, `PORT=8000`).
pub(crate) fn rewrite_port(command: &str, old: u16, new: u16) -> String {
    let re = Regex::new(&format!(r"(^|[^\d]){old}([^\d]|$)")).expect("port pattern");
    // twice: two occurrences that share the separator between them (`8000:8000`) are found in two passes
    let once = re.replace_all(command, format!("${{1}}{new}${{2}}").as_str()).into_owned();
    re.replace_all(&once, format!("${{1}}{new}${{2}}").as_str()).into_owned()
}

/// A server that is started is checked to be *listening*, not merely not dead yet: a port already taken makes some servers print that they
/// started and exit a moment later. The job must stay alive for a few seconds with the port answering; until then, an exit is a failure
/// with what it printed.
pub(crate) async fn await_listening(id: u32, port: u16, patience: Duration) -> Result<String, ToolError> {
    let job = JOBS.lock().get(&id).cloned().ok_or_else(|| ToolError::new(ErrorKind::NotFound, format!("no background job {id}")))?;
    let start = tokio::time::Instant::now();
    let tail = |job: &Job| {
        let out = job.out.lock().trim().to_string();
        out.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect::<String>()
    };
    loop {
        if let Some(c) = *job.exit.lock() {
            return Err(ToolError::new(ErrorKind::Other, format!("the server exited [exit code {c}] before it listened on port {port}: {}", tail(&job))));
        }
        let up = listening(port).await;
        if up && start.elapsed() >= Duration::from_secs(3) {
            return Ok(format!("\nlistening on port {port}"));
        }
        if start.elapsed() >= patience {
            job.kill.notify_one();
            return Err(ToolError::new(
                ErrorKind::Other,
                format!("nothing is listening on port {port} {}s after the start (it was stopped): {}", patience.as_secs(), tail(&job)),
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
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

#[cfg(test)]
mod job_tests {
    use super::*;

    fn test_policy() -> PolicyEngine {
        PolicyEngine::new(crate::policy::PolicyConfig::default(), std::env::temp_dir().join("dfae-job-tests"))
    }

    #[tokio::test]
    async fn a_background_command_that_dies_at_once_is_a_failure_with_its_output() {
        let dir = std::env::temp_dir();
        let id = spawn_job(&test_policy(), "echo 'Address already in use' >&2; exit 3", &dir).unwrap();
        let err = settle_job(id, Duration::from_secs(3)).await.unwrap_err().to_string();
        assert!(err.contains("exit code 3") && err.contains("Address already in use"), "{err}");
    }

    #[test]
    fn the_port_a_command_names() {
        assert_eq!(port_in("uvicorn main:app --host 0.0.0.0 --port 8002"), Some(8002));
        assert_eq!(port_in("npm run dev -- -p 8001 -H 0.0.0.0"), Some(8001));
        assert_eq!(port_in("python3 -m http.server 8003 --bind 0.0.0.0"), Some(8003));
        assert_eq!(port_in("PORT=8004 node server.js"), Some(8004));
        assert_eq!(port_in("flask run --host 0.0.0.0 -p 5000"), Some(5000));
        assert_eq!(port_in("python3 app.py"), None);
        assert_eq!(port_in("pip install x -p 22"), None, "a privileged port is no dev server");
    }

    #[tokio::test]
    async fn a_port_somebody_holds_is_refused_before_anything_starts() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let e = refuse_taken_port(port).await.unwrap_err().message;
        assert!(e.contains("already in use") && e.contains("free in 8000-8010"), "{e}");
        drop(held);
        assert!(refuse_taken_port(port).await.is_ok());
    }

    #[test]
    fn a_port_is_rewritten_where_it_stands_on_its_own() {
        assert_eq!(rewrite_port("npm run start -- -H 0.0.0.0 -p 8000", 8000, 8001), "npm run start -- -H 0.0.0.0 -p 8001");
        assert_eq!(rewrite_port("PORT=8000 node s.js && curl localhost:8000/x", 8000, 8003), "PORT=8003 node s.js && curl localhost:8003/x");
        assert_eq!(rewrite_port("python3 -m http.server 8000 --bind 0.0.0.0 # 18000", 8000, 8002), "python3 -m http.server 8002 --bind 0.0.0.0 # 18000");
    }

    #[tokio::test]
    async fn a_port_held_by_a_stranger_is_replaced_and_one_held_by_our_old_server_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        // a stranger: another port is chosen
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let moved = choose_port(port, dir.path()).await.unwrap();
        assert!(moved.is_some_and(|p| (8000..=8010).contains(&p) && p != port), "{moved:?}");
        drop(held);
        assert_eq!(choose_port(port, dir.path()).await.unwrap(), None, "free: kept");
        // our own server for this directory: stopped, and the port kept
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let id = spawn_job(&test_policy(), &format!("python3 -m http.server {port} --bind 127.0.0.1"), dir.path()).unwrap();
        assert!(await_listening(id, port, Duration::from_secs(15)).await.is_ok());
        assert_eq!(choose_port(port, dir.path()).await.unwrap(), None, "redeploy on the same port");
        assert!(job(&json!({"job_id": id})).unwrap().1.exit.lock().is_some(), "the old server was stopped");
    }

    #[tokio::test]
    async fn a_server_is_up_only_when_its_port_answers_and_it_stays_alive() {
        let policy = test_policy();
        // listens (python is what the tests already rely on) and stays
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let id = spawn_job(&policy, &format!("python3 -m http.server {port} --bind 127.0.0.1"), &std::env::temp_dir()).unwrap();
        assert!(await_listening(id, port, Duration::from_secs(15)).await.unwrap().contains("listening"));
        job(&json!({"job_id": id})).unwrap().1.kill.notify_one();
        // a program that says it started and dies is not a server, even when something else holds the port
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = held.local_addr().unwrap().port();
        let id = spawn_job(&policy, "echo started; sleep 1; echo 'address already in use' >&2; exit 1", &std::env::temp_dir()).unwrap();
        let err = await_listening(id, busy, Duration::from_secs(15)).await.unwrap_err().message;
        assert!(err.contains("exited") && err.contains("already in use"), "{err}");
    }

    #[tokio::test]
    async fn a_running_background_command_is_started() {
        let id = spawn_job(&test_policy(), "echo ready; sleep 5", &std::env::temp_dir()).unwrap();
        let first = settle_job(id, Duration::from_millis(400)).await.unwrap();
        assert!(first.contains("ready"), "{first}");
        job(&json!({"job_id": id})).unwrap().1.kill.notify_one();
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    use crate::policy::PolicyConfig;
    use tokio_util::sync::CancellationToken;

    fn rig() -> (tempfile::TempDir, PolicyEngine, ToolContext) {
        let ws = tempfile::tempdir().unwrap();
        let policy = PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf());
        let ctx = ToolContext { task_id: "t".into(), step_id: "s".into(), workspace: ws.path().to_path_buf(), cancel: CancellationToken::new() };
        (ws, policy, ctx)
    }

    async fn sh(script: &str, secs: u64) -> ToolOutput {
        let (ws, policy, ctx) = rig();
        run_script(&policy, script, ws.path(), Duration::from_secs(secs), 4096, &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn stderr_comes_in_order_with_stdout_and_a_failure_says_its_code() {
        let o = sh("echo one; echo two >&2; echo three; exit 3", 10).await;
        assert_eq!(o.content, "one\ntwo\nthree\n[exit code 3]");
        assert!(!o.ok);
        assert_eq!(sh("echo fine", 10).await.content, "fine");
    }

    #[tokio::test]
    async fn the_toolbox_is_there_and_nothing_waits_for_a_human() {
        let o = sh("echo $DFAE_TOOLS; echo $PYTHONUSERBASE; echo $DEBIAN_FRONTEND; command -v dfae-apt; read -r x; echo [$x]", 10).await;
        assert!(o.content.contains("/.tools") && o.content.contains("/.tools/py") && o.content.contains("noninteractive"), "{}", o.content);
        assert!(o.content.contains("/.tools/bin/dfae-apt") && o.content.ends_with("[]"), "stdin is closed: {}", o.content);
    }

    #[tokio::test]
    async fn a_command_that_times_out_is_killed_with_everything_it_started_and_keeps_what_it_printed() {
        let (ws, policy, ctx) = rig();
        let marker = ws.path().join("child-survived");
        let script = format!("echo started; (sleep 2; touch {}) & sleep 30", marker.display());
        let t0 = std::time::Instant::now();
        let o = run_script(&policy, &script, ws.path(), Duration::from_secs(1), 4096, &ctx).await.unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert!(o.content.starts_with("started") && o.content.contains("timed out after 1s and was killed"), "{}", o.content);
        assert!(!o.ok);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "the background child of the command was killed with it");
    }

    #[tokio::test]
    async fn a_long_log_keeps_its_start_and_its_end() {
        let o = sh("echo START; seq 1 5000; echo THE-ERROR-AT-THE-END; exit 1", 10).await;
        assert!(
            o.content.starts_with("START") && o.content.contains("bytes omitted") && o.content.contains("THE-ERROR-AT-THE-END\n[exit code 1]"),
            "{}",
            &o.content[..200.min(o.content.len())]
        );
        assert!(o.content.len() < 6000);
    }

    #[test]
    fn progress_bars_and_colours_are_what_a_terminal_would_show() {
        assert_eq!(clean_output("\x1b[32mok\x1b[0m\n  10%\r  50%\r 100%\ndone"), "ok\n 100%\ndone");
    }

    #[tokio::test]
    async fn a_missing_tool_comes_with_the_permission_to_install_it() {
        let o = sh("definitely-not-installed --version", 10).await;
        assert!(
            o.content.contains("[exit code 127]") && o.content.contains("[hint] `definitely-not-installed` is not installed. You are allowed to install it"),
            "{}",
            o.content
        );
        let o = sh("python3 -c 'import definitely_missing_module'", 10).await;
        assert!(
            o.content.contains("[hint] the Python module `definitely_missing_module` is missing: `pip install definitely_missing_module`"),
            "{}",
            o.content
        );
    }

    #[test]
    fn a_broken_native_module_is_named_as_such() {
        for out in [
            "Inconsistency detected by ld.so: ../sysdeps/x86_64/dl-machine.h: 545: elf_machine_rela_relative: Assertion failed!",
            "Error: /w/site/node_modules/@next/swc-linux-x64-gnu/next-swc.linux-x64-gnu.node: invalid ELF header",
            "code: 'ERR_DLOPEN_FAILED'",
            "Error: wrong ELF class: ELFCLASS32",
            "Bus error (core dumped) loading next-swc.linux-x64-gnu.node",
        ] {
            let h = hint("npm run build", out).unwrap_or_default();
            assert!(h.contains("native module in node_modules is broken") && h.contains("rm -rf node_modules"), "{out} -> {h}");
        }
        assert!(hint("ls", "Bus error").is_none_or(|h| !h.contains("native module")));
    }

    #[test]
    fn node_installs_are_recognised() {
        for c in ["npm install", "cd site && npm i && npm run build", "npm ci", "yarn", "yarn add next", "pnpm install", "npx -y create-next-app@15 lab"] {
            assert!(installs_node_modules(c), "{c}");
        }
        for c in ["npm run build", "npm start", "pip install flask", "ls node_modules"] {
            assert!(!installs_node_modules(c), "{c}");
        }
    }

    #[tokio::test]
    async fn an_install_that_times_out_says_node_modules_may_be_incomplete() {
        let o = sh("echo npm install; sleep 30 # npm install", 1).await;
        assert!(o.content.contains("interrupted install: node_modules may be incomplete"), "{}", o.content);
    }

    #[tokio::test]
    async fn a_dropped_command_takes_its_whole_process_group_with_it() {
        let (ws, policy, ctx) = rig();
        let tag = format!("{}", 100 + std::process::id() % 800);
        let script = format!("sleep {tag}.317 & sleep {tag}.318");
        let fut = run_script(&policy, &script, ws.path(), Duration::from_secs(60), 4096, &ctx);
        // the executor's backstop gives up on the call: the future is dropped while the command runs
        let _ = tokio::time::timeout(Duration::from_millis(600), fut).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let left: Vec<String> = std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| std::fs::read(e.ok()?.path().join("cmdline")).ok())
            .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
            .filter(|c| c.contains(&format!("{tag}.317")) || c.contains(&format!("{tag}.318")))
            .collect();
        assert!(left.is_empty(), "left running: {left:?}");
    }

    #[test]
    fn installs_and_builds_get_the_longer_timeout() {
        let (_ws, policy, _ctx) = rig();
        assert_eq!(default_timeout(&policy, "ls -la"), Duration::from_secs(120));
        assert_eq!(default_timeout(&policy, "python3 -m pip install requests && python3 x.py"), Duration::from_secs(600));
        assert_eq!(default_timeout(&policy, "cd app && npm ci && npm run build"), Duration::from_secs(600));
        assert_eq!(default_timeout(&policy, "git clone https://example.com/x.git"), Duration::from_secs(600));
    }
}
