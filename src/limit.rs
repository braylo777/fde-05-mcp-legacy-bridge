//! Rate limiting and retry planning for fragile legacy APIs.

use serde_json::{json, Value};

/// Token bucket in integer milli-tokens, driven by an explicit clock (ms).
#[derive(Debug, Clone)]
pub struct Bucket {
    pub capacity: u64,
    pub refill_per_sec: u64,
    milli: u64,
    last_ms: u64,
}

impl Bucket {
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Bucket {
            capacity,
            refill_per_sec,
            milli: capacity * 1000,
            last_ms: 0,
        }
    }
    fn refill(&mut self, now_ms: u64) {
        let dt = now_ms.saturating_sub(self.last_ms);
        self.milli = (self.milli + dt * self.refill_per_sec).min(self.capacity * 1000);
        self.last_ms = self.last_ms.max(now_ms);
    }
    /// Ok(()) if a token was taken, Err(wait_ms) until the next token.
    pub fn take(&mut self, now_ms: u64) -> Result<(), u64> {
        self.refill(now_ms);
        if self.milli >= 1000 {
            self.milli -= 1000;
            Ok(())
        } else if self.refill_per_sec == 0 {
            Err(u64::MAX)
        } else {
            Err((1000 - self.milli).div_ceil(self.refill_per_sec))
        }
    }
}

/// Deterministic xorshift for jitter, so plans are reproducible in tests and audits.
fn rng(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// Plan retries for a sequence of upstream responses (e.g. [503, 429, 200]).
/// Full-jitter exponential backoff; a Retry-After header (seconds) overrides it.
pub fn retry_plan_op(i: &Value) -> Result<Value, String> {
    let base = i["base_ms"].as_u64().unwrap_or(200);
    let cap = i["max_backoff_ms"].as_u64().unwrap_or(10_000);
    let max_attempts = i["max_attempts"].as_u64().unwrap_or(5) as usize;
    let budget = i["deadline_ms"].as_u64().unwrap_or(30_000);
    let mut seed = i["seed"].as_u64().unwrap_or(42).max(1);
    let responses = i["responses"]
        .as_array()
        .ok_or("`responses` must be a list of status codes or {status, retry_after_s}")?;
    let mut t = 0u64;
    let mut steps = Vec::new();
    for (n, r) in responses.iter().enumerate().take(max_attempts) {
        let status = r
            .as_u64()
            .or(r["status"].as_u64())
            .ok_or("each response needs a status")?;
        let retry_after = r["retry_after_s"].as_u64();
        let retryable = matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504);
        if (200..300).contains(&status) {
            steps
                .push(json!({"attempt": n + 1, "at_ms": t, "status": status, "action": "success"}));
            return Ok(json!({"outcome": "success", "total_ms": t, "steps": steps}));
        }
        if !retryable {
            steps.push(json!({"attempt": n + 1, "at_ms": t, "status": status, "action": "give up: not retryable"}));
            return Ok(json!({"outcome": "failed", "total_ms": t, "steps": steps}));
        }
        let exp = base.saturating_mul(1u64 << n.min(20)).min(cap);
        let wait = match retry_after {
            Some(s) => s * 1000,
            None => rng(&mut seed) % (exp + 1),
        };
        if n + 1 >= max_attempts || t + wait > budget {
            steps.push(json!({"attempt": n + 1, "at_ms": t, "status": status, "action": "give up: attempts or deadline exhausted"}));
            return Ok(json!({"outcome": "failed", "total_ms": t, "steps": steps}));
        }
        steps.push(json!({"attempt": n + 1, "at_ms": t, "status": status,
            "action": format!("retry in {wait} ms ({})", if retry_after.is_some() { "Retry-After" } else { "full jitter" })}));
        t += wait;
    }
    Ok(json!({"outcome": "failed", "total_ms": t, "steps": steps, "note": "ran out of responses"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bucket_burst_then_refill() {
        let mut b = Bucket::new(2, 1);
        assert!(b.take(0).is_ok());
        assert!(b.take(0).is_ok());
        assert_eq!(b.take(0), Err(1000));
        assert_eq!(b.take(400), Err(600));
        assert!(b.take(1000).is_ok());
    }
    #[test]
    fn retry_after_respected() {
        let r = retry_plan_op(&json!({"responses": [{"status": 429, "retry_after_s": 2}, 200]}))
            .unwrap();
        assert_eq!(r["outcome"], "success");
        assert_eq!(r["total_ms"], 2000);
    }
    #[test]
    fn non_retryable_stops() {
        let r = retry_plan_op(&json!({"responses": [400, 200]})).unwrap();
        assert_eq!(r["outcome"], "failed");
    }
    #[test]
    fn jitter_within_cap_and_deterministic() {
        let a = retry_plan_op(&json!({"responses": [503, 503, 503, 200], "seed": 7})).unwrap();
        let b = retry_plan_op(&json!({"responses": [503, 503, 503, 200], "seed": 7})).unwrap();
        assert_eq!(a, b);
        assert!(a["total_ms"].as_u64().unwrap() <= 200 + 400 + 800);
    }
    #[test]
    fn deadline_enforced() {
        let r = retry_plan_op(
            &json!({"responses": [{"status": 503, "retry_after_s": 60}, 200], "deadline_ms": 5000}),
        )
        .unwrap();
        assert_eq!(r["outcome"], "failed");
    }
}
