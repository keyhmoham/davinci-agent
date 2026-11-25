//! Lightweight lexical utilities: tokenisation, stemming and a BM25-style
//! ranker used by the local decision engine and the tool selector.

use std::collections::{HashMap, HashSet};

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "then", "else", "of", "to", "in", "on", "at", "for", "from", "by", "with", "about", "as", "is", "are", "was",
    "were", "be", "been", "being", "it", "its", "this", "that", "these", "those", "i", "me", "my", "we", "our", "you", "your", "he", "she", "they", "them",
    "do", "does", "did", "can", "could", "would", "should", "will", "shall", "may", "might", "please", "so", "than", "too", "very", "just", "also", "into",
    "out", "up", "down", "over", "any", "all", "some", "what", "which", "who", "whom", "how", "when", "where", "why", "there", "here", "need", "want", "like",
];

pub fn is_stopword(w: &str) -> bool {
    STOPWORDS.contains(&w)
}

/// Lowercased alphanumeric tokens (underscores kept inside words).
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            for l in ch.to_lowercase() {
                cur.push(l);
            }
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Very light suffix stemmer — enough to match "files"/"file", "listing"/"list".
pub fn stem(w: &str) -> String {
    let n = w.len();
    if w.chars().any(|c| c.is_ascii_digit()) || n <= 3 {
        return w.to_string();
    }
    if let Some(p) = w.strip_suffix("ies") {
        return format!("{p}y");
    }
    if n > 5
        && let Some(p) = w.strip_suffix("ing")
    {
        return p.to_string();
    }
    if n > 4 {
        if let Some(p) = w.strip_suffix("ed") {
            return p.to_string();
        }
        if let Some(p) = w.strip_suffix("es")
            && (p.ends_with(['s', 'x', 'z']) || p.ends_with("ch") || p.ends_with("sh"))
        {
            return p.to_string();
        }
    }
    if let Some(p) = w.strip_suffix('s')
        && !p.ends_with('s')
    {
        return p.to_string();
    }
    w.to_string()
}

/// Tokens with stopwords removed and stems applied.
pub fn content_tokens(s: &str) -> Vec<String> {
    tokenize(s).into_iter().filter(|t| !is_stopword(t)).map(|t| stem(&t)).collect()
}

/// BM25-style scores of `query` against each document, normalised to ~[0, 1].
pub fn bm25_scores(query: &str, docs: &[String]) -> Vec<f32> {
    let q: Vec<String> = {
        let mut seen = HashSet::new();
        content_tokens(query).into_iter().filter(|t| seen.insert(t.clone())).collect()
    };
    let doc_tokens: Vec<Vec<String>> = docs.iter().map(|d| content_tokens(d)).collect();
    let n = docs.len().max(1) as f32;
    let avgdl = (doc_tokens.iter().map(|d| d.len()).sum::<usize>() as f32 / n).max(1.0);
    let mut df: HashMap<&str, usize> = HashMap::new();
    for d in &doc_tokens {
        let uniq: HashSet<&str> = d.iter().map(|s| s.as_str()).collect();
        for t in uniq {
            *df.entry(t).or_default() += 1;
        }
    }
    let (k1, b) = (1.2f32, 0.75f32);
    let idf = |t: &str| -> f32 {
        let d = *df.get(t).unwrap_or(&0) as f32;
        (((n - d + 0.5) / (d + 0.5)) + 1.0).ln()
    };
    let max_possible: f32 = q.iter().map(|t| idf(t) * (k1 + 1.0)).sum::<f32>().max(1e-6);
    doc_tokens
        .iter()
        .map(|d| {
            let dl = d.len() as f32;
            let mut tf: HashMap<&str, f32> = HashMap::new();
            for t in d {
                *tf.entry(t.as_str()).or_default() += 1.0;
            }
            let s: f32 = q
                .iter()
                .map(|t| {
                    let f = *tf.get(t.as_str()).unwrap_or(&0.0);
                    if f == 0.0 { 0.0 } else { idf(t) * (f * (k1 + 1.0)) / (f + k1 * (1.0 - b + b * dl / avgdl)) }
                })
                .sum();
            (s / max_possible).clamp(0.0, 1.0)
        })
        .collect()
}

/// Jaccard similarity of two token sets.
pub fn jaccard(a: &[String], b: &[String]) -> f32 {
    let sa: HashSet<&String> = a.iter().collect();
    let sb: HashSet<&String> = b.iter().collect();
    if sa.is_empty() && sb.is_empty() {
        return 0.0;
    }
    sa.intersection(&sb).count() as f32 / sa.union(&sb).count() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_basic() {
        assert_eq!(stem("files"), "file");
        assert_eq!(stem("listing"), "list");
        assert_eq!(stem("queries"), "query");
        assert_eq!(stem("class"), "class");
    }

    #[test]
    fn bm25_prefers_relevant_doc() {
        let docs = vec!["list directory entries files folder".to_string(), "evaluate arithmetic expression calculate".to_string()];
        let s = bm25_scores("show me the files in this folder", &docs);
        assert!(s[0] > s[1]);
        let s = bm25_scores("calculate 3 + 4", &docs);
        assert!(s[1] > s[0]);
    }
}
