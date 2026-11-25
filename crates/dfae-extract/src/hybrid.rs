use crate::normalize::merge_entities;
use crate::rules::RuleExtractor;
use async_trait::async_trait;
use dfae_core::*;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub struct HybridExtractor {
    semantic: Option<Arc<dyn EntityExtractor>>,
    rules: RuleExtractor,
    cache: Mutex<HashMap<String, ExtractionResult>>,
    warned: AtomicBool,
}

const CACHE_CAP: usize = 256;

impl HybridExtractor {
    pub fn new(semantic: Option<Arc<dyn EntityExtractor>>) -> Self {
        Self { semantic, rules: RuleExtractor::new(), cache: Mutex::new(HashMap::new()), warned: AtomicBool::new(false) }
    }

    pub fn has_semantic(&self) -> bool {
        self.semantic.is_some()
    }
}

#[async_trait]
impl EntityExtractor for HybridExtractor {
    fn name(&self) -> &str {
        "hybrid"
    }

    async fn extract(&self, request: ExtractionRequest) -> Result<ExtractionResult, ExtractionError> {
        let key = sha256_hex(&format!("{}|{}|{}", request.text, request.labels.join(","), request.threshold));
        if let Some(mut c) = self.cache.lock().get(&key).cloned() {
            c.cached = true;
            c.latency_ms = 0;
            return Ok(c);
        }
        let started = Instant::now();
        let rule_res = self.rules.extract(request.clone()).await?;
        let (semantic, backend) = match &self.semantic {
            Some(g) => match g.extract(request.clone()).await {
                Ok(r) => {
                    if self.warned.swap(false, Ordering::Relaxed) {
                        tracing::info!("semantic extractor is available again");
                    }
                    let b = format!("{}+rules", r.backend);
                    (r.entities, b)
                }
                Err(e) => {
                    if !self.warned.swap(true, Ordering::Relaxed) {
                        tracing::warn!(error = %e, "semantic extractor unavailable (a GLiNER service that has just started is still loading its model: `docker compose logs gliner`); using rules only");
                    }
                    (vec![], "rules(fallback)".to_string())
                }
            },
            None => (vec![], "rules".to_string()),
        };
        let entities = merge_entities(semantic, rule_res.entities);
        let result = ExtractionResult { entities, backend, latency_ms: started.elapsed().as_millis() as u64, cached: false };
        let mut c = self.cache.lock();
        if c.len() >= CACHE_CAP {
            c.clear();
        }
        c.insert(key, result.clone());
        Ok(result)
    }

    async fn health(&self) -> bool {
        match &self.semantic {
            Some(g) => g.health().await,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Down;
    #[async_trait]
    impl EntityExtractor for Down {
        fn name(&self) -> &str {
            "down"
        }
        async fn extract(&self, _r: ExtractionRequest) -> Result<ExtractionResult, ExtractionError> {
            Err(ExtractionError::Unavailable("nope".into()))
        }
    }

    fn req(t: &str) -> ExtractionRequest {
        ExtractionRequest { text: t.into(), labels: dfae_core::DEFAULT_LABELS.iter().map(|s| s.to_string()).collect(), threshold: 0.4 }
    }

    #[tokio::test]
    async fn degrades_to_rules_and_caches() {
        let h = HybridExtractor::new(Some(Arc::new(Down)));
        let a = h.extract(req("fetch https://example.com/x and read /workspace/a.txt")).await.unwrap();
        assert_eq!(a.backend, "rules(fallback)");
        assert!(a.entities.iter().any(|e| e.label == "url"));
        assert!(a.entities.iter().any(|e| e.label == "file path" && e.text == "/workspace/a.txt"));
        let b = h.extract(req("fetch https://example.com/x and read /workspace/a.txt")).await.unwrap();
        assert!(b.cached);
    }
}
