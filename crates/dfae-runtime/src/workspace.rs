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
        parts.push(if n == 0 {
            format!("{name}/ (empty)")
        } else if n > 8 {
            format!("{name}/ ({} …{n} entries)", shown.join(" "))
        } else {
            format!("{name}/ ({})", shown.join(" "))
        });
    }
    if total > max_top {
        parts.push(format!("…+{} more", total - max_top));
    }
    format!("WORKSPACE: {}", parts.join(", "))
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
}
