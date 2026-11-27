//! A short picture of what the workspace holds, for the planner. Knowing that `site/` exists, or that the workspace
//! is empty, saves the exploratory `fs_list` step (and its re-plan) that a planner otherwise writes for safety.

use std::path::Path;

const SKIP: &[&str] = &["node_modules", "target", "__pycache__", "venv", "dist", "build"];

/// `WORKSPACE: a.txt, site/ (index.html style.css), src/ (12 entries)` — two levels, capped, no hidden files.
pub fn snapshot(root: &Path, max_top: usize) -> String {
    let Ok(rd) = std::fs::read_dir(root) else { return String::new() };
    let mut top: Vec<(String, bool)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            (!name.starts_with('.')).then(|| (name, e.file_type().map(|t| t.is_dir()).unwrap_or(false)))
        })
        .collect();
    if top.is_empty() {
        return "WORKSPACE: empty".into();
    }
    top.sort();
    let origins = origins(root);
    let total = top.len();
    let mut parts = Vec::new();
    for (name, is_dir) in top.into_iter().take(max_top) {
        if !is_dir {
            parts.push(name);
            continue;
        }
        if SKIP.contains(&name.as_str()) {
            parts.push(format!("{name}/ (skipped)"));
            continue;
        }
        let mut kids: Vec<String> = std::fs::read_dir(root.join(&name))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { format!("{n}/") } else { n }
                    })
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        kids.sort();
        let n = kids.len();
        let shown: Vec<String> = kids.into_iter().take(8).collect();
        let by = origins.get(&name).map(|g| format!("by: \"{}\"; ", g.chars().take(60).collect::<String>())).unwrap_or_default();
        parts.push(if n == 0 {
            format!("{name}/ ({by}empty)")
        } else if n > 8 {
            format!("{name}/ ({by}{} …{n} entries)", shown.join(" "))
        } else {
            format!("{name}/ ({by}{})", shown.join(" "))
        });
    }
    if total > max_top {
        parts.push(format!("…+{} more", total - max_top));
    }
    format!("WORKSPACE: {}", parts.join(", "))
}

// ---------------------------------------------------- new work, new directory (coding plan B3) ----

/// Where the workspace remembers which request made which top-level directory (hidden: never shown as a file).
const ORIGINS: &str = ".dfae/dirs.json";

/// `directory → the request that made it`.
pub fn origins(root: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(root.join(ORIGINS)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Remember that `dir` was made for `goal` (a follow-up — "make its title bigger" — then finds it by what it was made for).
pub fn record_origin(root: &Path, dir: &str, goal: &str) {
    let mut all = origins(root);
    if all.contains_key(dir) {
        return;
    }
    all.insert(dir.to_string(), goal.chars().take(120).collect());
    let _ = std::fs::create_dir_all(root.join(".dfae"));
    if let Ok(text) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::write(root.join(ORIGINS), text);
    }
}

/// The top-level directories of the workspace (not hidden).
pub fn top_dirs(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Does the request ask for something new ("create a landing page…", "build me an app…") rather than a change to what exists?
pub fn new_work(goal: &str) -> bool {
    static NEW: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:create|build|make|generate|scaffold|set\s+up|start|develop|write)\b(?:\s+(?:me|us))?\s+(?:a|an|new|another|one)\b")
            .unwrap()
    });
    static EXISTING: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(?:existing|current|that|this|same|my|our|the)\s+(?:\w+\s+)?(?:app|project|site|website|page|directory|folder|repo|codebase)\b|\b(?:in|into|inside)\s+(?:the\s+)?[\w.-]+/").unwrap()
    });
    NEW.is_match(goal) && !EXISTING.is_match(goal)
}

/// A directory name for the request: what it is about, and what kind of thing it is (`scientific-laboratory-landing`).
pub fn slug(goal: &str) -> String {
    const STOP: &[&str] = &[
        "a",
        "an",
        "the",
        "and",
        "or",
        "of",
        "to",
        "in",
        "on",
        "for",
        "with",
        "using",
        "use",
        "it",
        "its",
        "me",
        "us",
        "my",
        "that",
        "this",
        "create",
        "build",
        "make",
        "generate",
        "write",
        "serve",
        "run",
        "deploy",
        "start",
        "set",
        "up",
        "please",
        "professional",
        "modern",
        "beautiful",
        "simple",
        "nice",
        "new",
        "nextjs",
        "next",
        "js",
        "react",
        "vue",
        "vite",
        "html",
        "css",
        "python",
        "rust",
        "node",
        "page",
        "about",
        "then",
        "is",
        "be",
        "can",
        "you",
        "some",
        "small",
        "single",
        "one",
        "good",
        "great",
    ];
    const KIND: &[&str] = &["landing", "site", "website", "app", "api", "blog", "dashboard", "portfolio", "shop", "store", "game", "cli", "bot", "docs"];
    let lower = goal.to_lowercase();
    let words: Vec<String> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect();
    let kind = words.iter().find(|w| KIND.contains(&w.as_str())).cloned();
    // the subject: what follows "about" / "for" / "on", else the first words that say anything
    let after = words.iter().position(|w| matches!(w.as_str(), "about" | "for" | "on" | "called" | "named")).map(|i| i + 1).unwrap_or(0);
    let subject: Vec<&String> = words[after..].iter().filter(|w| !STOP.contains(&w.as_str()) && !KIND.contains(&w.as_str()) && w.len() > 1).take(2).collect();
    let mut parts: Vec<String> = subject.into_iter().cloned().collect();
    if let Some(k) = kind {
        parts.push(k);
    }
    let s = parts.join("-");
    let s: String = s.chars().take(32).collect::<String>().trim_end_matches('-').to_string();
    if s.is_empty() { "project".into() } else { s }
}

/// `base`, or `base-2`, `base-3` … : the first name the workspace does not have yet.
pub fn free_dir(root: &Path, base: &str) -> String {
    if !root.join(base).exists() {
        return base.to_string();
    }
    (2..100).map(|i| format!("{base}-{i}")).find(|n| !root.join(n).exists()).unwrap_or_else(|| format!("{base}-{}", std::process::id()))
}

/// `text` with the directory `old` (as a path, or named as a directory) moved to `new`; prose that merely contains the word is left.
pub fn move_dir_in(text: &str, old: &str, new: &str) -> String {
    let o = regex::escape(old);
    let re = regex::Regex::new(&format!(
        r"(?i)(^|[\s`'\x22(=:,])(?:\./)?{o}(/|[`'\x22)]|\s+(?:directory|folder|dir|project)\b|$)|(\b(?:directory|folder|dir|cwd|into|inside|in)\s+[`'\x22]?){o}\b"
    ))
    .expect("dir pattern");
    re.replace_all(text, |c: &regex::Captures| match c.get(3) {
        Some(pre) => format!("{}{new}", pre.as_str()),
        None => format!("{}{new}{}", &c[1], &c[2]),
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_two_levels_and_caps_the_rest() {
        let d = std::env::temp_dir().join(format!("dfae-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("site")).unwrap();
        std::fs::create_dir_all(d.join("empty")).unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join("site/index.html"), "x").unwrap();
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        let s = snapshot(&d, 10);
        assert_eq!(s, "WORKSPACE: empty/ (empty), notes.txt, site/ (index.html)");
        assert!(snapshot(&d, 1).ends_with("…+2 more"));
        std::fs::remove_dir_all(d.join("site")).unwrap();
        std::fs::remove_dir_all(d.join("empty")).unwrap();
        std::fs::remove_file(d.join("notes.txt")).unwrap();
        assert_eq!(snapshot(&d, 10), "WORKSPACE: empty");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn new_work_gets_a_name_of_its_own_and_its_origin_is_remembered() {
        let g = "create a professional landing page about a scientific laboratory using nextjs and build and serve it";
        assert!(new_work(g));
        assert_eq!(slug(g), "scientific-laboratory-landing");
        assert_eq!(slug("build me a todo app with React"), "todo-app");
        assert!(!new_work("make the title of that page bigger"));
        assert!(!new_work("add a contact form to the existing site"));
        assert!(!new_work("create a page in site/ for the team"));
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("site")).unwrap();
        std::fs::write(d.path().join("site/index.html"), "x").unwrap();
        assert_eq!(free_dir(d.path(), "site"), "site-2");
        record_origin(d.path(), "site", "landing page about earth");
        assert!(snapshot(d.path(), 10).contains("site/ (by: \"landing page about earth\"; index.html)"), "{}", snapshot(d.path(), 10));
        assert_eq!(top_dirs(d.path()), ["site"]);
    }

    #[test]
    fn a_directory_is_moved_where_it_is_a_path_not_where_it_is_a_word() {
        let m = |t: &str| move_dir_in(t, "site", "lab-landing");
        assert_eq!(m("write site/app/page.tsx"), "write lab-landing/app/page.tsx");
        assert_eq!(m("run npm run build in site"), "run npm run build in lab-landing");
        assert_eq!(
            m("scaffold the app into the directory site with `npx create-next-app@15 site`"),
            "scaffold the app into the directory lab-landing with `npx create-next-app@15 lab-landing`"
        );
        assert_eq!(m("a site about the lab"), "a site about the lab", "prose stays");
        assert_eq!(m("http://localhost:8001 serves the website"), "http://localhost:8001 serves the website");
    }
}
