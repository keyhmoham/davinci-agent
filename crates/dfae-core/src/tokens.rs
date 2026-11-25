//! Token estimation and deterministic text compression.
//!
//! The engine never has a tokenizer for every provider, so it uses a cheap
//! estimator that is slightly conservative (over-counts) and prefers
//! provider-reported usage whenever it is available.

/// Estimate the number of LLM tokens in `s`.
pub fn estimate_tokens(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let chars = s.chars().count();
    let words = s.split_whitespace().count();
    // ~4 chars per token for prose; ~1.33 tokens per word as a floor.
    chars.div_ceil(4).max((words * 4).div_ceil(3))
}

/// Truncate `s` so that it fits in roughly `max_tokens` tokens.
pub fn truncate_tokens(s: &str, max_tokens: usize) -> String {
    if estimate_tokens(s) <= max_tokens {
        return s.to_string();
    }
    let max_chars = max_tokens * 4;
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Deterministically compress tool output before it enters a prompt.
///
/// * normalises line endings and trailing whitespace
/// * collapses runs of identical lines into `line (xN)`
/// * collapses runs of blank lines
/// * if still over budget keeps the head and tail and elides the middle
pub fn compress_text(s: &str, max_tokens: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut run: usize = 0;
    let mut last: Option<String> = None;
    let flush = |lines: &mut Vec<String>, last: &Option<String>, run: usize| {
        if let Some(l) = last {
            if run > 1 && !l.trim().is_empty() {
                lines.push(format!("{l} (x{run})"));
            } else if l.trim().is_empty() {
                lines.push(String::new());
            } else {
                lines.push(l.clone());
            }
        }
    };
    for raw in s.replace("\r\n", "\n").lines() {
        let l = raw.trim_end().to_string();
        if last.as_deref() == Some(l.as_str()) {
            run += 1;
        } else {
            flush(&mut lines, &last, run);
            last = Some(l);
            run = 1;
        }
    }
    flush(&mut lines, &last, run);
    // collapse blank-line runs
    let mut out: Vec<String> = Vec::new();
    for l in lines {
        if l.is_empty() && out.last().map(|p: &String| p.is_empty()).unwrap_or(true) {
            continue;
        }
        out.push(l);
    }
    let text = out.join("\n").trim().to_string();
    if estimate_tokens(&text) <= max_tokens {
        return text;
    }
    elide_middle(&text, max_tokens)
}

fn elide_middle(text: &str, max_tokens: usize) -> String {
    let budget_chars = max_tokens.saturating_mul(4).max(80);
    let total: usize = text.chars().count();
    if total <= budget_chars {
        return text.to_string();
    }
    let head_chars = budget_chars * 6 / 10;
    let tail_chars = budget_chars - head_chars;
    let head: String = text.chars().take(head_chars).collect();
    let tail: String = text.chars().skip(total - tail_chars).collect();
    let elided = total - head_chars - tail_chars;
    format!("{head}\n…[{elided} chars elided]…\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_are_monotonic() {
        assert_eq!(estimate_tokens(""), 0);
        assert!(estimate_tokens("hello world") >= 2);
        assert!(estimate_tokens(&"word ".repeat(100)) > estimate_tokens("word word"));
    }

    #[test]
    fn compress_dedupes_and_elides() {
        let s = "a\na\na\nb\n\n\n\nc";
        let c = compress_text(s, 100);
        assert_eq!(c, "a (x3)\nb\n\nc");
        let big = (0..2000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let c = compress_text(&big, 100);
        assert!(estimate_tokens(&c) < 200);
        assert!(c.contains("elided"));
        assert!(c.starts_with("line 0"));
        assert!(c.ends_with("line 1999"));
    }
}
