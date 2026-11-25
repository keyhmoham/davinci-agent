use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Beta-style calibration of decision confidence from verified outcomes.
///
/// `calibrated = (raw * W + successes) / (W + n)` — with no evidence the raw
/// confidence is used as-is; as outcomes accumulate, the observed success rate
/// of that decision takes over.
#[derive(Default)]
pub struct Calibrator {
    stats: Mutex<HashMap<String, (f64, f64)>>,
}

const PRIOR_WEIGHT: f64 = 6.0;

impl Calibrator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calibrate(&self, key: &str, raw: f64) -> f64 {
        let (s, f) = self.stats.lock().get(key).copied().unwrap_or((0.0, 0.0));
        let n = s + f;
        ((raw * PRIOR_WEIGHT + s) / (PRIOR_WEIGHT + n)).clamp(0.0, 1.0)
    }

    pub fn record(&self, key: &str, success: bool) {
        let mut g = self.stats.lock();
        let e = g.entry(key.to_string()).or_insert((0.0, 0.0));
        if success {
            e.0 += 1.0;
        } else {
            e.1 += 1.0;
        }
    }

    pub fn counts(&self, key: &str) -> (f64, f64) {
        self.stats.lock().get(key).copied().unwrap_or((0.0, 0.0))
    }

    pub fn snapshot(&self) -> Value {
        let g = self.stats.lock();
        let m: serde_json::Map<String, Value> = g.iter().map(|(k, (s, f))| (k.clone(), json!([s, f]))).collect();
        Value::Object(m)
    }

    pub fn load(&self, v: &Value) {
        if let Some(m) = v.as_object() {
            let mut g = self.stats.lock();
            for (k, val) in m {
                if let (Some(s), Some(f)) = (val.get(0).and_then(|x| x.as_f64()), val.get(1).and_then(|x| x.as_f64())) {
                    g.insert(k.clone(), (s, f));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_overrides_prior() {
        let c = Calibrator::new();
        assert!((c.calibrate("k", 0.9) - 0.9).abs() < 1e-9);
        for _ in 0..30 {
            c.record("k", false);
        }
        assert!(c.calibrate("k", 0.9) < 0.2, "repeated failures must lower trust");
        let c2 = Calibrator::new();
        c2.load(&c.snapshot());
        assert_eq!(c2.counts("k"), (0.0, 30.0));
    }
}
