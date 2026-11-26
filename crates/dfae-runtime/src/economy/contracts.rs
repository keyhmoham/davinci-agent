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
/// A short reply that is only a question for the user ("What information do you want to include?"). The agent never asks:
/// a step that comes back like this has not done its work.
pub fn asks_the_user(text: &str) -> bool {
    static ASK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?is)\b(?:what|which|where|when|how)\b[^?]*\b(?:do you|would you|should i|shall i)\b|\b(?:could|can|would|will) you (?:please |kindly )?(?:provide|tell|specify|share|clarify|confirm|let me know)\b|\bplease (?:provide|tell me|specify|let me know|clarify)\b|\bI(?:'ll| will)? need (?:some |more |a few )?(?:more )?(?:details|information|content|input)\b").unwrap()
    });
    let t = text.trim();
    t.split_whitespace().count() <= 70 && t.ends_with('?') && ASK.is_match(t)
}

/// A plain reply that is really an attempt to call a tool ("<tool_call>Tool: web.search {…}"): a model that was told tools exist,
/// in a place where it has none. Showing that to a user is wrong, and acting on it is not this layer's job.
pub fn pseudo_tool_call(text: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?im)<\s*/?\s*tool_call\s*>|<\s*/?\s*function_call\s*>|^\s*tool\s*:\s*[\w.\-]+\s*$|^\s*\{\s*"(?:tool|tool_name|name)"\s*:\s*"[\w.\-]+"\s*,\s*"(?:arguments|args|parameters|input)""#).unwrap()
    });
    RE.is_match(text)
}

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
    if asks_the_user(t) {
        return Some("asks the user instead of deciding".into());
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
    fn a_reply_that_only_asks_the_user_is_not_a_result() {
        assert!(asks_the_user("To create a landing page about Earth, I'll need some content and design details. What information do you want to include?"));
        assert!(asks_the_user("Could you please provide the file name?"));
        assert!(!asks_the_user("Earth is the third planet. Why does it matter?"), "a question inside content is not a request for input");
        assert!(!asks_the_user("The port is 8080."));
        assert!(model_output_issue("What would you like the page to say?").is_some());
    }

    #[test]
    fn a_reply_that_is_a_tool_call_in_disguise_is_recognised() {
        assert!(pseudo_tool_call("<tool_call>Tool: web.search\n{\n\"query\": \"bitcoin\"\n}"));
        assert!(pseudo_tool_call("Tool: web_search\n{\"query\": \"x\"}"));
        assert!(pseudo_tool_call("{\"name\": \"web_search\", \"arguments\": {\"query\": \"x\"}}"));
        assert!(!pseudo_tool_call("Bitcoin is at 91,000 USD."));
        assert!(!pseudo_tool_call("You can call it a tool: a hammer is one."));
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
