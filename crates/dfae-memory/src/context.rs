//! Memory ranking and token-budgeted context assembly.

use crate::cosine;
use chrono::Utc;
use dfae_core::text::content_tokens;
use dfae_core::tokens::{estimate_tokens, truncate_tokens};
use dfae_core::*;
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct RankedMemory {
    pub item: MemoryItem,
    pub similarity: f32,
    pub score: f32,
    /// `similarity` is semantic (see `ScoredMemory::semantic`).
    pub semantic: bool,
}

impl RankedMemory {
    /// Related enough to the query to be shown at all. The two similarity scales differ: unrelated texts score about 0 with the hash
    /// embedding and about 0.2 with a semantic one (measured with voyage-4-lite: wrong matches averaged 0.22, right ones 0.78; a
    /// paraphrased lesson or a preference that bears on a request scored 0.41-0.62). At most six are shown, within the context budget.
    pub fn relevant(&self) -> bool {
        self.similarity >= if self.semantic { 0.35 } else { 0.2 }
    }
}

/// Rank retrieval candidates: semantic similarity dominates, with recency,
/// importance and entity overlap as tie-breakers. Entity overlap is a signal,
/// not a substitute for semantic relevance.
pub fn rank_memories(cands: Vec<ScoredMemory>, query_entities: &[Entity]) -> Vec<RankedMemory> {
    let now = Utc::now();
    let q: HashSet<String> = query_entities.iter().map(|e| e.text.to_lowercase()).collect();
    let mut out: Vec<RankedMemory> = cands
        .into_iter()
        .map(|c| {
            let age_days = (now - c.item.created_at).num_seconds().max(0) as f32 / 86400.0;
            let recency = 0.5f32.powf(age_days / 7.0);
            let overlap = if q.is_empty() {
                0.0
            } else {
                let theirs: HashSet<String> = c.item.entities.iter().map(|e| e.text.to_lowercase()).collect();
                q.intersection(&theirs).count() as f32 / q.len() as f32
            };
            let score = 0.55 * c.similarity.max(0.0) + 0.15 * recency + 0.15 * c.item.importance + 0.15 * overlap;
            RankedMemory { item: c.item, similarity: c.similarity, score, semantic: c.semantic }
        })
        .collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    out
}

/// Drop near-duplicate memories (identical text or very similar token sets).
pub fn dedupe_memories(ranked: Vec<RankedMemory>) -> Vec<RankedMemory> {
    let mut kept: Vec<(RankedMemory, Vec<String>)> = Vec::new();
    for r in ranked {
        let toks = content_tokens(&r.item.text);
        let dup = kept.iter().any(|(k, kt)| k.item.text == r.item.text || dfae_core::text::jaccard(kt, &toks) > 0.85);
        if !dup {
            kept.push((r, toks));
        }
    }
    kept.into_iter().map(|(r, _)| r).collect()
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: &'static str,
    pub text: String,
    pub tokens: usize,
    /// Lower = more important; priority 0 is never dropped.
    pub priority: u8,
}

#[derive(Debug, Clone, Default)]
pub struct ContextPackage {
    pub sections: Vec<Section>,
    pub total_tokens: usize,
    pub dropped: Vec<String>,
}

impl ContextPackage {
    pub fn render(&self) -> String {
        self.sections.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

pub struct ContextInput<'a> {
    pub goal: &'a str,
    pub entities: &'a [Entity],
    pub memories: &'a [RankedMemory],
    /// Summaries of earlier tasks in the same session (most recent last).
    pub prior_turns: &'a [String],
    /// `(label, compressed output)` of completed steps.
    pub observations: &'a [(String, String)],
    /// Results of the agents this one depends on `(label, summary)`.
    pub upstream: &'a [(String, String)],
    pub profile: ContextProfile,
}

fn render_entities(es: &[Entity]) -> String {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for e in es.iter().filter(|e| e.label != "constraint").take(16) {
        match groups.iter_mut().find(|(l, _)| *l == e.label) {
            Some((_, v)) => {
                if !v.contains(&e.text) {
                    v.push(e.text.clone());
                }
            }
            None => groups.push((e.label.clone(), vec![e.text.clone()])),
        }
    }
    groups.iter().map(|(l, v)| format!("{}={}", l.replace(' ', "_"), v.join(","))).collect::<Vec<_>>().join(" | ")
}

/// Assemble a context package that fits the profile's token budget, dropping
/// the least important material first.
pub fn build_context(input: &ContextInput) -> ContextPackage {
    let budget = input.profile.token_budget();
    let (mem_n, prior_n, obs_tokens) = match input.profile {
        // a simple request still gets what memory knows about it: the two most relevant memories (already filtered for relevance,
        // ~80 tokens each) and the last turn. With none, a lesson, a fact or a template never reached the planner of a request the
        // decision model scored simple (measured: a needle among 200 memories was retrieved, then dropped here)
        ContextProfile::Minimal => (2, 1, 120),
        ContextProfile::Standard => (3, 2, 500),
        ContextProfile::Reasoning => (6, 4, 1400),
    };
    let mut sections: Vec<Section> = Vec::new();
    let mut push = |name: &'static str, text: String, priority: u8| {
        if !text.trim().is_empty() {
            let tokens = estimate_tokens(&text);
            sections.push(Section { name, text, tokens, priority });
        }
    };
    push("goal", format!("GOAL: {}", truncate_tokens(input.goal, budget / 2)), 0);
    let constraints: Vec<&str> = input.entities.iter().filter(|e| e.label == "constraint").map(|e| e.text.as_str()).collect();
    if !constraints.is_empty() {
        push("constraints", format!("CONSTRAINTS: {}", constraints.join("; ")), 1);
    }
    let ent = render_entities(input.entities);
    if !ent.is_empty() {
        push("entities", format!("ENT: {ent}"), 1);
    }
    if !input.upstream.is_empty() {
        // essential: this is what the agent was handed to work from
        let mut lines = vec!["UPSTREAM:".to_string()];
        for (label, summary) in input.upstream {
            lines.push(format!(" {label}: {}", truncate_tokens(summary, 90)));
        }
        push("upstream", lines.join("\n"), 1);
    }
    if !input.observations.is_empty() {
        let mut lines = vec!["OBS:".to_string()];
        let per = (obs_tokens / input.observations.len().max(1)).max(40);
        for (label, text) in input.observations {
            lines.push(format!(" {label}: {}", truncate_tokens(text, per)));
        }
        push("observations", lines.join("\n"), 2);
    }
    if mem_n > 0 && !input.memories.is_empty() {
        let mut lines = vec!["MEM:".to_string()];
        for m in input.memories.iter().take(mem_n) {
            lines.push(format!(" - [{}] {}", m.item.kind.as_str(), truncate_tokens(&m.item.text, 80)));
        }
        push("memories", lines.join("\n"), 3);
    }
    if prior_n > 0 && !input.prior_turns.is_empty() {
        let start = input.prior_turns.len().saturating_sub(prior_n);
        let mut lines = vec!["PRIOR:".to_string()];
        for p in &input.prior_turns[start..] {
            lines.push(format!(" - {}", truncate_tokens(p, 60)));
        }
        push("prior", lines.join("\n"), 4);
    }
    // fit the budget: drop lowest-priority sections whole, then shrink
    let mut dropped = Vec::new();
    loop {
        let total: usize = sections.iter().map(|s| s.tokens).sum();
        if total <= budget {
            break;
        }
        let worst = sections.iter().enumerate().filter(|(_, s)| s.priority > 0).max_by_key(|(_, s)| s.priority).map(|(i, _)| i);
        match worst {
            Some(i) => {
                dropped.push(sections[i].name.to_string());
                sections.remove(i);
            }
            None => {
                // only the goal remains and it is still too big
                if let Some(g) = sections.first_mut() {
                    g.text = truncate_tokens(&g.text, budget);
                    g.tokens = estimate_tokens(&g.text);
                }
                break;
            }
        }
    }
    sections.sort_by_key(|s| match s.name {
        "goal" => 0,
        "constraints" => 1,
        "entities" => 2,
        "upstream" => 3,
        "prior" => 4,
        "memories" => 5,
        _ => 6,
    });
    let total_tokens = sections.iter().map(|s| s.tokens).sum();
    ContextPackage { sections, total_tokens, dropped }
}

/// Cosine-based helper used by the skill matcher.
pub fn best_similarity(a: &[f32], others: &[Vec<f32>]) -> f32 {
    others.iter().map(|o| cosine(a, o)).fold(0.0, f32::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(l: &str, t: &str) -> Entity {
        Entity { label: l.into(), text: t.into(), start: None, end: None, score: 1.0, source: "t".into() }
    }

    #[test]
    fn budget_is_respected_and_goal_survives() {
        let ents = vec![ent("software", "Redis"), ent("constraint", "do not restart until Friday")];
        let mems: Vec<RankedMemory> = (0..8)
            .map(|i| RankedMemory {
                item: MemoryItem::new(MemoryKind::Semantic, format!("memory number {i} {}", "filler ".repeat(40)), 0.5),
                similarity: 0.5,
                score: 0.5,
                semantic: false,
            })
            .collect();
        let obs = vec![("s1 fs_list".to_string(), "x ".repeat(2000))];
        let input = ContextInput {
            goal: "deploy redis",
            entities: &ents,
            memories: &mems,
            prior_turns: &[],
            observations: &obs,
            upstream: &[],
            profile: ContextProfile::Minimal,
        };
        let pkg = build_context(&input);
        assert!(pkg.total_tokens <= ContextProfile::Minimal.token_budget());
        let r = pkg.render();
        assert!(r.contains("GOAL: deploy redis"));
        assert!(r.contains("CONSTRAINTS: do not restart"));
        assert!(!r.contains("memory number"), "minimal profile carries no memories");

        let input = ContextInput { profile: ContextProfile::Reasoning, ..input };
        let pkg = build_context(&input);
        assert!(pkg.render().contains("memory number 0"));
        assert!(pkg.total_tokens <= ContextProfile::Reasoning.token_budget());
    }

    #[test]
    fn upstream_results_survive_even_the_minimal_budget() {
        let up = vec![("c1: read todo.md".to_string(), "- [ ] write docs".to_string())];
        let input = ContextInput {
            goal: "explain what is open",
            entities: &[],
            memories: &[],
            prior_turns: &[],
            observations: &[],
            upstream: &up,
            profile: ContextProfile::Minimal,
        };
        let r = build_context(&input).render();
        assert!(r.contains("UPSTREAM:") && r.contains("write docs"), "{r}");
        assert!(r.find("GOAL").unwrap() < r.find("UPSTREAM").unwrap());
    }

    #[test]
    fn ranking_prefers_similar_and_dedupes() {
        let mk = |t: &str, sim: f32| ScoredMemory::by_shape(MemoryItem::new(MemoryKind::Semantic, t, 0.5), sim);
        let ranked = rank_memories(vec![mk("alpha beta gamma", 0.2), mk("delta epsilon", 0.9), mk("delta epsilon", 0.89)], &[]);
        assert_eq!(ranked[0].item.text, "delta epsilon");
        let d = dedupe_memories(ranked);
        assert_eq!(d.len(), 2);
    }
}
