//! Omni Code Mode Execution Engine (`codemode`).
//!
//! Implements two high-performance primitives for code-driven tool orchestration
//! natively in zero-dependency Rust:
//! 1. `CodeModeTypeGenerator`: Compiles registered OmniMCP tool JSON Schemas into compact TypeScript
//!    declarations (`declare namespace codemode { ... }`) and supports `{{types}}` prompt injection.
//! 2. `CodeModeExecutor`: Sub-millisecond in-process AST sandbox executing JavaScript/TypeScript
//!    (`async () => { await codemode.srv.tool(...); }`) and closure scripts (`|x| expr`) with:
//!    - Zero outbound network/filesystem access (`globalOutbound: null` isolation)
//!    - Console output capture (`console.log`, `console.warn`, `console.error` -> `CodeOutput.logs`)
//!    - Built-in Search & Execute discovery (`codemode.search()`, `codemode.describe()`)
//!    - Human-in-the-loop mutation approval gates (`require_mutation_approval`)
//!    - Instruction fuel metering (`max_operations`), stack depth, AST depth, and heap memory caps.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    generate_ulid, FeatureStatus, SagaCompensationRecord, SchemaViolation, ServerRecord,
    SuspendedExecutionFrame, ToolStatus, APPROVAL_REQUIRED, FUEL_EXHAUSTED, INTERNAL_ERROR,
    MEMORY_EXHAUSTED, OUTBOUND_BLOCKED, SCHEMA_PREFLIGHT_FAILED, TIMEOUT,
};
use crate::storage::audit_log::sha256_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

mod columnar;
mod parallelizer;
mod parser;
mod typegen;
mod vm;
#[cfg(feature = "sandbox-wasm")]
pub mod wasm_sandbox;

#[cfg(test)]
mod tests;

use columnar::*;
use parallelizer::*;
pub use parser::*;
pub use typegen::*;
pub use vm::*;
#[cfg(feature = "sandbox-wasm")]
pub use wasm_sandbox::{
    decode_wasm_base64, WasmAttachedFile, WasmFileReader, WasmSandboxExecutor, WasmToolCallback,
};
