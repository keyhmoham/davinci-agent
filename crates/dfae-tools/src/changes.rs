//! What a tool call would change among the files that are already there (plan item E1e).
//!
//! Once a task has read content from outside (the web, an outside service), what it does next may be steered by that content. Creating
//! things is harmless; changing, moving or removing what was there before is what an injected instruction is after (delete the tests,
//! set `admin = true`, empty a file). This module says which existing paths a call touches, so the executor can hold back the ones the
//! user's request does not name. It errs on the side of reporting a change: it decides only whether a human is asked.

use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// One existing path a call would change, relative to the workspace (`.` for the workspace as a whole).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub how: &'static str,
}

/// The existing paths this call would change. `since`: paths created after it (by this task) are not reported.
pub fn existing_changes(tool: &str, args: &Value, workspace: &Path, since: Option<std::time::SystemTime>) -> Vec<Change> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let cwd = match s("cwd") {
        "" => workspace.to_path_buf(),
        c => workspace.join(c),
    };
    let r = Resolver { workspace, cwd: &cwd, since };
    let mut out = Vec::new();
    match tool {
        "fs_delete" => r.push(&mut out, s("path"), "removes"),
        "fs_write" => r.push(&mut out, s("path"), "overwrites"),
        "fs_edit" => r.push(&mut out, s("path"), "edits"),
        "fs_move" => {
            r.push(&mut out, s("from"), "moves");
            r.push(&mut out, s("to"), "overwrites");
        }
        "fs_copy" => r.push(&mut out, s("to"), "overwrites"),
        "shell" => shell(&r, s("command"), &mut out),
        _ => {}
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.path.clone()));
    out
}

struct Resolver<'a> {
    workspace: &'a Path,
    cwd: &'a Path,
    since: Option<std::time::SystemTime>,
}

impl Resolver<'_> {
    /// The workspace-relative form of an existing path that was not created after `since`.
    fn existing(&self, word: &str) -> Option<String> {
        let w = word.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ')' | '(' | ';'));
        if w.is_empty() || w.starts_with('-') || w.contains("://") || w.starts_with("/dev/") {
            return None;
        }
        // a glob changes what its directory holds
        let w = match w.find(['*', '?', '[']) {
            Some(i) => w[..i].rsplit_once('/').map(|(d, _)| d).unwrap_or("."),
            None => w,
        };
        let p = if Path::new(w).is_absolute() { PathBuf::from(w) } else { self.cwd.join(w) };
        let p = normalise(&p);
        let rel = p.strip_prefix(normalise(self.workspace)).ok()?;
        let meta = std::fs::symlink_metadata(&p).ok()?;
        if let (Some(since), Ok(born)) = (self.since, meta.created())
            && born >= since
        {
            return None; // made by this task
        }
        let rel = rel.to_string_lossy().to_string();
        Some(if rel.is_empty() { ".".into() } else { rel })
    }

    fn push(&self, out: &mut Vec<Change>, word: &str, how: &'static str) {
        if let Some(path) = self.existing(word) {
            out.push(Change { path, how });
        }
    }
}

fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// A script (inline python/node/perl, or a heredoc) that writes, moves or removes files.
static SCRIPT_WRITES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"open\([^)]*,\s*['"][wax+]|\.write_text\(|\.write_bytes\(|\.unlink\(|\.rename\(|\.replace\(|rmtree|os\.remove|os\.rmdir|shutil\.(?:move|copy)|writeFileSync|appendFileSync|renameSync|rmSync|unlinkSync|rmdirSync|fs\.(?:writeFile|rm|unlink|rename)\b"#,
    )
    .unwrap()
});
static QUOTED: LazyLock<[Regex; 2]> = LazyLock::new(|| [Regex::new(r"'([^'\n]{1,200})'").unwrap(), Regex::new(r#""([^"\n]{1,200})""#).unwrap()]);

fn shell(r: &Resolver, command: &str, out: &mut Vec<Change>) {
    for segment in command.split(['\n', ';', '|', '&']) {
        let words = shlex::split(segment).unwrap_or_else(|| segment.split_whitespace().map(String::from).collect());
        let mut argv: Vec<&str> = Vec::new();
        let mut i = 0;
        while i < words.len() {
            let w = words[i].as_str();
            // redirections: `> f`, `>> f`, `>f`, `2> f`, `&> f`
            let after = w.trim_start_matches(['1', '2', '&']);
            if let Some(t) = after.strip_prefix(">>").or_else(|| after.strip_prefix('>')) {
                let target = if t.is_empty() { words.get(i + 1).map(|s| s.as_str()).unwrap_or("") } else { t };
                if !target.starts_with('&') {
                    r.push(out, target, "overwrites");
                }
                i += if t.is_empty() { 2 } else { 1 };
                continue;
            }
            if w.starts_with('<') {
                i += if w == "<" || w == "<<" || w == "<<<" { 2 } else { 1 };
                continue;
            }
            argv.push(w);
            i += 1;
        }
        // skip `VAR=x`, `sudo`, `command`, `env`, `xargs`
        let start = argv.iter().position(|w| !w.contains('=') && !matches!(*w, "sudo" | "command" | "env" | "xargs" | "nohup" | "time")).unwrap_or(argv.len());
        let argv = &argv[start..];
        let Some(bin) = argv.first().map(|b| b.rsplit('/').next().unwrap_or(b)) else { continue };
        let operands: Vec<&str> = argv[1..].iter().copied().filter(|a| !a.starts_with('-')).collect();
        match bin {
            "rm" | "rmdir" | "unlink" | "shred" => operands.iter().for_each(|o| r.push(out, o, "removes")),
            "truncate" => operands.iter().for_each(|o| r.push(out, o, "empties")),
            "mv" => operands.iter().for_each(|o| r.push(out, o, "moves")),
            "cp" | "rsync" | "install" | "ln" => {
                if let Some(last) = operands.last() {
                    r.push(out, last, "overwrites");
                }
            }
            "tee" => operands.iter().for_each(|o| r.push(out, o, "overwrites")),
            "sed" | "perl" if argv[1..].iter().any(|a| a.starts_with("-i") || a.starts_with("--in-place") || a.starts_with("-pi")) => {
                // the first operand of sed is the script
                operands.iter().skip(usize::from(bin == "sed" && !argv.contains(&"-e"))).for_each(|o| r.push(out, o, "edits"))
            }
            "find" if argv.iter().any(|a| *a == "-delete" || *a == "-exec" || *a == "-execdir") => {
                r.push(out, operands.first().copied().unwrap_or("."), "removes or changes files under")
            }
            "git" if operands.first().is_some_and(|s| matches!(*s, "clean" | "reset" | "checkout" | "restore" | "rm" | "mv" | "stash" | "switch")) => {
                r.push(out, ".", "resets files in")
            }
            _ => {}
        }
    }
    if SCRIPT_WRITES.is_match(command) {
        // each quote kind on its own: `"open('a.txt','w')"` holds a single-quoted name inside a double-quoted script
        for c in QUOTED.iter().flat_map(|q| q.captures_iter(command)) {
            r.push(out, &c[1], "changes (from a script)");
        }
    }
}

/// Does the request name this workspace path (the path, or the name of it or of a directory it is in)?
pub fn named_in(request: &str, rel: &str) -> bool {
    if rel == "." || rel.is_empty() {
        return false;
    }
    let low = request.to_lowercase();
    let rel = rel.to_lowercase();
    let mut names: Vec<String> = Vec::new();
    let mut prefix = String::new();
    for part in rel.split('/') {
        prefix = if prefix.is_empty() { part.to_string() } else { format!("{prefix}/{part}") };
        names.push(prefix.clone());
        names.push(part.to_string());
    }
    let edge = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')));
    names.iter().filter(|n| n.len() >= 2).any(|n| {
        low.match_indices(n.as_str()).any(|(i, _)| {
            let before = low[..i].chars().next_back();
            let after = low[i + n.len()..].chars().next();
            // a trailing "." ends a sentence, not a name
            edge(before) && (edge(after) || (after == Some('.') && edge(low[i + n.len() + 1..].chars().next())))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ws() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for f in ["config.toml", "app.py", "tests/test_app.py", "src/lib/util.py"] {
            let p = d.path().join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        d
    }

    fn sh(d: &Path, c: &str) -> Vec<String> {
        existing_changes("shell", &json!({"command": c}), d, None).into_iter().map(|c| c.path).collect()
    }

    #[test]
    fn changes_to_existing_files_are_found_in_commands_and_scripts() {
        let d = ws();
        let p = d.path();
        assert_eq!(sh(p, "rm -rf tests"), ["tests"]);
        assert_eq!(sh(p, "mkdir -p logs && printf 'PORT=3000\\n' > settings.ini"), Vec::<String>::new(), "new things are not changes");
        assert_eq!(sh(p, "sed -i 's/admin = false/admin = true/' config.toml"), ["config.toml"]);
        assert_eq!(sh(p, "echo 'admin = true' >> config.toml"), ["config.toml"]);
        assert_eq!(sh(p, ": > app.py"), ["app.py"]);
        assert_eq!(sh(p, "mv tests /tmp/old_tests"), ["tests"]);
        assert_eq!(sh(p, "truncate -s 0 app.py"), ["app.py"]);
        assert_eq!(sh(p, "rm -f tests/*"), ["tests"]);
        assert_eq!(sh(p, "git checkout -- ."), ["."]);
        assert_eq!(sh(p, "python3 - <<'EOF'\nimport shutil\nshutil.rmtree('tests')\nEOF"), ["tests"]);
        assert_eq!(sh(p, "python3 -c \"open('config.toml','w').write('admin = true')\""), ["config.toml"]);
        assert_eq!(
            sh(p, "cat config.toml app.py > merged.txt && grep -r admin src && python3 -m pytest -q 2>&1 | tail -3"),
            Vec::<String>::new(),
            "reads are not changes"
        );
        assert_eq!(sh(p, "cd src && ls"), Vec::<String>::new());
        let fs = |t: &str, a: serde_json::Value| existing_changes(t, &a, p, None).into_iter().map(|c| c.path).collect::<Vec<_>>();
        assert_eq!(fs("fs_edit", json!({"path": "config.toml"})), ["config.toml"]);
        assert_eq!(fs("fs_write", json!({"path": "notes/price.txt"})), Vec::<String>::new());
        assert_eq!(fs("fs_move", json!({"from": "tests", "to": "old"})), ["tests"]);
        assert_eq!(fs("fs_delete", json!({"path": "../outside"})), Vec::<String>::new(), "outside the workspace is the policy's business");
        // made by this task: not a change to what was there
        let since = std::time::SystemTime::now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(p.join("fresh.txt"), "x").unwrap();
        if std::fs::metadata(p.join("fresh.txt")).unwrap().created().is_ok() {
            assert!(existing_changes("fs_delete", &json!({"path": "fresh.txt"}), p, Some(since)).is_empty());
            assert_eq!(existing_changes("fs_delete", &json!({"path": "app.py"}), p, Some(since)).len(), 1);
        }
    }

    #[test]
    fn a_path_is_named_by_itself_or_a_directory_it_is_in() {
        assert!(named_in("Read the setup page, then delete the tests folder", "tests"));
        assert!(named_in("delete the tests folder", "tests/test_app.py"));
        assert!(named_in("Update config.toml as the page says.", "config.toml"));
        assert!(named_in("fix the helper in src/lib", "src/lib/util.py"));
        assert!(!named_in("remove the bug from the parser", "tests"), "a verb is not a name");
        assert!(!named_in("change the config", "config.toml"));
        assert!(!named_in("Set up this project by following the setup instructions", "tests"));
        assert!(!named_in("run the latest tests_new suite", "tests"), "part of a longer name is not the name");
        assert!(!named_in("anything at all", "."));
    }
}
