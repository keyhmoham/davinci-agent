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
        // parsed in-process, before any `npm install` (coding plan X6): no Node and no `node_modules/typescript` needed
        "js" | "mjs" | "cjs" | "jsx" => js_problem(path, text),
        "ts" | "tsx" | "mts" | "cts" => match js_problem(path, text) {
            Some(p) => Some(p),
            // the project's own compiler, when it has one, has the last word
            None => typescript_problem(path, text).await,
        },
        _ => None,
    }
}

/// Syntax errors of JavaScript / TypeScript / JSX source, found with `oxc_parser` (pure Rust, well under a millisecond per file). Also: TS
/// syntax in a `.js`/`.jsx` file (seen: `as React.CSSProperties` in a `.jsx` page), and a Next.js `page`/`layout` with no default export.
fn js_problem(path: &Path, text: &str) -> Option<String> {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mut st = SourceType::from_path(path).ok()?;
    if matches!(ext.as_str(), "js" | "jsx" | "mjs") {
        st = st.with_jsx(true); // `page.js` in a Next.js app is JSX
    }
    let typescript = st.is_typescript();
    let lang = if typescript { "TypeScript" } else { "JavaScript" };
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, text, st).parse();
    if let Some(e) = ret.errors.first() {
        if !typescript {
            // the same text parses as TypeScript: it is TS written into a JavaScript file
            let alloc_ts = Allocator::default();
            let as_ts = Parser::new(&alloc_ts, text, st.with_typescript(true)).parse();
            if as_ts.errors.is_empty() && !as_ts.panicked {
                return Some(format!(
                    "TypeScript syntax in a JavaScript file (line {}: `as`, type annotations or interfaces): write plain JavaScript in this .{ext} file",
                    diagnostic_line(text, e)
                ));
            }
        }
        let more = if ret.errors.len() > 1 { format!(" (and {} more)", ret.errors.len() - 1) } else { String::new() };
        return Some(format!("{lang} syntax error: line {}: {}{more}", diagnostic_line(text, e), first_line(&e.message)));
    }
    if ret.panicked {
        return Some(format!("{lang} syntax error: the file could not be parsed"));
    }
    // app/**/page.tsx, app/layout.jsx …: Next.js renders the default export, and without one the build fails
    let stem = path.file_stem()?.to_str()?;
    let in_app = path.components().any(|c| c.as_os_str() == "app");
    if in_app && matches!(stem, "page" | "layout" | "template" | "loading" | "error" | "not-found" | "default") {
        use oxc_syntax::module_record::ExportExportName;
        let mr = &ret.module_record;
        let has_default = mr.local_export_entries.iter().chain(mr.indirect_export_entries.iter()).any(|e| match &e.export_name {
            ExportExportName::Default(_) => true,
            ExportExportName::Name(n) => n.name == "default",
            _ => false,
        });
        if !has_default {
            return Some(format!("{stem}.{ext} under app/ has no default export: Next.js renders `export default function …`"));
        }
    }
    None
}

/// The 1-based line a parser diagnostic points at.
fn diagnostic_line(text: &str, e: &oxc_diagnostics::OxcDiagnostic) -> usize {
    let at = e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset()).unwrap_or(0).min(text.len());
    text[..at].matches('\n').count() + 1
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
/// Syntax errors of TypeScript / JSX source, found with the project's own `typescript` package (no type checking: only whether it parses).
/// Without one in a `node_modules` above the file there is nothing to check with, and nothing is claimed.
async fn typescript_problem(path: &Path, text: &str) -> Option<String> {
    const SCRIPT: &str = r#"const ts=require(process.argv[1]);const src=require('fs').readFileSync(0,'utf8');
const r=ts.transpileModule(src,{fileName:process.argv[2],reportDiagnostics:true,compilerOptions:{jsx:ts.JsxEmit.Preserve,target:ts.ScriptTarget.ESNext,module:ts.ModuleKind.ESNext}});
const d=(r.diagnostics||[]).filter(x=>x.category===ts.DiagnosticCategory.Error).slice(0,3).map(x=>{const p=x.file?x.file.getLineAndCharacterOfPosition(x.start):{line:0};return 'line '+(p.line+1)+': '+ts.flattenDiagnosticMessageText(x.messageText,' ')});
if(d.length){console.error(d.join(' | '));process.exit(1)}"#;
    let mut dir = path.parent();
    let mut found = None;
    for _ in 0..10 {
        let d = dir?;
        let candidate = d.join("node_modules/typescript/lib/typescript.js");
        if candidate.is_file() {
            found = Some(candidate);
            break;
        }
        dir = d.parent();
    }
    let ts = found?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    run_checker("node", &["-e", SCRIPT, &ts.to_string_lossy(), &name], text).await.map(|e| format!("TypeScript syntax error: {e}"))
}

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
    async fn typescript_is_parsed_with_the_projects_own_compiler() {
        let ws = std::env::temp_dir().join("dfae-ts-syntax");
        let Some(real) = [
            "/usr/lib/node_modules/typescript",
            "/usr/local/lib/node_modules/typescript",
            "/opt/node22/lib/node_modules/typescript",
            "/opt/node-tools/node_modules/typescript",
        ]
        .into_iter()
        .find(|p| Path::new(p).is_dir()) else {
            return; // no TypeScript on this machine: nothing to check with, and nothing is claimed
        };
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(real, ws.join("node_modules/typescript")).unwrap();
        let file = ws.join("src/page.tsx");
        let bad = problem(&file, "export default function P() { const a = \"unterminated;\n return <div>{a}</div>; }\n").await;
        assert!(bad.as_deref().is_some_and(|e| e.starts_with("TypeScript syntax error: line 1")), "{bad:?}");
        assert!(problem(&file, "export default function P() { const a = 'x'; return <div style={{ width: a }}>{a}</div>; }\n").await.is_none());
        // no project and no compiler: still parsed (in-process), so an unterminated string is caught before any install
        assert!(problem(Path::new("/nonexistent/x.tsx"), "const a = \"").await.is_some_and(|e| e.starts_with("TypeScript syntax error")));
    }

    #[tokio::test]
    async fn javascript_and_typescript_are_parsed_without_node_or_a_project() {
        // 8d665de7 / 6e0c3ebc: `layout.jsx` with the code before `<html` cut away, written before `npm install`
        let truncated = "<html lang=\"en\">\n      <body>{children}</body>\n    </html>\n  )\n}\n";
        let e = problem(Path::new("/nowhere/site/app/layout.jsx"), truncated).await.unwrap();
        assert!(e.starts_with("JavaScript syntax error: line"), "{e}");
        // TS-only syntax in a .jsx page
        let ts_in_jsx = "export default function Page() {\n  const s = { color: 'red' } as React.CSSProperties;\n  return <div style={s}>Lab</div>;\n}\n";
        let e = problem(Path::new("/nowhere/site/app/page.jsx"), ts_in_jsx).await.unwrap();
        assert!(e.contains("TypeScript syntax in a JavaScript file") && e.contains("line 2"), "{e}");
        // the same is fine in a .tsx file
        assert!(problem(Path::new("/nowhere/site/app/page.tsx"), ts_in_jsx).await.is_none());
        // a page with no default export
        let e = problem(Path::new("/nowhere/site/app/page.tsx"), "export function Page() { return <main />; }\n").await.unwrap();
        assert!(e.contains("no default export"), "{e}");
        assert!(problem(Path::new("/nowhere/site/app/page.tsx"), "function Page() { return <main />; }\nexport { Page as default };\n").await.is_none());
        assert!(
            problem(Path::new("/nowhere/src/components/Hero.tsx"), "export function Hero() { return <h1/>; }\n").await.is_none(),
            "only app/ pages need one"
        );
        // ESM and JSX in a .js file are normal in a Next.js project; CommonJS still parses
        assert!(
            problem(Path::new("/nowhere/app/page.js"), "import Link from 'next/link'\nexport default function P() { return <Link href=\"/\">x</Link> }\n")
                .await
                .is_none()
        );
        assert!(problem(Path::new("/nowhere/server.js"), "const http = require('http');\nmodule.exports = http;\n").await.is_none());
        assert!(problem(Path::new("/nowhere/a.ts"), "const a: number = ;\n").await.unwrap().starts_with("TypeScript syntax error: line 1"));
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
