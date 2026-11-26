//! `wasmi`-backed WebAssembly sandbox execution profile (`sandbox-wasm` feature, v6.5.0).
//!
//! Provides:
//! - Strict OS-independent instruction-fuel and linear-memory isolation with zero WASI filesystem/network imports
//! - Bounded LRU cache of compiled `(Engine, Module)` keyed by SHA-256 digest of `.wasm` bytes (< 30 µs warm boot)
//! - Omni Wasm Guest ABI v1:
//!   - Guest exports: `memory`, `omni_alloc(len: i32) -> i32`, `omni_run(input_ptr: i32, input_len: i32) -> i64`
//!   - Host imports (`"omni"` namespace):
//!     - `omni::log_i64(val: i64)`
//!     - `omni::log_utf8(ptr: i32, len: i32)` (PII-redacted)
//!     - `omni::call_tool(req_ptr: i32, req_len: i32) -> i64` (governed host tool bridge)

use crate::data_plane::codemode::CodeOutput;
use crate::data_plane::security::PIIRedactor;
use crate::json::JsonValue;
use crate::models::{FeatureStatus, FUEL_EXHAUSTED, INTERNAL_ERROR, MEMORY_EXHAUSTED};
use crate::storage::sha256_hex;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use wasmi::core::TrapCode;
use wasmi::{Caller, Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder, Val};

const MAX_COMPILED_MODULES_CACHE: usize = 64;
const MAX_WASM_CALL_DEPTH: usize = 8;
const MAX_WASM_LOG_LINES: usize = 256;
const MAX_WASM_LOG_BYTES: usize = 256 * 1024;
const MAX_WASM_TOOL_CALLS_PER_EXEC: usize = 32;

thread_local! {
    static WASM_EXEC_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct DepthGuard;

impl DepthGuard {
    fn try_enter() -> Result<Self, String> {
        WASM_EXEC_DEPTH.with(|d| {
            let cur = d.get();
            if cur >= MAX_WASM_CALL_DEPTH {
                return Err(format!(
                    "Wasm recursion depth limit exceeded (max {})",
                    MAX_WASM_CALL_DEPTH
                ));
            }
            d.set(cur + 1);
            Ok(DepthGuard)
        })
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        WASM_EXEC_DEPTH.with(|d| {
            d.set(d.get().saturating_sub(1));
        });
    }
}

struct CompiledModuleCache {
    engine: Engine,
    entries: HashMap<String, Module>,
    lru_order: VecDeque<String>,
}

static MODULE_CACHE: OnceLock<Mutex<CompiledModuleCache>> = OnceLock::new();
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

fn get_or_compile_module(wasm_bytes: &[u8]) -> Result<(Engine, Module, bool), String> {
    let cache_mutex = MODULE_CACHE.get_or_init(|| {
        let mut cfg = Config::default();
        cfg.consume_fuel(true);
        Mutex::new(CompiledModuleCache {
            engine: Engine::new(&cfg),
            entries: HashMap::new(),
            lru_order: VecDeque::new(),
        })
    });

    let digest = sha256_hex(wasm_bytes);
    let mut cache = cache_mutex
        .lock()
        .map_err(|_| "Wasm module cache lock poisoned".to_string())?;

    if let Some(module) = cache.entries.get(&digest).cloned() {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
        if let Some(pos) = cache.lru_order.iter().position(|k| k == &digest) {
            cache.lru_order.remove(pos);
        }
        cache.lru_order.push_back(digest);
        return Ok((cache.engine.clone(), module, true));
    }

    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    let module = Module::new(&cache.engine, wasm_bytes)
        .map_err(|e| format!("Wasm module compilation failed: {}", e))?;

    if cache.entries.len() >= MAX_COMPILED_MODULES_CACHE {
        if let Some(oldest) = cache.lru_order.pop_front() {
            cache.entries.remove(&oldest);
        }
    }
    cache.entries.insert(digest.clone(), module.clone());
    cache.lru_order.push_back(digest);
    Ok((cache.engine.clone(), module, false))
}

/// Host callback type for `omni::call_tool(server, tool, arguments) -> Result<JsonValue, String>`.
pub type WasmToolCallback =
    Arc<dyn Fn(&str, &str, &JsonValue) -> Result<JsonValue, String> + Send + Sync>;

/// Host callback type for read-only byte-range file reads in Wasm Guest ABI v2 (`v6.8.0`).
pub type WasmFileReader = Arc<dyn Fn(&str, u64, usize) -> Result<Vec<u8>, String> + Send + Sync>;

/// Read-only attached file descriptor exposed to a Wasm Guest via ABI v2 (`omni::file_stat` /
/// `omni::file_read_chunk`) with zero ambient filesystem access.
#[derive(Clone)]
pub struct WasmAttachedFile {
    pub file_id: String,
    pub size_bytes: u64,
    pub reader: WasmFileReader,
}

/// Maximum single chunk read permitted by `omni::file_read_chunk` (256 KiB).
pub const MAX_WASM_FILE_CHUNK_BYTES: usize = 256 * 1024;

struct WasmHostState {
    logs: Vec<String>,
    total_log_bytes: usize,
    limits: StoreLimits,
    max_memory_bytes: usize,
    tool_callback: Option<WasmToolCallback>,
    tool_calls: usize,
    intermediate_bytes: usize,
    cancel_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    attached_files: BTreeMap<String, WasmAttachedFile>,
    cancelled: bool,
}

impl WasmHostState {
    fn push_log(&mut self, line: String) {
        if self.logs.len() >= MAX_WASM_LOG_LINES {
            return;
        }
        let line_len = line.len();
        if self.total_log_bytes.saturating_add(line_len) > MAX_WASM_LOG_BYTES {
            return;
        }
        self.total_log_bytes += line_len;
        self.logs.push(line);
    }
}

/// Strict WebAssembly sandbox executor backed by `wasmi`.
pub struct WasmSandboxExecutor {
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
}

impl Default for WasmSandboxExecutor {
    fn default() -> Self {
        Self {
            max_fuel: 250_000,
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

    /// Returns `(hits, misses)` for the shared SHA-256 compiled Wasm module LRU cache.
    pub fn module_cache_stats() -> (u64, u64) {
        (
            CACHE_HITS.load(Ordering::Relaxed),
            CACHE_MISSES.load(Ordering::Relaxed),
        )
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

    fn build_linker(engine: &Engine) -> Linker<WasmHostState> {
        let mut linker = Linker::<WasmHostState>::new(engine);

        let _ = linker.func_wrap(
            "omni",
            "log_i64",
            |mut caller: Caller<'_, WasmHostState>, val: i64| {
                if let Ok(rem) = caller.get_fuel() {
                    let _ = caller.set_fuel(rem.saturating_sub(50));
                }
                caller.data_mut().push_log(format!("[wasm] {}", val));
            },
        );

        let _ = linker.func_wrap(
            "omni",
            "log_utf8",
            |mut caller: Caller<'_, WasmHostState>, ptr: i32, len: i32| {
                if let Ok(rem) = caller.get_fuel() {
                    let _ = caller.set_fuel(rem.saturating_sub(100));
                }
                if ptr < 0 || !(0..=65_536).contains(&len) {
                    return;
                }
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return,
                };
                let mem_size = mem.data_size(&caller);
                if (ptr as usize)
                    .checked_add(len as usize)
                    .map_or(true, |end| end > mem_size)
                {
                    return;
                }
                let mut buf = vec![0u8; len as usize];
                if mem.read(&caller, ptr as usize, &mut buf).is_ok() {
                    let raw = String::from_utf8_lossy(&buf);
                    let redacted = PIIRedactor::redact_text(&raw);
                    caller.data_mut().push_log(format!("[wasm] {}", redacted));
                }
            },
        );

        let _ = linker.func_wrap(
            "omni",
            "call_tool",
            |mut caller: Caller<'_, WasmHostState>, req_ptr: i32, req_len: i32| -> i64 {
                if let Some(ref flag) = caller.data().cancel_flag {
                    if flag.load(Ordering::Relaxed) {
                        caller.data_mut().cancelled = true;
                        let _ = caller.set_fuel(0);
                        return 0;
                    }
                }
                if let Ok(rem) = caller.get_fuel() {
                    let _ = caller.set_fuel(rem.saturating_sub(500));
                }
                if req_ptr < 0 || req_len <= 0 || req_len > 2 * 1024 * 1024 {
                    return 0;
                }
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return 0,
                };
                let mem_size = mem.data_size(&caller);
                if (req_ptr as usize)
                    .checked_add(req_len as usize)
                    .map_or(true, |end| end > mem_size)
                {
                    return 0;
                }
                if caller.data().tool_calls >= MAX_WASM_TOOL_CALLS_PER_EXEC {
                    return write_json_to_guest(
                        &mut caller,
                        &json!({
                            "ok": false,
                            "error": format!(
                                "Wasm tool call quota exceeded (max {} calls per invocation)",
                                MAX_WASM_TOOL_CALLS_PER_EXEC
                            )
                        }),
                    );
                }
                let mut req_buf = vec![0u8; req_len as usize];
                if mem.read(&caller, req_ptr as usize, &mut req_buf).is_err() {
                    return 0;
                }
                let req_str = match std::str::from_utf8(&req_buf) {
                    Ok(s) => s,
                    Err(_) => return 0,
                };
                let req_json = match JsonValue::parse(req_str) {
                    Ok(j) => j,
                    Err(e) => {
                        return write_json_to_guest(
                            &mut caller,
                            &json!({"ok": false, "error": format!("Invalid tool request JSON: {}", e)}),
                        );
                    }
                };
                let server = req_json
                    .get("server")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let tool = req_json.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = req_json
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(JsonValue::object);

                let cb_opt = caller.data().tool_callback.clone();
                let resp_json = match cb_opt {
                    Some(cb) => {
                        caller.data_mut().tool_calls += 1;
                        match cb(server, tool, &arguments) {
                            Ok(val) => {
                                let redacted = PIIRedactor::redact_payload(&val);
                                json!({"ok": true, "result": redacted})
                            }
                            Err(err_msg) => json!({"ok": false, "error": err_msg}),
                        }
                    }
                    None => json!({
                        "ok": false,
                        "error": "No host tool bridge configured for this Wasm execution"
                    }),
                };

                let bytes_len = resp_json.to_string_compact().len();
                caller.data_mut().intermediate_bytes += bytes_len;
                write_json_to_guest(&mut caller, &resp_json)
            },
        );

        // Wasm Guest ABI v2: `omni::file_stat(id_ptr, id_len) -> i64`
        let _ = linker.func_wrap(
            "omni",
            "file_stat",
            |mut caller: Caller<'_, WasmHostState>, id_ptr: i32, id_len: i32| -> i64 {
                if let Some(ref flag) = caller.data().cancel_flag {
                    if flag.load(Ordering::Relaxed) {
                        caller.data_mut().cancelled = true;
                        let _ = caller.set_fuel(0);
                        return -2;
                    }
                }
                if let Ok(rem) = caller.get_fuel() {
                    let _ = caller.set_fuel(rem.saturating_sub(100));
                }
                if id_ptr < 0 || id_len <= 0 || id_len > 256 {
                    return -1;
                }
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return -1,
                };
                let mem_size = mem.data_size(&caller);
                if (id_ptr as usize)
                    .checked_add(id_len as usize)
                    .map_or(true, |end| end > mem_size)
                {
                    return -1;
                }
                let mut id_buf = vec![0u8; id_len as usize];
                if mem.read(&caller, id_ptr as usize, &mut id_buf).is_err() {
                    return -1;
                }
                let file_id = match std::str::from_utf8(&id_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };
                caller
                    .data()
                    .attached_files
                    .get(file_id)
                    .map(|f| f.size_bytes as i64)
                    .unwrap_or(-1)
            },
        );

        // Wasm Guest ABI v2: `omni::file_read_chunk(id_ptr, id_len, offset, max_len, dst_ptr) -> i32`
        let _ = linker.func_wrap(
            "omni",
            "file_read_chunk",
            |mut caller: Caller<'_, WasmHostState>,
             id_ptr: i32,
             id_len: i32,
             offset: i64,
             max_len: i32,
             dst_ptr: i32|
             -> i32 {
                if let Some(ref flag) = caller.data().cancel_flag {
                    if flag.load(Ordering::Relaxed) {
                        caller.data_mut().cancelled = true;
                        let _ = caller.set_fuel(0);
                        return -2;
                    }
                }
                if id_ptr < 0
                    || id_len <= 0
                    || id_len > 256
                    || offset < 0
                    || max_len < 0
                    || dst_ptr < 0
                {
                    return -1;
                }
                let capped_len = (max_len as usize).min(MAX_WASM_FILE_CHUNK_BYTES);
                if capped_len == 0 {
                    return 0;
                }
                let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                    Some(m) => m,
                    None => return -1,
                };
                let mem_size = mem.data_size(&caller);
                if (id_ptr as usize)
                    .checked_add(id_len as usize)
                    .map_or(true, |end| end > mem_size)
                {
                    return -1;
                }
                if (dst_ptr as usize)
                    .checked_add(capped_len)
                    .map_or(true, |end| end > mem_size)
                {
                    return -1;
                }
                let mut id_buf = vec![0u8; id_len as usize];
                if mem.read(&caller, id_ptr as usize, &mut id_buf).is_err() {
                    return -1;
                }
                let file_id = match std::str::from_utf8(&id_buf) {
                    Ok(s) => s.to_string(),
                    Err(_) => return -1,
                };
                let attached = match caller.data().attached_files.get(&file_id).cloned() {
                    Some(f) => f,
                    None => return -1,
                };
                let chunk = match (attached.reader)(&file_id, offset as u64, capped_len) {
                    Ok(bytes) => bytes,
                    Err(_) => return -1,
                };
                let to_write = chunk.len().min(capped_len);
                // Fuel-meter per KiB read (20 fuel units / KiB, min 50)
                let fuel_cost = ((to_write.div_ceil(1024) as u64) * 20).max(50);
                if let Ok(rem) = caller.get_fuel() {
                    if rem < fuel_cost {
                        let _ = caller.set_fuel(0);
                        return -3;
                    }
                    let _ = caller.set_fuel(rem.saturating_sub(fuel_cost));
                }
                if mem
                    .write(&mut caller, dst_ptr as usize, &chunk[..to_write])
                    .is_err()
                {
                    return -1;
                }
                caller.data_mut().intermediate_bytes += to_write;
                to_write as i32
            },
        );

        linker
    }

    /// Executes a compiled `.wasm` module numeric entrypoint with fuel & linear-memory caps.
    /// Zero WASI filesystem/network host functions are exposed.
    pub fn execute_wasm(&self, wasm_bytes: &[u8], export_name: &str, args: &[i64]) -> CodeOutput {
        let parse_start = Instant::now();
        let (engine, module, _cache_hit) = match get_or_compile_module(wasm_bytes) {
            Ok(pair) => pair,
            Err(e) => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    e,
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
            total_log_bytes: 0,
            limits: StoreLimitsBuilder::new()
                .memory_size(self.max_memory_bytes)
                .build(),
            max_memory_bytes: self.max_memory_bytes,
            tool_callback: None,
            tool_calls: 0,
            intermediate_bytes: 0,
            cancel_flag: None,
            attached_files: BTreeMap::new(),
            cancelled: false,
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

        let linker = Self::build_linker(&engine);
        let instance_pre = match linker.instantiate(&mut store, &module) {
            Ok(inst) => inst,
            Err(e) => {
                return self.handle_instantiation_err(e, &store, boot_parse_us, eval_start);
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
            return self.classify_trap(err, boot_parse_us, eval_us, ops_consumed, logs);
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
            tool_calls: store.data().tool_calls,
            intermediate_bytes_processed: store.data().intermediate_bytes,
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

    /// Executes a `.wasm` plugin/script using **Omni Wasm Guest ABI v1**:
    /// - Serializes `input_json` into guest linear memory via guest `omni_alloc(len: i32) -> i32`
    /// - Calls guest `omni_run(input_ptr: i32, input_len: i32) -> i64` (`(out_ptr << 32) | out_len`)
    /// - Exposes `"omni"::log_utf8` and `"omni"::call_tool` host imports
    /// - Reads & parses UTF-8 JSON output from guest linear memory.
    pub fn execute_wasm_abi_v1(
        &self,
        wasm_bytes: &[u8],
        input_json: &JsonValue,
        tool_callback: Option<WasmToolCallback>,
    ) -> CodeOutput {
        self.execute_wasm_abi_v2(wasm_bytes, input_json, tool_callback, Vec::new(), None)
    }

    /// Wasm Guest ABI v2 (`v6.8.0`): extends ABI v1 with read-only `attached_files`
    /// (`omni::file_stat`, `omni::file_read_chunk`) and cooperative `cancel_flag`.
    pub fn execute_wasm_abi_v2(
        &self,
        wasm_bytes: &[u8],
        input_json: &JsonValue,
        tool_callback: Option<WasmToolCallback>,
        attached_files: Vec<WasmAttachedFile>,
        cancel_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> CodeOutput {
        if let Some(ref flag) = cancel_flag {
            if flag.load(Ordering::Relaxed) {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    "Cancelled: Wasm execution cancelled before start".to_string(),
                    0,
                    0,
                    0,
                    Vec::new(),
                );
            }
        }
        let _depth_guard = match DepthGuard::try_enter() {
            Ok(g) => g,
            Err(e) => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    e,
                    0,
                    0,
                    0,
                    Vec::new(),
                );
            }
        };
        let parse_start = Instant::now();
        let (engine, module, _cache_hit) = match get_or_compile_module(wasm_bytes) {
            Ok(pair) => pair,
            Err(e) => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    e,
                    parse_start.elapsed().as_micros() as u64,
                    0,
                    0,
                    Vec::new(),
                );
            }
        };
        let boot_parse_us = parse_start.elapsed().as_micros() as u64;

        let eval_start = Instant::now();
        let mut attached_map = BTreeMap::new();
        for f in attached_files {
            attached_map.insert(f.file_id.clone(), f);
        }
        let host_state = WasmHostState {
            logs: Vec::new(),
            total_log_bytes: 0,
            limits: StoreLimitsBuilder::new()
                .memory_size(self.max_memory_bytes)
                .build(),
            max_memory_bytes: self.max_memory_bytes,
            tool_callback,
            tool_calls: 0,
            intermediate_bytes: 0,
            cancel_flag,
            attached_files: attached_map,
            cancelled: false,
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

        let linker = Self::build_linker(&engine);
        let instance_pre = match linker.instantiate(&mut store, &module) {
            Ok(inst) => inst,
            Err(e) => {
                return self.handle_instantiation_err(e, &store, boot_parse_us, eval_start);
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

        let memory = match instance.get_memory(&store, "memory") {
            Some(m) => m,
            None => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    "Wasm ABI v1 module must export linear 'memory'".to_string(),
                    boot_parse_us,
                    eval_start.elapsed().as_micros() as u64,
                    0,
                    store.data().logs.clone(),
                );
            }
        };

        let alloc_fn = match instance.get_typed_func::<i32, i32>(&store, "omni_alloc") {
            Ok(f) => f,
            Err(e) => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    format!("Wasm ABI v1 export 'omni_alloc(i32) -> i32' missing: {}", e),
                    boot_parse_us,
                    eval_start.elapsed().as_micros() as u64,
                    0,
                    store.data().logs.clone(),
                );
            }
        };

        let run_fn = match instance.get_typed_func::<(i32, i32), i64>(&store, "omni_run") {
            Ok(f) => f,
            Err(e) => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    format!(
                        "Wasm ABI v1 export 'omni_run(i32, i32) -> i64' missing: {}",
                        e
                    ),
                    boot_parse_us,
                    eval_start.elapsed().as_micros() as u64,
                    0,
                    store.data().logs.clone(),
                );
            }
        };

        let input_bytes = input_json.to_string_compact().into_bytes();
        if input_bytes.len() > self.max_memory_bytes || input_bytes.len() > i32::MAX as usize {
            return Self::err_output(
                FeatureStatus::MemoryExhausted,
                MEMORY_EXHAUSTED,
                format!(
                    "Wasm ABI v1 input length {} exceeds memory cap {}",
                    input_bytes.len(),
                    self.max_memory_bytes
                ),
                boot_parse_us,
                eval_start.elapsed().as_micros() as u64,
                0,
                store.data().logs.clone(),
            );
        }
        let input_len = input_bytes.len() as i32;
        let input_ptr = match alloc_fn.call(&mut store, input_len) {
            Ok(p) if p >= 0 => p,
            Ok(p) => {
                return Self::err_output(
                    FeatureStatus::Error,
                    INTERNAL_ERROR,
                    format!("omni_alloc returned invalid negative pointer {}", p),
                    boot_parse_us,
                    eval_start.elapsed().as_micros() as u64,
                    self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0)),
                    store.data().logs.clone(),
                );
            }
            Err(err) => {
                let eval_us = eval_start.elapsed().as_micros() as u64;
                let ops = self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0));
                let logs = store.data().logs.clone();
                return self.classify_trap(err, boot_parse_us, eval_us, ops, logs);
            }
        };

        if let Err(e) = memory.write(&mut store, input_ptr as usize, &input_bytes) {
            return Self::err_output(
                FeatureStatus::MemoryExhausted,
                MEMORY_EXHAUSTED,
                format!("Failed writing input into Wasm linear memory: {}", e),
                boot_parse_us,
                eval_start.elapsed().as_micros() as u64,
                self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0)),
                store.data().logs.clone(),
            );
        }

        let packed_res = run_fn.call(&mut store, (input_ptr, input_len));
        let eval_us = eval_start.elapsed().as_micros() as u64;
        let ops_consumed = self.max_fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        let logs = store.data().logs.clone();
        let tool_calls = store.data().tool_calls;
        let intermediate_bytes = store.data().intermediate_bytes;
        let was_cancelled = store.data().cancelled
            || store
                .data()
                .cancel_flag
                .as_ref()
                .map(|f| f.load(Ordering::Relaxed))
                .unwrap_or(false);
        let mem_bytes = memory.data_size(&store);

        if was_cancelled {
            return Self::err_output(
                FeatureStatus::Error,
                INTERNAL_ERROR,
                "Cancelled: Wasm execution cancelled cooperatively".to_string(),
                boot_parse_us,
                eval_us,
                ops_consumed,
                logs,
            );
        }

        let packed = match packed_res {
            Ok(v) => v as u64,
            Err(err) => {
                return self.classify_trap(err, boot_parse_us, eval_us, ops_consumed, logs);
            }
        };

        let out_ptr = (packed >> 32) as usize;
        let out_len = (packed & 0xFFFF_FFFF) as usize;
        if out_len > self.max_memory_bytes
            || out_ptr
                .checked_add(out_len)
                .map_or(true, |end| end > mem_bytes)
        {
            return Self::err_output(
                FeatureStatus::MemoryExhausted,
                MEMORY_EXHAUSTED,
                format!(
                    "Wasm ABI v1 output slice ({}..{}) exceeds linear memory bounds ({} bytes)",
                    out_ptr,
                    out_ptr.saturating_add(out_len),
                    mem_bytes
                ),
                boot_parse_us,
                eval_us,
                ops_consumed,
                logs,
            );
        }

        let result_val = if out_len == 0 {
            JsonValue::Null
        } else {
            let mut out_buf = vec![0u8; out_len];
            if let Err(e) = memory.read(&store, out_ptr, &mut out_buf) {
                return Self::err_output(
                    FeatureStatus::MemoryExhausted,
                    MEMORY_EXHAUSTED,
                    format!(
                        "Failed reading Wasm ABI v1 output from linear memory: {}",
                        e
                    ),
                    boot_parse_us,
                    eval_us,
                    ops_consumed,
                    logs,
                );
            }
            let out_str = String::from_utf8_lossy(&out_buf);
            JsonValue::parse(&out_str).unwrap_or_else(|_| JsonValue::Str(out_str.into_owned()))
        };

        let redacted_result = PIIRedactor::redact_payload(&result_val);
        let out_bytes = redacted_result.to_string_compact().len();

        CodeOutput {
            status: "OK".to_string(),
            feature_status: FeatureStatus::Ok,
            error_code: None,
            result: redacted_result,
            logs,
            pending_approvals: Vec::new(),
            error: None,
            boot_parse_us,
            eval_us,
            ops_consumed,
            peak_memory_bytes: mem_bytes,
            tool_calls,
            intermediate_bytes_processed: intermediate_bytes,
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

    fn handle_instantiation_err(
        &self,
        e: wasmi::Error,
        store: &Store<WasmHostState>,
        boot_parse_us: u64,
        eval_start: Instant,
    ) -> CodeOutput {
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
        Self::err_output(
            FeatureStatus::Error,
            INTERNAL_ERROR,
            format!("Wasm instantiation failed: {}", err_str),
            boot_parse_us,
            eval_us,
            ops,
            store.data().logs.clone(),
        )
    }

    fn classify_trap(
        &self,
        err: wasmi::Error,
        boot_parse_us: u64,
        eval_us: u64,
        ops_consumed: u64,
        logs: Vec<String>,
    ) -> CodeOutput {
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
        Self::err_output(
            FeatureStatus::Error,
            INTERNAL_ERROR,
            format!("Wasm execution trap: {}", err_str),
            boot_parse_us,
            eval_us,
            ops_consumed,
            logs,
        )
    }
}

fn write_json_to_guest(caller: &mut Caller<'_, WasmHostState>, val: &JsonValue) -> i64 {
    let bytes = val.to_string_compact().into_bytes();
    let max_mem = caller.data().max_memory_bytes;
    if bytes.len() > max_mem || bytes.len() > i32::MAX as usize {
        return 0;
    }
    let len = bytes.len() as i32;
    let alloc = match caller
        .get_export("omni_alloc")
        .and_then(|e| e.into_func())
        .and_then(|f| f.typed::<i32, i32>(&*caller).ok())
    {
        Some(f) => f,
        None => return 0,
    };
    let ptr = match alloc.call(&mut *caller, len) {
        Ok(p) if p >= 0 => p,
        _ => return 0,
    };
    let mem = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return 0,
    };
    if mem.write(&mut *caller, ptr as usize, &bytes).is_err() {
        return 0;
    }
    (((ptr as u32 as u64) << 32) | (len as u32 as u64)) as i64
}

/// Decodes a standard Base64-encoded `.wasm` binary payload.
pub fn decode_wasm_base64(b64: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = b64.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut val = 0u32;
    let mut valb = -8i32;
    for b in cleaned.bytes() {
        if b == b'=' {
            break;
        }
        let d = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("Invalid base64 character '{}'", b as char)),
        };
        val = (val << 6) | d;
        valb += 6;
        if valb >= 0 {
            out.push(((val >> valb) & 0xFF) as u8);
            valb -= 8;
        }
    }
    if out.len() < 4 || &out[..4] != b"\0asm" {
        return Err(
            "Decoded payload does not start with valid WebAssembly magic header (\\0asm)"
                .to_string(),
        );
    }
    Ok(out)
}
