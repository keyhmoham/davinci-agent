//! Is what a step just wrote well-formed? A self-hosted model writes the files, and what it gets wrong is mostly
//! mechanical: a JSON object cut off by the token limit, an unclosed tag, a stray quote. Parsers find those for free; the
//! planner (the planner model) then hears one short error line instead of discovering it three steps later.

use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// `Some(problem)` when `text`, the content of `path`, is malformed for its type. Unknown types are never a problem.
pub async fn problem(path: &Path, text: &str) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "json" => serde_json::from_str::<serde_json::Value>(text).err().map(|e| format!("invalid JSON: {e}")),
        "toml" => toml::from_str::<toml::Value>(text).err().map(|e| format!("invalid TOML: {}", first_line(&e.to_string()))),
        "html" | "htm" => html_problem(text),
        "py" => run_checker("python3", &["-c", "import sys; compile(sys.stdin.read(), sys.argv[1], 'exec')", &path.to_string_lossy()], text)
            .await
            .map(|e| format!("Python syntax error: {e}")),
        "sh" | "bash" => run_checker("bash", &["-n"], text).await.map(|e| format!("shell syntax error: {e}")),
        "js" | "mjs" | "cjs" => run_checker("node", &["--check", "--input-type=commonjs", "-"], text).await.map(|e| format!("JavaScript syntax error: {e}")),
        _ => None,
    }
}

fn first_line(s: &str) -> String {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(200).collect()
}

/// Truncated or unbalanced pages: the failure a token limit produces.
fn html_problem(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("<html") && !lower.contains("<body") && !lower.contains("<div") && !lower.contains("<p") {
        return None; // a fragment or not really HTML
    }
    for tag in ["html", "body", "head", "script", "style", "div", "table", "ul", "ol", "form", "section", "main", "nav", "header", "footer"] {
        let open = count_tags(&lower, &format!("<{tag}"));
        let close = lower.matches(&format!("</{tag}>")).count();
        if open > close {
            return Some(format!("the page looks cut off: {open} <{tag}> opened, {close} closed"));
        }
    }
    None
}

/// Opening tags of exactly this name (`<div` but not `<divider`), ignoring self-closing ones.
fn count_tags(lower: &str, open: &str) -> usize {
    lower
        .match_indices(open)
        .filter(|(i, _)| {
            let rest = &lower[i + open.len()..];
            let next = rest.chars().next();
            matches!(next, Some(' ' | '>' | '\n' | '\t' | '/')) && !rest.split('>').next().unwrap_or("").ends_with('/')
        })
        .count()
}

/// Run an interpreter's syntax-check mode on `text` (stdin). `None` when it passes, or when the interpreter is not here:
/// a missing checker is no evidence against the file.
async fn run_checker(program: &str, args: &[&str], text: &str) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let text = text.to_string();
    tokio::spawn(async move {
        let _ = stdin.write_all(text.as_bytes()).await;
    });
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await.ok()?.ok()?;
    if out.status.success() {
        return None;
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // the last line of a Python/Node/bash report is the message; earlier lines are the source excerpt
    let msg = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("syntax error");
    Some(msg.trim().chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_what_a_token_limit_leaves_behind() {
        assert!(problem(Path::new("a.json"), "{\"a\": [1, 2").await.unwrap().starts_with("invalid JSON"));
        assert!(problem(Path::new("a.json"), "{\"a\": [1, 2]}").await.is_none());
        assert!(problem(Path::new("c.toml"), "port = \n").await.unwrap().starts_with("invalid TOML"));
        assert!(problem(Path::new("c.toml"), "port = 1\n").await.is_none());
        let cut = "<!doctype html><html><head><style>body{}</style></head><body><div class=\"a\"><h1>Hi</h1>";
        assert!(problem(Path::new("i.html"), cut).await.unwrap().contains("cut off"));
        assert!(problem(Path::new("i.html"), "<html><body><div><br/></div></body></html>").await.is_none());
        assert!(problem(Path::new("n.txt"), "{{{").await.is_none());
    }

    #[tokio::test]
    async fn real_interpreters_check_scripts_when_they_are_installed() {
        if let Some(e) = problem(Path::new("a.py"), "def f(:\n  pass\n").await {
            assert!(e.contains("syntax"), "{e}");
        }
        assert!(problem(Path::new("a.py"), "def f():\n    return 1\n").await.is_none());
        if let Some(e) = problem(Path::new("a.sh"), "if true; then\n  echo hi\n").await {
            assert!(e.contains("shell syntax"), "{e}");
        }
        assert!(problem(Path::new("a.sh"), "echo hi\n").await.is_none());
    }
}
