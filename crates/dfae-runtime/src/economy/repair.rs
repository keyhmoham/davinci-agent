//! Deterministic repair: fix what code can fix, ask a model only for the rest.
//!
//! Two situations, one set of fixes:
//! * **static** — a freshly parsed plan has argument problems (wrong types, an
//!   unknown argument, a missing required one that the request itself supplies);
//! * **runtime** — a step failed with `InvalidArgs` or `NotFound` (the file is
//!   one directory over from where the plan looked).
//!
//! Policy denials, timeouts and tool failures are never repaired here, and a
//! repaired step is still validated and policy-checked exactly like any other.

use dfae_core::*;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Fix {
    pub args: Value,
    pub note: String,
}

fn coerce(ty: ParamType, v: &Value) -> Option<Value> {
    match (ty, v) {
        (ParamType::String, Value::String(s)) if s.contains("{{") => None,
        (ParamType::String, Value::Number(n)) => Some(json!(n.to_string())),
        (ParamType::String, Value::Bool(b)) => Some(json!(b.to_string())),
        (ParamType::Integer, Value::String(s)) => s.trim().parse::<i64>().ok().map(|n| json!(n)),
        (ParamType::Integer, Value::Number(n)) => n.as_f64().filter(|f| f.fract() == 0.0).map(|f| json!(f as i64)),
        (ParamType::Number, Value::String(s)) => s.trim().parse::<f64>().ok().map(|n| json!(n)),
        (ParamType::Boolean, Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "1" => Some(json!(true)),
            "false" | "no" | "0" => Some(json!(false)),
            _ => None,
        },
        _ => None,
    }
}

/// The value a parameter takes from the request's entities — only when there is exactly one candidate:
/// with two file paths in the sentence ("copy a.txt to b.txt") which one is which is a question for a model, not a guess.
fn entity_value(p: &ParamSpec, goal: &str, entities: &[Entity]) -> Option<Value> {
    let mut candidates: Vec<&Entity> = Vec::new();
    for e in entities.iter().filter(|e| p.entity_hints.iter().any(|h| h == &e.label) && goal.contains(e.text.as_str())) {
        if !candidates.iter().any(|c| c.text == e.text) {
            candidates.push(e);
        }
    }
    match candidates.as_slice() {
        [e] => coerce(p.ty, &json!(e.text)).or_else(|| Some(json!(e.text))),
        _ => None,
    }
}

/// Make `args` satisfy `meta`'s schema if that can be done without guessing.
pub fn fix_args(meta: &ToolMetadata, args: &Value, goal: &str, entities: &[Entity]) -> Option<Fix> {
    if meta.validate_args(args).is_ok() {
        return None;
    }
    let mut obj: Map<String, Value> = match args {
        Value::Object(m) => m.clone(),
        Value::Null => Map::new(),
        _ => return None,
    };
    let mut notes: Vec<String> = Vec::new();
    let known: HashSet<&str> = meta.params.iter().map(|p| p.name.as_str()).collect();
    for k in obj.keys().cloned().collect::<Vec<_>>() {
        if !known.contains(k.as_str()) {
            obj.remove(&k);
            notes.push(format!("dropped unknown argument '{k}'"));
        }
    }
    for p in &meta.params {
        if let Some(v) = obj.get(&p.name).cloned() {
            if v.is_null() {
                obj.remove(&p.name);
            } else if let Some(nv) = coerce(p.ty, &v).filter(|nv| nv != &v) {
                obj.insert(p.name.clone(), nv);
                notes.push(format!("converted '{}' to {:?}", p.name, p.ty).to_lowercase());
            }
        }
    }
    let missing: Vec<&ParamSpec> = meta.params.iter().filter(|p| p.required && !obj.contains_key(&p.name)).collect();
    for p in missing {
        if let Some(v) = entity_value(p, goal, entities).or_else(|| p.default.clone()) {
            notes.push(format!("filled '{}' from the request", p.name));
            obj.insert(p.name.clone(), v);
        }
    }
    let fixed = Value::Object(obj);
    (meta.validate_args(&fixed).is_ok() && !notes.is_empty()).then(|| Fix { args: fixed, note: notes.join("; ") })
}

/// Fill the *required* parameters that the sentence and its entities supply, even when the arguments stay
/// incomplete (`fix_args` only answers when the result is valid). Returns the arguments and whether anything was added.
pub fn fill_partial(meta: &ToolMetadata, args: &Value, goal: &str, entities: &[Entity]) -> (Value, bool) {
    let mut obj: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    let mut added = false;
    for p in meta.params.iter().filter(|p| p.required) {
        if obj.get(&p.name).map(|v| !v.is_null()).unwrap_or(false) {
            continue;
        }
        if let Some(v) = entity_value(p, goal, entities) {
            obj.insert(p.name.clone(), v);
            added = true;
        }
    }
    (Value::Object(obj), added)
}

/// If a path argument does not exist but exactly one file or directory of that
/// name exists elsewhere in the workspace, use it.
pub fn relocate_path(meta: &ToolMetadata, args: &Value, workspace: &Path) -> Option<Fix> {
    let obj = args.as_object()?;
    for p in
        meta.params.iter().filter(|p| p.ty == ParamType::String && (p.name == "path" || p.entity_hints.iter().any(|h| h == "file path" || h == "directory")))
    {
        let Some(Value::String(v)) = obj.get(&p.name) else { continue };
        if v.contains("{{") || v.trim().is_empty() {
            continue;
        }
        let want = Path::new(v).file_name()?.to_string_lossy().to_lowercase();
        let mut hits = find_by_name(workspace, &want);
        hits.sort();
        hits.dedup();
        if hits.len() == 1 {
            let rel = hits[0].strip_prefix(workspace).unwrap_or(&hits[0]).to_string_lossy().into_owned();
            if &rel != v {
                let mut new = obj.clone();
                new.insert(p.name.clone(), json!(rel));
                return Some(Fix { args: Value::Object(new), note: format!("'{v}' was not found; using the only match '{rel}'") });
            }
        }
    }
    None
}

/// Bounded breadth-first search of the workspace for entries named `name` (lowercase).
fn find_by_name(root: &Path, name: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut seen = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 4000 {
                return out;
            }
            let fname = e.file_name().to_string_lossy().to_lowercase();
            if matches!(fname.as_str(), ".git" | "target" | "node_modules" | ".env" | ".ssh" | ".aws") {
                continue;
            }
            if fname == name {
                out.push(e.path());
            }
            if depth < 5 && e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false) {
                queue.push_back((e.path(), depth + 1));
            }
        }
    }
    out
}

/// The repair appropriate to a runtime failure, if there is one.
pub fn repair_after_failure(meta: &ToolMetadata, args: &Value, kind: ErrorKind, goal: &str, entities: &[Entity], workspace: &Path) -> Option<Fix> {
    match kind {
        ErrorKind::InvalidArgs => fix_args(meta, args, goal, entities),
        ErrorKind::NotFound => relocate_path(meta, args, workspace),
        _ => None, // policy, timeouts, transient and tool failures are not argument problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ToolMetadata {
        ToolMetadata {
            name: "fs_read".into(),
            description: String::new(),
            category: "filesystem".into(),
            risk: Risk::Low,
            requires_approval: false,
            side_effects: false,
            reversible: true,
            cost_class: "free".into(),
            params: vec![
                ParamSpec::new("path", ParamType::String, true, "").hints(&["file path"]),
                ParamSpec::new("max_bytes", ParamType::Integer, false, ""),
                ParamSpec::new("append", ParamType::Boolean, false, ""),
            ],
            keywords: vec![],
            direct_patterns: vec![],
            cacheable: false,
            contract: vec![],
        }
    }

    fn ent(l: &str, t: &str) -> Entity {
        Entity { label: l.into(), text: t.into(), start: None, end: None, score: 1.0, source: "t".into() }
    }

    #[test]
    fn fixes_types_unknown_arguments_and_missing_values_from_the_request() {
        let m = meta();
        let f = fix_args(&m, &json!({"path": "a.txt", "max_bytes": "100", "append": "yes", "encoding": "utf8"}), "read a.txt", &[]).unwrap();
        assert_eq!(f.args, json!({"path": "a.txt", "max_bytes": 100, "append": true}));
        assert!(f.note.contains("dropped unknown argument 'encoding'") && f.note.contains("max_bytes"), "{}", f.note);
        let f = fix_args(&m, &json!({}), "please read notes/a.txt now", &[ent("file path", "notes/a.txt")]).unwrap();
        assert_eq!(f.args, json!({"path": "notes/a.txt"}));
        // nothing to go on: no guessing
        assert!(fix_args(&m, &json!({}), "read it", &[]).is_none());
        assert!(fix_args(&m, &json!({"path": "a", "max_bytes": "lots"}), "", &[]).is_none());
        assert!(fix_args(&m, &json!({"path": "a"}), "", &[]).is_none(), "already valid");
        assert!(fix_args(&m, &json!("not an object"), "", &[]).is_none());
    }

    #[test]
    fn relocates_a_path_only_when_the_match_is_unique() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("notes/deep")).unwrap();
        std::fs::write(d.path().join("notes/todo.md"), "x").unwrap();
        std::fs::write(d.path().join("notes/deep/dup.txt"), "x").unwrap();
        std::fs::write(d.path().join("dup.txt"), "x").unwrap();
        let m = meta();
        let f = relocate_path(&m, &json!({"path": "todo.md"}), d.path()).unwrap();
        assert_eq!(f.args, json!({"path": "notes/todo.md"}));
        assert!(f.note.contains("only match"));
        assert!(relocate_path(&m, &json!({"path": "TODO.MD"}), d.path()).is_some(), "case-insensitive");
        assert!(relocate_path(&m, &json!({"path": "dup.txt"}), d.path()).is_none(), "two candidates: ambiguous, so not guessed");
        assert!(relocate_path(&m, &json!({"path": "missing.txt"}), d.path()).is_none());
        assert!(relocate_path(&m, &json!({"path": "notes/todo.md"}), d.path()).is_none(), "already the right path");
    }

    #[test]
    fn only_argument_problems_are_repairable() {
        let m = meta();
        let d = tempfile::tempdir().unwrap();
        for k in [ErrorKind::Policy, ErrorKind::Timeout, ErrorKind::Transient, ErrorKind::Io, ErrorKind::Other, ErrorKind::Cancelled] {
            assert!(repair_after_failure(&m, &json!({"path": "x", "max_bytes": "5"}), k, "", &[], d.path()).is_none(), "{k:?}");
        }
        assert!(repair_after_failure(&m, &json!({"path": "x", "max_bytes": "5"}), ErrorKind::InvalidArgs, "", &[], d.path()).is_some());
    }
}
