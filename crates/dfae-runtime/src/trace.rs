//! The code an error points at. A build or a test run says `src/app/page.tsx:453` or `File "app.py", line 12`; a planner that has to ask for the
//! file first spends a round (and its tokens) on a read. Here the lines are found and handed over with the error.

use dfae_tools::PolicyEngine;
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

static PY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"File "([^"]+)", line (\d+)"#).unwrap());
static FILE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([A-Za-z0-9_./@\-]+\.(?:py|tsx?|jsx?|mjs|cjs|rs|go|java|kt|rb|php|c|cc|cpp|h|hpp|cs|swift|vue|svelte|scss|css|html)):(\d+)(?::\d+)?").unwrap()
});

/// Where a path of an error message is in the workspace: as written, or relative to the directory the failing command ran in.
fn locate(policy: &PolicyEngine, raw: &str, cwds: &[&str]) -> Option<std::path::PathBuf> {
    let raw = raw.trim_start_matches("./");
    if ["node_modules/", "site-packages/", "/.venv/", ".venv/", "/.tools/", ".tools/", ".next/", "/usr/", "/lib/", "target/debug/", "dist-packages/"]
        .iter()
        .any(|s| raw.contains(s))
    {
        return None;
    }
    let ws = policy.workspace().to_string_lossy().to_string();
    let relative = raw.strip_prefix(&format!("{ws}/")).unwrap_or(raw);
    if relative.starts_with('/') {
        return None;
    }
    let mut tries = vec![relative.to_string()];
    tries.extend(cwds.iter().filter(|c| !c.is_empty() && **c != ".").map(|c| format!("{}/{relative}", c.trim_end_matches('/'))));
    tries.into_iter().find_map(|t| policy.guard().read_path(&t).ok().filter(|p| p.is_file()))
}

/// The lines around each `file:line` an error text names (at most three files, ±9 lines each), numbered, the named line marked.
pub fn code_at_errors(policy: &PolicyEngine, errors: &[(String, Option<String>)]) -> String {
    let mut seen: Vec<(std::path::PathBuf, usize)> = Vec::new();
    let mut out = String::new();
    for (text, cwd) in errors {
        let cwds: Vec<&str> = cwd.as_deref().into_iter().collect();
        let refs = PY.captures_iter(text).chain(FILE_LINE.captures_iter(text)).filter_map(|c| Some((c[1].to_string(), c[2].parse::<usize>().ok()?)));
        for (raw, line) in refs {
            let Some(path) = locate(policy, &raw, &cwds) else { continue };
            if seen.iter().any(|(p, l)| *p == path && l.abs_diff(line) < 8)
                || seen.iter().map(|(p, _)| p).collect::<std::collections::HashSet<_>>().len() >= 3 && !seen.iter().any(|(p, _)| *p == path)
            {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else { continue };
            let lines: Vec<&str> = src.lines().collect();
            if line == 0 || line > lines.len() {
                continue;
            }
            let (from, to) = (line.saturating_sub(9).max(1), (line + 9).min(lines.len()));
            let shown: Vec<String> =
                (from..=to).map(|n| format!("{}{n:>4} | {}", if n == line { ">" } else { " " }, lines[n - 1].chars().take(160).collect::<String>())).collect();
            out.push_str(&format!("{}:{line}\n{}\n", relative(policy.workspace(), &path), shown.join("\n")));
            seen.push((path, line));
        }
    }
    if out.is_empty() { out } else { format!("CODE AT THE ERROR (the current text of the files it names):\n{}", out.trim_end()) }
}

fn relative(ws: &Path, p: &Path) -> String {
    let ws = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
    p.strip_prefix(&ws).unwrap_or(p).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfae_tools::PolicyConfig;

    #[test]
    fn the_lines_an_error_names_come_with_it() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("app/src")).unwrap();
        let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(ws.path().join("app/src/page.tsx"), &body).unwrap();
        std::fs::write(ws.path().join("tool.py"), "a = 1\nb = oops\n").unwrap();
        let policy = PolicyEngine::new(PolicyConfig::default(), ws.path().to_path_buf());
        let next = "    at <unknown> (src/src/../src/page.tsx:20:3)\n  Error occurred prerendering page \"/\"";
        // the path of a build run in `app` is relative to `app`
        let got = code_at_errors(&policy, &[("at <unknown> (src/page.tsx:20:3)".to_string(), Some("app".into()))]);
        assert!(got.contains("src/page.tsx:20") && got.contains(">  20 | line 20") && got.contains("  11 | line 11") && !got.contains("line 30"), "{got}");
        let py = code_at_errors(&policy, &[("Traceback...\n  File \"tool.py\", line 2, in <module>\nNameError: oops".to_string(), None)]);
        assert!(py.contains(">   2 | b = oops"), "{py}");
        // library code, files that are not there, and paths outside the workspace are not offered
        let none = code_at_errors(&policy, &[("File \"/usr/lib/python3/x.py\", line 3\n at node_modules/x/index.js:4\n at missing.ts:2".to_string(), None)]);
        assert!(none.is_empty(), "{none}");
        let _ = next;
    }
}
