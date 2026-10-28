//! Production Rate Limiter - Sharded token-bucket rate limiting per caller actor and token.
//! Sharding across 16 independent locks eliminates lock contention under high multi-agent concurrency.

use crate::logging::now_epoch_secs;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SHARD_COUNT: usize = 16;

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last_refreshed: f64,
}

#[derive(Clone)]
pub struct RateLimiter {
    pub default_rpm: u32,
    shards: Arc<Vec<Mutex<HashMap<String, Bucket>>>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(1200)
    }
}

impl RateLimiter {
    pub fn new(default_rpm: u32) -> Self {
        let mut shards = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            shards.push(Mutex::new(HashMap::new()));
        }
        Self {
            default_rpm,
            shards: Arc::new(shards),
        }
    }

    fn shard_index(key: &str) -> usize {
        let mut h: usize = 5381;
        for b in key.as_bytes() {
            h = ((h << 5).wrapping_add(h)) ^ (*b as usize);
        }
        h % SHARD_COUNT
    }

    /// Returns `(is_allowed, remaining_tokens)`.
    pub fn check_rate_limit(&self, key: &str, max_rpm: u32) -> (bool, f64) {
        let rpm = if max_rpm > 0 {
            max_rpm
        } else {
            self.default_rpm
        };
        let capacity = rpm as f64;
        let fill_rate = capacity / 60.0;
        let now = now_epoch_secs();

        let idx = Self::shard_index(key);
        let mut guard = self.shards[idx].lock().unwrap();

        if let Some(bucket) = guard.get_mut(key) {
            let elapsed = (now - bucket.last_refreshed).max(0.0);
            bucket.tokens = (bucket.tokens + elapsed * fill_rate).min(capacity);
            bucket.last_refreshed = now;

            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                (true, bucket.tokens)
            } else {
                (false, 0.0)
            }
        } else {
            let rem = (capacity - 1.0).max(0.0);
            guard.insert(
                key.to_string(),
                Bucket {
                    tokens: rem,
                    last_refreshed: now,
                },
            );
            (capacity >= 1.0, rem)
        }
    }

    pub fn reset(&self) {
        for shard in self.shards.iter() {
            shard.lock().unwrap().clear();
        }
    }
}
