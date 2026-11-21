//! Production Rate Limiter - Sharded token-bucket rate limiting per caller actor and token.
//! Sharding across 16 independent locks eliminates lock contention under high multi-agent concurrency.

use crate::logging::now_epoch_secs;
use crate::models::TenantQuotaPolicy;
use std::collections::{HashMap, VecDeque};
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
            // Memory bound guard: if shard capacity reached, prune expired entries (>300s)
            // or evict the least recently refreshed entry (LRU eviction).
            const MAX_SHARD_ENTRIES: usize = 1024;
            const BUCKET_TTL_SECS: f64 = 300.0;

            if guard.len() >= MAX_SHARD_ENTRIES {
                guard.retain(|_, b| (now - b.last_refreshed) < BUCKET_TTL_SECS);
                if guard.len() >= MAX_SHARD_ENTRIES {
                    if let Some(oldest_key) = guard
                        .iter()
                        .min_by(|a, b| {
                            a.1.last_refreshed
                                .partial_cmp(&b.1.last_refreshed)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(k, _)| k.clone())
                    {
                        guard.remove(&oldest_key);
                    }
                }
            }

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

/// Sliding-window 1-hour usage record for per-tenant / per-principal quotas (v6.5.0).
#[derive(Default)]
struct TenantUsageWindow {
    tool_calls: VecDeque<(f64, u64)>,
    codemode_fuel: VecDeque<(f64, u64)>,
    egress_bytes: VecDeque<(f64, u64)>,
}

impl TenantUsageWindow {
    fn prune_older_than(queue: &mut VecDeque<(f64, u64)>, cutoff: f64) {
        while let Some(&(ts, _)) = queue.front() {
            if ts < cutoff {
                queue.pop_front();
            } else {
                break;
            }
        }
    }

    fn sum_and_retry_after(queue: &VecDeque<(f64, u64)>, now: f64, window_sec: f64) -> (u64, u64) {
        let total: u64 = queue.iter().map(|(_, amt)| *amt).sum();
        let retry_after = queue
            .front()
            .map(|(oldest, _)| ((oldest + window_sec - now).ceil().max(1.0)) as u64)
            .unwrap_or(60);
        (total, retry_after)
    }
}

/// Per-tenant / per-principal sliding-window quota enforcer (`max_tool_calls_per_hour`,
/// `max_codemode_fuel_per_hour`, `max_egress_bytes_per_hour`) returning `-32004 RATE_LIMITED`
/// with `Retry-After` seconds on breach (v6.5.0).
#[derive(Clone)]
pub struct TenantQuotaTracker {
    window_sec: f64,
    default_policy: TenantQuotaPolicy,
    tenants: Arc<Mutex<HashMap<String, TenantUsageWindow>>>,
}

impl Default for TenantQuotaTracker {
    fn default() -> Self {
        Self::new(TenantQuotaPolicy::default())
    }
}

impl TenantQuotaTracker {
    pub fn new(default_policy: TenantQuotaPolicy) -> Self {
        Self {
            window_sec: 3600.0,
            default_policy,
            tenants: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_window_sec(mut self, window_sec: f64) -> Self {
        self.window_sec = window_sec.max(1.0);
        self
    }

    /// Checks and records `tool_calls`, `fuel`, and `egress_bytes` against `policy` (or default).
    /// Returns `Ok(())` if within quota, or `Err((retry_after_sec, diagnostic_message))`.
    pub fn check_and_consume(
        &self,
        tenant_id: &str,
        tool_calls: u64,
        fuel: u64,
        egress_bytes: u64,
        policy_override: Option<&TenantQuotaPolicy>,
    ) -> Result<(), (u64, String)> {
        let policy = policy_override.unwrap_or(&self.default_policy);
        let now = now_epoch_secs();
        let cutoff = now - self.window_sec;

        let mut guard = self.tenants.lock().unwrap();
        let entry = guard.entry(tenant_id.to_string()).or_default();

        TenantUsageWindow::prune_older_than(&mut entry.tool_calls, cutoff);
        TenantUsageWindow::prune_older_than(&mut entry.codemode_fuel, cutoff);
        TenantUsageWindow::prune_older_than(&mut entry.egress_bytes, cutoff);

        let (used_calls, calls_retry) =
            TenantUsageWindow::sum_and_retry_after(&entry.tool_calls, now, self.window_sec);
        if tool_calls > 0
            && policy.max_tool_calls_per_hour > 0
            && used_calls.saturating_add(tool_calls) > policy.max_tool_calls_per_hour
        {
            return Err((
                calls_retry,
                format!(
                    "Tenant '{}' exceeded hourly tool call quota ({}/{} calls/hr); Retry-After: {}s",
                    tenant_id, used_calls, policy.max_tool_calls_per_hour, calls_retry
                ),
            ));
        }

        let (used_fuel, fuel_retry) =
            TenantUsageWindow::sum_and_retry_after(&entry.codemode_fuel, now, self.window_sec);
        if fuel > 0
            && policy.max_codemode_fuel_per_hour > 0
            && used_fuel.saturating_add(fuel) > policy.max_codemode_fuel_per_hour
        {
            return Err((
                fuel_retry,
                format!(
                    "Tenant '{}' exceeded hourly Code Mode fuel quota ({}/{} ops/hr); Retry-After: {}s",
                    tenant_id, used_fuel, policy.max_codemode_fuel_per_hour, fuel_retry
                ),
            ));
        }

        let (used_egress, egress_retry) =
            TenantUsageWindow::sum_and_retry_after(&entry.egress_bytes, now, self.window_sec);
        if egress_bytes > 0
            && policy.max_egress_bytes_per_hour > 0
            && used_egress.saturating_add(egress_bytes) > policy.max_egress_bytes_per_hour
        {
            return Err((
                egress_retry,
                format!(
                    "Tenant '{}' exceeded hourly egress quota ({}/{} bytes/hr); Retry-After: {}s",
                    tenant_id, used_egress, policy.max_egress_bytes_per_hour, egress_retry
                ),
            ));
        }

        if tool_calls > 0 {
            entry.tool_calls.push_back((now, tool_calls));
        }
        if fuel > 0 {
            entry.codemode_fuel.push_back((now, fuel));
        }
        if egress_bytes > 0 {
            entry.egress_bytes.push_back((now, egress_bytes));
        }
        Ok(())
    }

    pub fn usage_snapshot(&self, tenant_id: &str) -> (u64, u64, u64) {
        let now = now_epoch_secs();
        let cutoff = now - self.window_sec;
        let mut guard = self.tenants.lock().unwrap();
        let Some(entry) = guard.get_mut(tenant_id) else {
            return (0, 0, 0);
        };
        TenantUsageWindow::prune_older_than(&mut entry.tool_calls, cutoff);
        TenantUsageWindow::prune_older_than(&mut entry.codemode_fuel, cutoff);
        TenantUsageWindow::prune_older_than(&mut entry.egress_bytes, cutoff);
        (
            entry.tool_calls.iter().map(|(_, a)| *a).sum(),
            entry.codemode_fuel.iter().map(|(_, a)| *a).sum(),
            entry.egress_bytes.iter().map(|(_, a)| *a).sum(),
        )
    }

    pub fn reset(&self) {
        self.tenants.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_bounded_memory_lru_pruning() {
        let limiter = RateLimiter::new(60);
        for i in 0..2000 {
            let key = format!("agent_worker_key_{}:agent", i);
            let (allowed, _) = limiter.check_rate_limit(&key, 10);
            assert!(allowed);
        }

        for shard in limiter.shards.iter() {
            let guard = shard.lock().unwrap();
            assert!(
                guard.len() <= 1024,
                "Shard entry count {} exceeded MAX_SHARD_ENTRIES 1024",
                guard.len()
            );
        }
    }
}
