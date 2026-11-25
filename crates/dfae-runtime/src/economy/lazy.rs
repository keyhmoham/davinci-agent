//! Optional lazy context expansion for reasoning steps.
//!
//! Dependencies are shown as short summaries; if they are not enough the model
//! replies `NEED <step id>` and gets that step's fuller output on one retry.
//! The reply is only honoured when it is short and names real dependencies, so a
//! model that merely *mentions* "need" in an answer is never misread.

pub fn need_instruction(ids: &[String]) -> String {
    format!("\nIf INPUTS are not enough to do this step, reply with exactly `NEED <id>` (one of: {}) and nothing else.", ids.join(", "))
}

/// Dependency ids the model asked for, or empty if the reply is an answer.
pub fn parse_need(text: &str, valid: &[String]) -> Vec<String> {
    let t = text.trim();
    if t.len() > 80 || !t.to_ascii_lowercase().starts_with("need") {
        return vec![];
    }
    let rest = t[4..].trim_start_matches([':', ' ', '`']);
    let mut out: Vec<String> = Vec::new();
    for tok in rest.split(|c: char| c == ',' || c == ';' || c == '`' || c.is_whitespace()) {
        if valid.iter().any(|v| v == tok) && !out.iter().any(|o| o == tok) {
            out.push(tok.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec!["s1".into(), "s2".into()]
    }

    #[test]
    fn recognises_only_genuine_requests() {
        assert_eq!(parse_need("NEED s1", &ids()), vec!["s1"]);
        assert_eq!(parse_need("  need: `s2`, s1 ", &ids()), vec!["s2", "s1"]);
        assert!(parse_need("NEED s9", &ids()).is_empty(), "unknown ids are ignored");
        assert!(parse_need("You need to open s1 and then read the whole file carefully for details", &ids()).is_empty());
        assert!(parse_need("The answer is 4. NEED s1", &ids()).is_empty());
        assert!(need_instruction(&ids()).contains("s1, s2"));
    }
}
