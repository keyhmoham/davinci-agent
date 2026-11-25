//! Zero-cost pre/post-conditions. Each stops bad data at the node that produced
//! it, before a model call is spent on it downstream.

use crate::planner::template_refs;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Substitute `{{id}}` templates, refusing to run a step whose input is missing or empty.
pub fn resolve_templates(args: &Value, outputs: &HashMap<&str, &str>, render: &dyn Fn(&str) -> String) -> Result<Value, String> {
    let mut bad: Vec<String> = Vec::new();
    for id in template_refs(args) {
        match outputs.get(id.as_str()) {
            None => bad.push(format!("{id} has not produced a result")),
            Some(o) if o.trim().is_empty() => bad.push(format!("{id} produced no output")),
            _ => {}
        }
    }
    if !bad.is_empty() {
        bad.dedup();
        return Err(format!("input {} — the step was not run", bad.join("; ")));
    }
    Ok(crate::planner::substitute(args, &|id| outputs.get(id).map(|o| render(o))))
}

/// Why a model's step output must not be passed on, if it must not.
pub fn model_output_issue(text: &str) -> Option<String> {
    let t = text.trim();
    if !t.chars().any(|c| c.is_alphanumeric()) {
        return Some("empty output".into());
    }
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.len() >= 30 {
        let unique: HashSet<String> = words.iter().map(|w| w.to_lowercase()).collect();
        if (unique.len() as f64) / (words.len() as f64) < 0.2 {
            return Some("degenerate repetition".into());
        }
    }
    if t.contains("<data src=") || t.starts_with("STEP:") || t.contains("\nINPUTS:") {
        return Some("the model echoed its prompt".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn templates_resolve_or_the_step_is_not_run() {
        let outs: HashMap<&str, &str> = [("s1", "42"), ("s2", "  ")].into_iter().collect();
        let ok = resolve_templates(&json!({"e": "{{s1}} + 1"}), &outs, &|o| o.to_string()).unwrap();
        assert_eq!(ok, json!({"e": "42 + 1"}));
        let e = resolve_templates(&json!({"e": "{{s2}}"}), &outs, &|o| o.to_string()).unwrap_err();
        assert!(e.contains("s2 produced no output") && e.contains("not run"), "{e}");
        let e = resolve_templates(&json!({"e": "{{s7}}"}), &outs, &|o| o.to_string()).unwrap_err();
        assert!(e.contains("s7 has not produced a result"));
        assert_eq!(resolve_templates(&json!({"e": "plain"}), &outs, &|o| o.to_string()).unwrap(), json!({"e": "plain"}));
    }

    #[test]
    fn rejects_junk_model_output_only() {
        assert_eq!(model_output_issue("   ").as_deref(), Some("empty output"));
        assert_eq!(model_output_issue(".").as_deref(), Some("empty output"));
        assert!(model_output_issue(&"the the the ".repeat(20)).unwrap().contains("repetition"));
        assert!(model_output_issue("STEP: summarise\nGOAL: x").unwrap().contains("echoed"));
        assert!(model_output_issue("<data src=\"s1\">x</data>").is_some());
        assert!(model_output_issue("4").is_none(), "a legitimately short answer passes");
        assert!(model_output_issue("The file lists two open tasks: docs and tests.").is_none());
        let long: String = (0..60).map(|i| format!("word{i} ")).collect();
        assert!(model_output_issue(&long).is_none());
    }
}
