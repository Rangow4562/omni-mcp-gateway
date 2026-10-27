use crate::data_plane::codemode::{
    CodeModeError, CodeModeExecutor, CodeOutput, HostCallbacks, SandboxLimits,
};
use crate::json::JsonValue;
use crate::models::{CompletedMutationRecord, SagaCompensationRecord};
use crate::storage::audit_log::sha256_hex;
use std::sync::Mutex;

/// Pure-Rust 64-bit `xoshiro256**` deterministic pseudo-random number generator (PRNG).
/// Inspired by SlateDB / FoundationDB deterministic simulation testing (`dst`).
#[derive(Clone, Debug)]
pub struct Xoshiro256StarStar {
    s: [u64; 4],
}

impl Xoshiro256StarStar {
    /// Initializes a 256-bit state from a single 64-bit seed using `SplitMix64`.
    pub fn seed_from_u64(seed: u64) -> Self {
        let mut sm = seed;
        let mut next_sm = || {
            sm = sm.wrapping_add(0x9e3779b97f4a7c15);
            let mut z = sm;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
            z ^ (z >> 31)
        };
        Self {
            s: [next_sm(), next_sm(), next_sm(), next_sm()],
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let result = (self.s[1].wrapping_mul(5)).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;

        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];

        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);

        result
    }

    /// Uniform float in `[0.0, 1.0)`.
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / ((1u64 << 53) as f64))
    }

    /// Returns `true` with probability `p` (`0.0 <= p <= 1.0`).
    #[inline]
    pub fn gen_bool(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        if p >= 1.0 {
            return true;
        }
        self.next_f64() < p
    }
}

/// Fault injection parameters for Deterministic Simulation Testing (`dst`).
#[derive(Clone, Debug)]
pub struct FaultInjectionConfig {
    pub base_seed: u64,
    pub num_runs: usize,
    pub network_drop_prob: f64,
    pub circuit_open_prob: f64,
    pub latency_spike_prob: f64,
    pub malformed_payload_prob: f64,
}

impl Default for FaultInjectionConfig {
    fn default() -> Self {
        Self {
            base_seed: 42,
            num_runs: 25,
            network_drop_prob: 0.10,
            circuit_open_prob: 0.08,
            latency_spike_prob: 0.10,
            malformed_payload_prob: 0.05,
        }
    }
}

impl FaultInjectionConfig {
    pub fn from_json(v: &JsonValue) -> Self {
        let mut cfg = Self::default();
        if let Some(s) = v
            .get("seed")
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("base_seed").and_then(|x| x.as_u64()))
        {
            cfg.base_seed = s;
        }
        if let Some(n) = v
            .get("runs")
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("num_runs").and_then(|x| x.as_u64()))
        {
            cfg.num_runs = (n as usize).clamp(1, 1000);
        }
        if let Some(f) = v.get("faults").or(Some(v)) {
            if let Some(p) = f
                .get("network_drop_prob")
                .and_then(|x| x.as_f64())
                .or_else(|| f.get("network_drop").and_then(|x| x.as_f64()))
            {
                cfg.network_drop_prob = p.clamp(0.0, 1.0);
            }
            if let Some(p) = f
                .get("circuit_open_prob")
                .and_then(|x| x.as_f64())
                .or_else(|| f.get("circuit_open").and_then(|x| x.as_f64()))
            {
                cfg.circuit_open_prob = p.clamp(0.0, 1.0);
            }
            if let Some(p) = f
                .get("latency_spike_prob")
                .and_then(|x| x.as_f64())
                .or_else(|| f.get("latency_spike").and_then(|x| x.as_f64()))
            {
                cfg.latency_spike_prob = p.clamp(0.0, 1.0);
            }
            if let Some(p) = f
                .get("malformed_payload_prob")
                .and_then(|x| x.as_f64())
                .or_else(|| f.get("malformed_payload").and_then(|x| x.as_f64()))
            {
                cfg.malformed_payload_prob = p.clamp(0.0, 1.0);
            }
        }
        cfg
    }

    pub fn to_json(&self) -> JsonValue {
        let mut m = JsonValue::object();
        m.insert("base_seed".to_string(), JsonValue::from(self.base_seed as i64));
        m.insert("num_runs".to_string(), JsonValue::from(self.num_runs as i64));
        m.insert(
            "network_drop_prob".to_string(),
            JsonValue::from(self.network_drop_prob),
        );
        m.insert(
            "circuit_open_prob".to_string(),
            JsonValue::from(self.circuit_open_prob),
        );
        m.insert(
            "latency_spike_prob".to_string(),
            JsonValue::from(self.latency_spike_prob),
        );
        m.insert(
            "malformed_payload_prob".to_string(),
            JsonValue::from(self.malformed_payload_prob),
        );
        m
    }
}

/// Outcome of a single seeded simulation run.
#[derive(Clone, Debug)]
pub struct SeedRunOutcome {
    pub seed: u64,
    pub status: String,
    pub injected_faults: Vec<String>,
    pub saga_compensations_triggered: usize,
    pub tool_calls: usize,
    pub ops_consumed: u64,
    pub error: Option<String>,
}

impl SeedRunOutcome {
    pub fn to_json(&self) -> JsonValue {
        let mut m = JsonValue::object();
        m.insert("seed".to_string(), JsonValue::from(self.seed as i64));
        m.insert("status".to_string(), JsonValue::from(self.status.as_str()));
        m.insert(
            "injected_faults".to_string(),
            JsonValue::Array(
                self.injected_faults
                    .iter()
                    .map(|f| JsonValue::from(f.as_str()))
                    .collect(),
            ),
        );
        m.insert(
            "saga_compensations_triggered".to_string(),
            JsonValue::from(self.saga_compensations_triggered as i64),
        );
        m.insert(
            "tool_calls".to_string(),
            JsonValue::from(self.tool_calls as i64),
        );
        m.insert(
            "ops_consumed".to_string(),
            JsonValue::from(self.ops_consumed as i64),
        );
        if let Some(ref err) = self.error {
            m.insert("error".to_string(), JsonValue::from(err.as_str()));
        } else {
            m.insert("error".to_string(), JsonValue::Null);
        }
        m
    }
}

/// Aggregated report across all deterministic simulation runs.
#[derive(Clone, Debug)]
pub struct SimulationReport {
    pub config: FaultInjectionConfig,
    pub total_runs: usize,
    pub succeeded_runs: usize,
    pub compensated_rollback_runs: usize,
    pub approval_suspended_runs: usize,
    pub unhandled_failure_runs: usize,
    pub resilience_score: f64,
    pub deterministic_fingerprint: String,
    pub seed_outcomes: Vec<SeedRunOutcome>,
}

impl SimulationReport {
    pub fn to_json(&self) -> JsonValue {
        let mut m = JsonValue::object();
        m.insert("config".to_string(), self.config.to_json());
        m.insert(
            "total_runs".to_string(),
            JsonValue::from(self.total_runs as i64),
        );
        m.insert(
            "succeeded_runs".to_string(),
            JsonValue::from(self.succeeded_runs as i64),
        );
        m.insert(
            "compensated_rollback_runs".to_string(),
            JsonValue::from(self.compensated_rollback_runs as i64),
        );
        m.insert(
            "approval_suspended_runs".to_string(),
            JsonValue::from(self.approval_suspended_runs as i64),
        );
        m.insert(
            "unhandled_failure_runs".to_string(),
            JsonValue::from(self.unhandled_failure_runs as i64),
        );
        m.insert(
            "resilience_score".to_string(),
            JsonValue::from(self.resilience_score),
        );
        m.insert(
            "deterministic_fingerprint".to_string(),
            JsonValue::from(self.deterministic_fingerprint.as_str()),
        );
        m.insert(
            "seed_outcomes".to_string(),
            JsonValue::Array(self.seed_outcomes.iter().map(|o| o.to_json()).collect()),
        );
        m
    }
}

/// Deterministic Simulator (`dst`) for verifying CodeMode scripts under chaos conditions.
pub struct DeterministicSimulator;

impl DeterministicSimulator {
    /// Runs `code` across `config.num_runs` deterministic seeds using `Xoshiro256StarStar`.
    /// Each run intercepts tool calls, injects faults according to `config`, and executes
    /// LIFO Saga compensations if a fault occurs after mutations have succeeded.
    pub fn simulate_script<F, C>(
        code: &str,
        limits: SandboxLimits,
        config: FaultInjectionConfig,
        mut base_tool_handler: F,
        mut resolve_compensation: C,
    ) -> SimulationReport
    where
        F: FnMut(&str, &str, &[JsonValue]) -> Result<(JsonValue, Option<CompletedMutationRecord>), CodeModeError>,
        C: FnMut(&CompletedMutationRecord) -> SagaCompensationRecord,
    {
        let executor = CodeModeExecutor::new(limits);
        let handler_mutex = Mutex::new(&mut base_tool_handler);
        let mut outcomes = Vec::with_capacity(config.num_runs);
        let mut succeeded_runs = 0usize;
        let mut compensated_rollback_runs = 0usize;
        let mut approval_suspended_runs = 0usize;
        let mut unhandled_failure_runs = 0usize;

        for run_idx in 0..config.num_runs {
            let seed = config.base_seed.wrapping_add(run_idx as u64);
            let mut prng = Xoshiro256StarStar::seed_from_u64(seed);
            let injected_faults: Mutex<Vec<String>> = Mutex::new(Vec::new());
            let completed_mutations: Mutex<Vec<CompletedMutationRecord>> = Mutex::new(Vec::new());

            let output: CodeOutput = {
                let mut invoke_tool_cb =
                    |server: &str, tool: &str, args: Vec<JsonValue>| -> Result<JsonValue, CodeModeError> {
                        if prng.gen_bool(config.network_drop_prob) {
                            let msg = format!(
                                "DST_FAULT[seed={}]: network_drop on {}.{}",
                                seed, server, tool
                            );
                            injected_faults.lock().unwrap().push(msg.clone());
                            return Err(CodeModeError::RuntimeError(msg));
                        }
                        if prng.gen_bool(config.circuit_open_prob) {
                            let msg = format!(
                                "DST_FAULT[seed={}]: circuit_breaker_open on {}.{}",
                                seed, server, tool
                            );
                            injected_faults.lock().unwrap().push(msg.clone());
                            return Err(CodeModeError::RuntimeError(msg));
                        }
                        if prng.gen_bool(config.latency_spike_prob) {
                            injected_faults.lock().unwrap().push(format!(
                                "DST_FAULT[seed={}]: latency_spike_250ms on {}.{}",
                                seed, server, tool
                            ));
                        }
                        if prng.gen_bool(config.malformed_payload_prob) {
                            injected_faults.lock().unwrap().push(format!(
                                "DST_FAULT[seed={}]: malformed_payload on {}.{}",
                                seed, server, tool
                            ));
                            let mut corrupted = JsonValue::object();
                            corrupted.insert("_dst_corrupted".to_string(), JsonValue::Bool(true));
                            return Ok(corrupted);
                        }

                        let mut h = handler_mutex.lock().unwrap();
                        let (val, maybe_mutation) = (*h)(server, tool, &args)?;
                        if let Some(rec) = maybe_mutation {
                            completed_mutations.lock().unwrap().push(rec);
                        }
                        Ok(val)
                    };

                // In simulation mode, even parallel waves MUST execute in deterministic index order
                // so PRNG state transitions are 100% reproducible for a given seed.
                let mut invoke_parallel_cb =
                    |calls: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
                        let mut wave_prng = Xoshiro256StarStar::seed_from_u64(
                            seed ^ (0x517cc1b727220a95u64.wrapping_mul(calls.len() as u64 + 1)),
                        );
                        let mut results = Vec::with_capacity(calls.len());
                        for (srv, tl, a) in calls {
                            if wave_prng.gen_bool(config.network_drop_prob) {
                                let msg = format!(
                                    "DST_FAULT[seed={}]: parallel_wave network_drop on {}.{}",
                                    seed, srv, tl
                                );
                                injected_faults.lock().unwrap().push(msg.clone());
                                return Err(CodeModeError::RuntimeError(msg));
                            }
                            if wave_prng.gen_bool(config.circuit_open_prob) {
                                let msg = format!(
                                    "DST_FAULT[seed={}]: parallel_wave circuit_breaker_open on {}.{}",
                                    seed, srv, tl
                                );
                                injected_faults.lock().unwrap().push(msg.clone());
                                return Err(CodeModeError::RuntimeError(msg));
                            }
                            let mut h = handler_mutex.lock().unwrap();
                            let (val, maybe_mut) = (*h)(&srv, &tl, &a)?;
                            if let Some(rec) = maybe_mut {
                                completed_mutations.lock().unwrap().push(rec);
                            }
                            results.push(val);
                        }
                        Ok(results)
                    };

                let search_cb = |_q: &str| JsonValue::Array(Vec::new());
                let describe_cb = |_s: &str, _t: Option<&str>| String::new();

                let mut host = HostCallbacks {
                    invoke_tool: &mut invoke_tool_cb,
                    invoke_parallel_batch: &mut invoke_parallel_cb,
                    search_catalog: &search_cb,
                    describe_types: &describe_cb,
                };

                executor.execute(code, &mut host, &format!("dst-seed-{}", seed))
            };

            let mut final_status = output.status.clone();
            let mut saga_count = 0usize;
            let mut completed_vec = completed_mutations.into_inner().unwrap();
            let faults_vec = injected_faults.into_inner().unwrap();

            if final_status == "ERROR" && !completed_vec.is_empty() {
                // Unwind completed mutations in LIFO order
                while let Some(completed) = completed_vec.pop() {
                    let _comp_rec = resolve_compensation(&completed);
                    saga_count += 1;
                }
                final_status = "SAGA_ROLLED_BACK".to_string();
                compensated_rollback_runs += 1;
            } else if final_status == "OK" {
                succeeded_runs += 1;
            } else if final_status == "APPROVAL_REQUIRED" {
                approval_suspended_runs += 1;
            } else {
                unhandled_failure_runs += 1;
            }

            outcomes.push(SeedRunOutcome {
                seed,
                status: final_status,
                injected_faults: faults_vec,
                saga_compensations_triggered: saga_count,
                tool_calls: output.tool_calls,
                ops_consumed: output.ops_consumed,
                error: output.error,
            });
        }

        // Compute deterministic SHA-256 fingerprint across all seed outcomes
        let mut fingerprint_payload = String::new();
        for o in &outcomes {
            fingerprint_payload.push_str(&format!(
                "{}:{}:{}:{}:{};",
                o.seed,
                o.status,
                o.saga_compensations_triggered,
                o.tool_calls,
                o.injected_faults.len()
            ));
        }
        let deterministic_fingerprint = sha256_hex(fingerprint_payload.as_bytes());

        // Safe outcomes are OK, SAGA_ROLLED_BACK (cleanly compensated), or APPROVAL_REQUIRED
        // Plus if a read-only call failed cleanly before any uncompensated mutation occurred, count how many runs left zero uncompensated state
        let safe_runs = succeeded_runs + compensated_rollback_runs + approval_suspended_runs;
        let resilience_score = if config.num_runs > 0 {
            ((safe_runs as f64 / config.num_runs as f64) * 1000.0).round() / 10.0
        } else {
            100.0
        };

        SimulationReport {
            config,
            total_runs: outcomes.len(),
            succeeded_runs,
            compensated_rollback_runs,
            approval_suspended_runs,
            unhandled_failure_runs,
            resilience_score,
            deterministic_fingerprint,
            seed_outcomes: outcomes,
        }
    }
}
