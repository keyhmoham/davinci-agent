use dfae_core::Entity;

/// Lowercase labels, trim surrounding punctuation, drop empties/duplicates.
pub fn normalize_entities(mut es: Vec<Entity>) -> Vec<Entity> {
    for e in &mut es {
        e.label = e.label.trim().to_lowercase();
        e.text = e.text.trim().trim_matches(|c: char| ",.;:!?\"'`()[]".contains(c)).to_string();
    }
    es.retain(|e| !e.text.is_empty() && !e.label.is_empty());
    let mut out: Vec<Entity> = Vec::new();
    for e in es {
        if let Some(existing) = out.iter_mut().find(|x| x.label == e.label && x.text.eq_ignore_ascii_case(&e.text)) {
            if e.score > existing.score {
                *existing = e;
            }
        } else {
            out.push(e);
        }
    }
    out
}

fn overlaps(a: &Entity, b: &Entity) -> bool {
    // a constraint ("don't restart it until Friday") legitimately wraps other
    // entities; it must never be displaced by what it contains
    if a.label == "constraint" || b.label == "constraint" {
        return false;
    }
    match (a.start, a.end, b.start, b.end) {
        (Some(a0), Some(a1), Some(b0), Some(b1)) => a0 < b1 && b0 < a1,
        _ => false,
    }
}

/// Labels whose spans are exactly recognisable by rules; the rule extractor wins.
pub fn is_structured(label: &str) -> bool {
    matches!(label, "url" | "email" | "file path" | "ip address" | "version" | "expression" | "quoted")
}

/// Merge semantic (GLiNER) and structured (rules) entities, resolving overlaps.
pub fn merge_entities(semantic: Vec<Entity>, rules: Vec<Entity>) -> Vec<Entity> {
    let mut out: Vec<Entity> = Vec::new();
    // structured rule entities first: they are exact
    for r in rules.iter().filter(|r| is_structured(&r.label)) {
        out.push(r.clone());
    }
    for s in semantic.into_iter().chain(rules.into_iter().filter(|r| !is_structured(&r.label))) {
        let blocked = out.iter().any(|o| overlaps(o, &s) && (is_structured(&o.label) || o.score >= s.score));
        if blocked {
            continue;
        }
        out.retain(|o| !(overlaps(o, &s) && o.score < s.score && !is_structured(&o.label)));
        out.push(s);
    }
    out.sort_by_key(|e| e.start.unwrap_or(usize::MAX));
    normalize_entities(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(label: &str, text: &str, s: usize, t: usize, score: f32) -> Entity {
        Entity { label: label.into(), text: text.into(), start: Some(s), end: Some(t), score, source: "t".into() }
    }

    #[test]
    fn structured_rules_beat_overlapping_semantic_spans() {
        let sem = vec![e("organization", "example.com/a", 0, 13, 0.9)];
        let rules = vec![e("url", "example.com/a", 0, 13, 1.0)];
        let m = merge_entities(sem, rules);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].label, "url");
    }

    #[test]
    fn higher_score_wins_between_semantic_overlaps() {
        let sem = vec![e("software", "Redis", 0, 5, 0.6), e("tool", "Redis", 0, 5, 0.8)];
        let m = merge_entities(sem, vec![]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].label, "tool");
    }

    #[test]
    fn constraints_may_contain_other_entities() {
        let rules = vec![e("constraint", "don't restart it until Friday", 10, 39, 0.75), e("deadline", "Friday", 33, 39, 0.8)];
        let m = merge_entities(vec![], rules);
        assert_eq!(m.len(), 2);
        assert!(m.iter().any(|x| x.label == "constraint"));
    }

    #[test]
    fn normalizes_and_dedupes() {
        let m = normalize_entities(vec![e("Software", " Redis, ", 0, 5, 0.5), e("software", "redis", 0, 5, 0.9)]);
        assert_eq!(m.len(), 1);
        assert!((m[0].score - 0.9).abs() < 1e-6);
    }
}
