//! `wasmi`-backed WebAssembly sandbox execution profile (`sandbox-wasm` feature).
//!
//! Provides strict OS-independent instruction-fuel and linear-memory isolation
//! with zero filesystem/network WASI imports alongside the AST fast-path.

use crate::data_plane::codemode::CodeOutput;
use crate::json::JsonValue;
use crate::models::{FeatureStatus, FUEL_EXHAUSTED, INTERNAL_ERROR, MEMORY_EXHAUSTED};
use std::time::Instant;
use wasmi::core::TrapCode;
use wasmi::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, Val};

struct WasmHostState {
    logs: Vec<String>,
    limits: StoreLimits,
}

/// Strict WebAssembly sandbox executor backed by `wasmi`.
pub struct WasmSandboxExecutor {
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
}

impl Default for WasmSandboxExecutor {
    fn default() -> Self {
        Self {
            max_fuel: 100_000,
            max_memory_bytes: 16 * 1024 * 1024,
        }
    }
}

impl WasmSandboxExecutor {
    pub fn new(max_fuel: u64, max_memory_bytes: usize) -> Self {
        Self {
            max_fuel,
            max_memory_bytes,
        }
    }

    fn err_output(
        feature_status: FeatureStatus,
        error_code: i32,
        msg: String,
        boot_parse_us: u64,
        eval_us: u64,
        ops_consumed: u64,
        logs: Vec<String>,
    ) -> CodeOutput {
        CodeOutput {
            status: "ERROR".to_string(),
            feature_status,
            error_code: Some(error_code),
            result: JsonValue::Null,
            logs,
            pending_approvals: Vec::new(),
            error: Some(msg),
            boot_parse_us,
            eval_us,
            ops_consumed,
            peak_memory_bytes: 0,
            tool_calls: 0,
            intermediate_bytes_processed: 0,
            output_bytes_returned: 0,
            traceparent: String::new(),
            execution_id: None,
            suspended_frame: None,
            saga_compensations: Vec::new(),
            vectorized_pushdowns: 0,
            rows_scanned_vectorized: 0,
            auto_parallel_waves: 0,
            parallelized_tool_calls: 0,
        }
    }

    /// Executes a compiled `.wasm` module entrypoint with fuel & linear-memory caps.
    /// Zero WASI filesystem/network host functions are exposed.
    pub fn execute_wasm(&self, wasm_bytes: &[u8], export_name: &str, args: &[i64]) -> CodeOutput {
        let parse_start = Instant::now();
        let mut cfg = Config::default();
        cfg.consume_fuel(true);
        let engine = Engine::new(&cfg);

        let module = match Module::new(&engine, wasm_bytes) {
            Ok(m) => m,
            Err(e) => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    format!("Wasm module compilation failed: {}", e),
                    parse_start.elapsed().as_micros() as u64,
                    0,
                    0,
                    Vec::new(),
                );
            }
        };
        let boot_parse_us = parse_start.elapsed().as_micros() as u64;

        let eval_start = Instant::now();
        let host_state = WasmHostState {
            logs: Vec::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(self.max_memory_bytes)
                .build(),
        };
        let mut store = Store::new(&engine, host_state);
        store.limiter(|s| &mut s.limits);
        if let Err(e) = store.set_fuel(self.max_fuel) {
            return Self::err_output(
                FeatureStatus::Error,
                INTERNAL_ERROR,
                format!("Failed to set Wasm fuel: {}", e),
                boot_parse_us,
                0,
                0,
                Vec::new(),
            );
        }

        let mut linker = Linker::<WasmHostState>::new(&engine);
        let _ = linker.func_wrap(
            "omni",
            "log_i64",
            |mut caller: Caller<'_, WasmHostState>, val: i64| {
                caller.data_mut().logs.push(format!("[wasm] {}", val));
            },
        );

        let instance_pre = match linker.instantiate(&mut store, &module) {
            Ok(inst) => inst,
            Err(e) => {
                let ops = self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0));
                let eval_us = eval_start.elapsed().as_micros() as u64;
                let err_str = e.to_string();
                if err_str.to_lowercase().contains("memory") {
                    return Self::err_output(
                        FeatureStatus::MemoryExhausted,
                        MEMORY_EXHAUSTED,
                        format!("Wasm sandbox memory limit exceeded: {}", err_str),
                        boot_parse_us,
                        eval_us,
                        ops,
                        store.data().logs.clone(),
                    );
                }
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    format!("Wasm instantiation failed: {}", err_str),
                    boot_parse_us,
                    eval_us,
                    ops,
                    store.data().logs.clone(),
                );
            }
        };
        let instance = match instance_pre.start(&mut store) {
            Ok(inst) => inst,
            Err(e) => {
                let ops = self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0));
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    format!("Wasm start failed: {}", e),
                    boot_parse_us,
                    eval_start.elapsed().as_micros() as u64,
                    ops,
                    store.data().logs.clone(),
                );
            }
        };

        let func = match instance.get_func(&store, export_name) {
            Some(f) => f,
            None => {
                let ops = self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0));
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    format!("Wasm export '{}' not found", export_name),
                    boot_parse_us,
                    eval_start.elapsed().as_micros() as u64,
                    ops,
                    store.data().logs.clone(),
                );
            }
        };

        let ty = func.ty(&store);
        let wasm_args: Vec<Val> = args.iter().map(|v| Val::I64(*v)).collect();
        let mut wasm_results = vec![Val::I64(0); ty.results().len()];
        let call_res = func.call(&mut store, &wasm_args, &mut wasm_results);
        let eval_us = eval_start.elapsed().as_micros() as u64;
        let ops_consumed = self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        let logs = store.data().logs.clone();

        if let Err(err) = call_res {
            if let Some(TrapCode::OutOfFuel) = err.as_trap_code() {
                return Self::err_output(
                    FeatureStatus::FuelExhausted,
                    FUEL_EXHAUSTED,
                    format!(
                        "Wasm sandbox fuel exhausted after {} operations (limit: {})",
                        ops_consumed, self.max_fuel
                    ),
                    boot_parse_us,
                    eval_us,
                    ops_consumed,
                    logs,
                );
            }
            let err_str = err.to_string();
            if err_str.contains("memory") || err_str.contains("Memory") {
                return Self::err_output(
                    FeatureStatus::MemoryExhausted,
                    MEMORY_EXHAUSTED,
                    format!("Wasm sandbox memory limit exceeded: {}", err_str),
                    boot_parse_us,
                    eval_us,
                    ops_consumed,
                    logs,
                );
            }
            return Self::err_output(
                FeatureStatus::Error,
                INTERNAL_ERROR,
                format!("Wasm execution trap: {}", err_str),
                boot_parse_us,
                eval_us,
                ops_consumed,
                logs,
            );
        }

        let result_val = match wasm_results.first() {
            Some(Val::I64(v)) => JsonValue::from(*v as f64),
            Some(Val::I32(v)) => JsonValue::from(*v as f64),
            Some(Val::F64(v)) => JsonValue::from(f64::from_bits(v.to_bits())),
            Some(Val::F32(v)) => JsonValue::from(f32::from_bits(v.to_bits()) as f64),
            _ => JsonValue::Null,
        };
        let out_bytes = result_val.to_string_compact().len();

        CodeOutput {
            status: "OK".to_string(),
            feature_status: FeatureStatus::Ok,
            error_code: None,
            result: result_val,
            logs,
            pending_approvals: Vec::new(),
            error: None,
            boot_parse_us,
            eval_us,
            ops_consumed,
            peak_memory_bytes: self.max_memory_bytes.min(65536),
            tool_calls: 0,
            intermediate_bytes_processed: 0,
            output_bytes_returned: out_bytes,
            traceparent: String::new(),
            execution_id: None,
            suspended_frame: None,
            saga_compensations: Vec::new(),
            vectorized_pushdowns: 0,
            rows_scanned_vectorized: 0,
            auto_parallel_waves: 0,
            parallelized_tool_calls: 0,
        }
    }
}
