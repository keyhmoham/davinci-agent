//! What the content writer must know about the project a file belongs to (coding plan B1, B4).
//!
//! A page written into a generated Next.js app used to be told "a web page is a full HTML document with its styles inside it and no
//! external files" and to see only the files this task wrote, never the scaffold's `package.json`, `layout.tsx` or `globals.css`. It
//! answered with inline style objects (`as React.CSSProperties` in a `.jsx` file) that ignored the project's Tailwind. Here code finds the
//! project (the nearest manifest above the file), says in one line what kind of file is being written, and summarises the project in a
//! compact card: dependencies and their majors, the TypeScript path aliases, the root layout's imports, the start of the global stylesheet.

use std::path::{Path, PathBuf};

/// A project found above a file: the directory of its manifest, relative to the workspace.
#[derive(Debug, Clone, Default)]
pub(crate) struct Project {
    /// The project's directory, relative to the workspace (`""` for the workspace itself).
    pub dir: String,
    pub full: PathBuf,
    pub manifest: &'static str,
    /// `(name, major version or the spec as written)` of the dependencies (and dev dependencies) of a `package.json`.
    pub deps: Vec<(String, String)>,
}

impl Project {
    pub fn has(&self, dep: &str) -> bool {
        self.deps.iter().any(|(n, _)| n == dep)
    }

    pub fn major(&self, dep: &str) -> Option<u32> {
        self.deps.iter().find(|(n, _)| n == dep).and_then(|(_, v)| v.parse().ok())
    }

    fn is_next(&self) -> bool {
        self.has("next")
    }
}

/// The project `rel` (a path relative to the workspace `ws`) belongs to: the nearest directory above it, inside the workspace, that holds a
/// `package.json`, `pyproject.toml`, `Cargo.toml`, `go.mod` or `requirements.txt`.
pub(crate) fn find(ws: &Path, rel: &str) -> Option<Project> {
    const MANIFESTS: &[&str] = &["package.json", "pyproject.toml", "Cargo.toml", "go.mod", "requirements.txt"];
    let rel = rel.trim_start_matches("./");
    let mut dir = Path::new(rel).parent().map(Path::to_path_buf).unwrap_or_default();
    loop {
        let full = ws.join(&dir);
        for m in MANIFESTS {
            if full.join(m).is_file() {
                let mut p = Project { dir: dir.to_string_lossy().trim_end_matches('/').to_string(), full: full.clone(), manifest: m, deps: vec![] };
                if *m == "package.json" {
                    p.deps = package_deps(&full.join(m));
                }
                return Some(p);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `(name, major)` of a `package.json`'s dependencies and dev dependencies; `"^15.2.0"` → `"15"`, `"latest"` stays `"latest"`.
fn package_deps(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![] };
    let mut out = Vec::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(m) = v.get(key).and_then(|d| d.as_object()) {
            for (name, spec) in m {
                let spec = spec.as_str().unwrap_or("");
                let digits: String = spec.trim_start_matches(|c: char| !c.is_ascii_digit()).chars().take_while(|c| c.is_ascii_digit()).collect();
                out.push((name.clone(), if digits.is_empty() { spec.to_string() } else { digits }));
            }
        }
    }
    out
}

fn ext_of(path: &str) -> String {
    Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn stem_of(path: &str) -> String {
    Path::new(path).file_stem().and_then(|e| e.to_str()).unwrap_or("").to_string()
}

/// The one line that says what kind of file this is and how it must be written, chosen by code from the path and the project.
pub(crate) fn content_contract(path: &str, project: Option<&Project>) -> String {
    let ext = ext_of(path);
    let stem = stem_of(path);
    let in_app = Path::new(path).components().any(|c| c.as_os_str() == "app");
    let next = project.is_some_and(Project::is_next);
    let tailwind = project.and_then(|p| p.has("tailwindcss").then(|| p.major("tailwindcss").unwrap_or(4)));
    let styling = match tailwind {
        Some(_) => "style with Tailwind utility classes in `className` (the project uses tailwindcss)",
        None => "style with `className` and the project's CSS (globals.css or a CSS module), never inline style objects for layout",
    };
    let lang = if matches!(ext.as_str(), "ts" | "tsx") { "TypeScript" } else { "plain JavaScript (no type annotations, no `as`)" };
    match ext.as_str() {
        "html" | "htm" if project.is_none_or(|p| p.manifest != "package.json") => {
            "A web page: a full HTML document with its styles inside it and no external files.".into()
        }
        "html" | "htm" => "A complete HTML document.".into(),
        "tsx" | "jsx" | "js" | "ts" if next && in_app && stem == "page" => format!(
            "A Next.js App Router page: a React Server Component with `export default function …`; no \"use client\" unless it uses hooks or \
             event handlers; {styling}; {lang}. Only import packages the project has."
        ),
        "tsx" | "jsx" | "js" | "ts" if next && in_app && stem == "layout" => format!(
            "The Next.js root layout: keep `<html>`/`<body>`, the `metadata` export and the global stylesheet import; `export default function \
             RootLayout({{ children }})`; {lang}."
        ),
        "tsx" | "jsx" => format!("A React component module: {styling}; {lang}; it must parse and import only what exists."),
        "ts" | "mts" | "js" | "mjs" | "cjs" => {
            format!("A {} module: complete, it must parse and run.", if ext.contains('t') { "TypeScript" } else { "JavaScript" })
        }
        "css" if tailwind == Some(3) => "The global stylesheet: keep the `@tailwind base; @tailwind components; @tailwind utilities;` lines first.".into(),
        "css" if tailwind.is_some() => "The global stylesheet: keep `@import \"tailwindcss\";` as the first line.".into(),
        "css" => "A stylesheet: complete, plain CSS.".into(),
        "json" => "Valid JSON, nothing else.".into(),
        "md" | "markdown" => "A Markdown document.".into(),
        _ => "Complete and ready to use: code compiles or runs.".into(),
    }
}

/// A compact card of the project the file belongs to (≈ 150–250 tokens): what the writer needs to fit in. `None` outside a project.
pub(crate) fn project_card(p: &Project) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let name = if p.dir.is_empty() { ".".to_string() } else { p.dir.clone() };
    out.push(format!("project {name}/ ({})", p.manifest));
    if !p.deps.is_empty() {
        let deps: Vec<String> = p.deps.iter().take(30).map(|(n, v)| format!("{n}@{v}")).collect();
        out.push(format!("dependencies: {}", deps.join(", ")));
    }
    // TypeScript path aliases (`@/*` → `./src/*`): an import that uses them must match
    if let Ok(text) = std::fs::read_to_string(p.full.join("tsconfig.json"))
        && let Some(paths) = jsonc_paths(&text)
    {
        out.push(format!("tsconfig paths: {paths}"));
    }
    // the root layout's imports (fonts, the global stylesheet) and where the global stylesheet is
    for layout in ["src/app/layout.tsx", "src/app/layout.jsx", "src/app/layout.js", "app/layout.tsx", "app/layout.jsx", "app/layout.js"] {
        if let Ok(text) = std::fs::read_to_string(p.full.join(layout)) {
            let imports: Vec<&str> = text.lines().filter(|l| l.trim_start().starts_with("import ")).take(8).collect();
            if !imports.is_empty() {
                out.push(format!("{layout} imports: {}", imports.join(" ; ")));
            }
            break;
        }
    }
    for css in ["src/app/globals.css", "app/globals.css", "src/styles/globals.css", "styles/globals.css", "src/index.css"] {
        if let Ok(text) = std::fs::read_to_string(p.full.join(css)) {
            let head: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(40).collect();
            out.push(format!("{css} (start):\n{}", head.join("\n")));
            break;
        }
    }
    (out.len() > 1).then(|| out.join("\n"))
}

/// The `compilerOptions.paths` of a tsconfig (which may hold comments and trailing commas), as `alias → target` pairs.
fn jsonc_paths(text: &str) -> Option<String> {
    let re = regex::Regex::new(r#""(@?[\w~/.*-]+)"\s*:\s*\[\s*"([^"]+)""#).ok()?;
    let at = text.find("\"paths\"")?;
    let rest = &text[at..];
    let block = &rest[..rest.find('}').unwrap_or(rest.len())];
    let pairs: Vec<String> = re.captures_iter(block).map(|c| format!("{} → {}", &c[1], &c[2])).collect();
    (!pairs.is_empty()).then(|| pairs.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_app(ws: &Path, tailwind: &str) {
        let d = ws.join("lab");
        std::fs::create_dir_all(d.join("src/app")).unwrap();
        std::fs::write(
            d.join("package.json"),
            format!(r#"{{"dependencies": {{"next": "15.5.4", "react": "^19.1.0"}}, "devDependencies": {{"tailwindcss": "{tailwind}", "typescript": "^5"}}}}"#),
        )
        .unwrap();
        std::fs::write(d.join("tsconfig.json"), "{\n  // comment\n  \"compilerOptions\": {\n    \"paths\": { \"@/*\": [\"./src/*\"] },\n  }\n}").unwrap();
        std::fs::write(
            d.join("src/app/layout.tsx"),
            "import type { Metadata } from \"next\";\nimport \"./globals.css\";\nexport default function RootLayout() {}",
        )
        .unwrap();
        std::fs::write(d.join("src/app/globals.css"), "@import \"tailwindcss\";\n\n:root { --bg: #fff; }\n").unwrap();
    }

    #[test]
    fn a_page_in_a_next_app_is_told_it_is_one_and_sees_the_project() {
        let ws = tempfile::tempdir().unwrap();
        next_app(ws.path(), "^4");
        let p = find(ws.path(), "lab/src/app/page.tsx").unwrap();
        assert_eq!(p.dir, "lab");
        assert_eq!(p.major("next"), Some(15));
        let c = content_contract("lab/src/app/page.tsx", Some(&p));
        assert!(c.contains("Server Component") && c.contains("Tailwind") && c.contains("TypeScript"), "{c}");
        assert!(!c.contains("styles inside it"), "{c}");
        let c = content_contract("lab/src/app/page.jsx", Some(&p));
        assert!(c.contains("plain JavaScript"), "{c}");
        assert!(content_contract("lab/src/app/globals.css", Some(&p)).contains("@import \"tailwindcss\""));
        assert!(content_contract("lab/src/app/layout.tsx", Some(&p)).contains("metadata"));
        let card = project_card(&p).unwrap();
        assert!(card.contains("next@15") && card.contains("tailwindcss@4") && card.contains("@/* → ./src/*"), "{card}");
        assert!(card.contains("import \"./globals.css\"") && card.contains("@import \"tailwindcss\""), "{card}");
    }

    #[test]
    fn a_plain_page_keeps_the_html_rule_and_tailwind_3_keeps_its_directives() {
        let ws = tempfile::tempdir().unwrap();
        assert!(find(ws.path(), "site/index.html").is_none());
        assert!(content_contract("site/index.html", None).contains("styles inside it"));
        assert!(content_contract("tool.py", None).contains("compiles or runs"));
        next_app(ws.path(), "3.4.1");
        let p = find(ws.path(), "lab/src/app/globals.css").unwrap();
        assert!(content_contract("lab/src/app/globals.css", Some(&p)).contains("@tailwind base"));
    }
}
