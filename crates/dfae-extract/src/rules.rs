//! Rule-based extractor for structured spans.

use async_trait::async_trait;
use dfae_core::*;
use regex::Regex;
use std::sync::LazyLock;
use std::time::Instant;

struct Rule {
    label: &'static str,
    re: Regex,
    group: usize,
    score: f32,
}

fn r(label: &'static str, pat: &str, group: usize, score: f32) -> Rule {
    Rule { label, re: Regex::new(pat).unwrap_or_else(|e| panic!("bad rule {label}: {e}")), group, score }
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        r("url", r#"https?://[^\s)>\]"'`]+"#, 0, 1.0),
        r("email", r"\b[\w.+-]+@[\w-]+(?:\.[\w-]+)+\b", 0, 1.0),
        r("ip address", r"\b(?:\d{1,3}\.){3}\d{1,3}\b", 0, 1.0),
        r(
            "file path",
            r#"(?:^|[\s"'`(=])((?:\.{1,2}/|/)?(?:[\w.\-]+/)+[\w.\-]*[\w\-]|/[\w.\-]+|[\w\-]+\.(?:txt|md|json|toml|ya?ml|rs|py|js|ts|csv|log|conf|cfg|ini|sh|sql|html|xml|lock|env))"#,
            1,
            1.0,
        ),
        r("version", r"\b(?:v\d+(?:\.\d+)*|\d+\.\d+(?:\.\d+)+)(?:-[\w.]+)?\b", 0, 0.95),
        // where the user is, said in passing ("I'm Sam from New York.") or asked about ("weather in Paris")
        r(
            "location",
            r"(?i)\b(?:i(?:'m|\s+am)\b[^.!?\n]{0,40}?\bfrom|i\s+(?:live|am\s+living|am\s+based|'m\s+based)\s+in|(?:living|based|located)\s+in)\s+([A-Za-z][A-Za-z .'\-]{1,40}?)(?:\s*(?:[.,!?;\n]|$)|\s+(?:and|but|so|tell|please|what|how|can|could|now|today)\b)",
            1,
            0.8,
        ),
        r(
            "location",
            r"(?i)\b(?:weather|temperature|forecast|climate)\b[^.!?\n]*?\b(?:in|for|at|of|near)\s+((?:[A-Za-z][A-Za-z.'\-]*)(?:\s+[A-Za-z][A-Za-z.'\-]*){0,3}?)(?:\s+(?:today|tomorrow|now|tonight|please|right)\b|\s*[.,!?;\n]|\s*$)",
            1,
            0.75,
        ),
        r("quoted", r#""([^"\n]{1,200})""#, 1, 0.9),
        r("quoted", r"'([^'\n]{2,200})'", 1, 0.8),
        r("quoted", r"`([^`\n]{1,200})`", 1, 0.9),
        r("expression", r"(?:\d+(?:\.\d+)?|\([^()\n]*\))(?:\s*[-+*/^%]\s*(?:\d+(?:\.\d+)?|\([^()\n]*\)))+", 0, 0.95),
        r("environment", r"(?i)\b(production|prod|staging|stage|development|dev|qa|testing)\b", 1, 0.8),
        r(
            "deadline",
            r"(?i)\b(?:by |until |before |on |next )?(monday|tuesday|wednesday|thursday|friday|saturday|sunday|tomorrow|tonight|today|end of day|eod|next week|\d{4}-\d{2}-\d{2})\b",
            1,
            0.8,
        ),
        r("constraint", r"(?i)\b(?:do not|don't|dont|never|must not|without|avoid)\s+[^.,;\n]{2,60}", 0, 0.75),
        r(
            "software",
            r"(?i)\b(redis|postgres(?:ql)?|mysql|mariadb|mongodb|sqlite|nginx|apache|docker|kubernetes|k8s|kafka|rabbitmq|elasticsearch|python|rust|node(?:\.js)?|java|golang|git|linux|ubuntu|debian|openssl|openssh|curl|bash)\b",
            1,
            0.7,
        ),
        r(
            "action",
            r"(?i)\b(deploy|restart|install|uninstall|delete|remove|read|list|search|find|fetch|download|write|save|create|update|upgrade|patch|check|inspect|calculate|compute|summari[sz]e|remember|recall|run|execute|verify)\b",
            1,
            0.6,
        ),
        r("location", r"\b(Frankfurt|London|Paris|Berlin|Tokyo|Singapore|Sydney|Dublin|Amsterdam|Virginia|Oregon|Mumbai|Toronto|Madrid|Stockholm)\b", 1, 0.7),
    ]
});

/// Software names followed by a number: "Redis 8" -> software + version.
static SOFT_VER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(redis|postgres(?:ql)?|mysql|nginx|python|node(?:\.js)?|java|rust|docker|kubernetes|ubuntu|debian)\s+v?(\d+(?:\.\d+)*)\b").unwrap()
});

fn char_offset(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

#[derive(Default)]
pub struct RuleExtractor;

impl RuleExtractor {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&self, text: &str) -> Vec<Entity> {
        let mut out: Vec<Entity> = Vec::new();
        for rule in RULES.iter() {
            for caps in rule.re.captures_iter(text) {
                let Some(m) = caps.get(rule.group).or_else(|| caps.get(0)) else { continue };
                let label = rule.label;
                let mut t = m.as_str().to_string();
                // an expression must contain an operator surrounded by digits and be more than a date
                if label == "expression" && (t.contains('-') && t.matches('-').count() == 2 && !t.contains(' ')) {
                    continue; // looks like a date 2025-01-31
                }
                if label == "file path" && (t.starts_with("http") || t.contains("://")) {
                    continue;
                }
                if label == "quoted" {
                    t = t.trim().to_string();
                }
                if label == "version" && out.iter().any(|e| e.label == "ip address" && e.text.contains(&t)) {
                    continue;
                }
                out.push(Entity {
                    label: label.to_string(),
                    text: t,
                    start: Some(char_offset(text, m.start())),
                    end: Some(char_offset(text, m.end())),
                    score: rule.score,
                    source: "rules".into(),
                });
            }
        }
        for caps in SOFT_VER.captures_iter(text) {
            if let Some(v) = caps.get(2) {
                out.push(Entity {
                    label: "version".into(),
                    text: v.as_str().into(),
                    start: Some(char_offset(text, v.start())),
                    end: Some(char_offset(text, v.end())),
                    score: 0.9,
                    source: "rules".into(),
                });
            }
        }
        out
    }
}

#[async_trait]
impl EntityExtractor for RuleExtractor {
    fn name(&self) -> &str {
        "rules"
    }
    async fn extract(&self, request: ExtractionRequest) -> Result<ExtractionResult, ExtractionError> {
        let started = Instant::now();
        let entities = crate::normalize::normalize_entities(self.run(&request.text));
        Ok(ExtractionResult { entities, backend: "rules".into(), latency_ms: started.elapsed().as_millis() as u64, cached: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(es: &[Entity]) -> Vec<(String, String)> {
        es.iter().map(|e| (e.label.clone(), e.text.clone())).collect()
    }

    #[test]
    fn extracts_the_blueprint_example() {
        let es = RuleExtractor::new().run("Deploy Redis 8 on the production server in Frankfurt, but don't restart it until Friday.");
        let l = labels(&es);
        assert!(l.contains(&("software".into(), "Redis".into())));
        assert!(l.contains(&("version".into(), "8".into())));
        assert!(l.contains(&("environment".into(), "production".into())));
        assert!(l.contains(&("location".into(), "Frankfurt".into())));
        assert!(l.contains(&("deadline".into(), "Friday".into())));
        assert!(l.iter().any(|(k, v)| k == "constraint" && v.starts_with("don't restart")));
        assert!(l.contains(&("action".into(), "Deploy".into())));
    }

    #[test]
    fn extracts_paths_urls_and_expressions() {
        let es = RuleExtractor::new().run("read /workspace/notes/a.txt then fetch https://example.com/docs and compute (3 + 4) * 12");
        let l = labels(&es);
        assert!(l.contains(&("file path".into(), "/workspace/notes/a.txt".into())), "{l:?}");
        assert!(l.contains(&("url".into(), "https://example.com/docs".into())));
        assert!(l.iter().any(|(k, _)| k == "expression"), "{l:?}");
        assert!(!l.iter().any(|(k, v)| k == "file path" && v.contains("example.com")));
    }

    #[test]
    fn dates_are_not_expressions() {
        let es = RuleExtractor::new().run("deadline 2025-01-31");
        assert!(!es.iter().any(|e| e.label == "expression"));
        assert!(es.iter().any(|e| e.label == "deadline"));
    }
}
