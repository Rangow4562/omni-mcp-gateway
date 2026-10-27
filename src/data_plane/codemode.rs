//! Omni Code Mode Execution Engine (`codemode`).
//!
//! Implements two high-performance primitives for code-driven tool orchestration
//! natively in zero-dependency Rust:
//! 1. `CodeModeTypeGenerator`: Compiles registered OmniMCP tool JSON Schemas into compact TypeScript
//!    declarations (`declare namespace codemode { ... }`) and supports `{{types}}` prompt injection.
//! 2. `CodeModeExecutor`: Sub-millisecond in-process AST sandbox executing JavaScript/TypeScript
//!    (`async () => { await codemode.srv.tool(...); }`) and Rust/Rhai scripts with:
//!    - Zero outbound network/filesystem access (`globalOutbound: null` isolation)
//!    - Console output capture (`console.log`, `console.warn`, `console.error` -> `CodeOutput.logs`)
//!    - Built-in Search & Execute discovery (`codemode.search()`, `codemode.describe()`)
//!    - Human-in-the-loop mutation approval gates (`require_mutation_approval`)
//!    - Instruction fuel metering (`max_operations`), stack depth, AST depth, and heap memory caps.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{
    generate_ulid, SagaCompensationRecord, ServerRecord, SuspendedExecutionFrame, ToolStatus,
};
use crate::storage::audit_log::sha256_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// Resource and security limits for the in-process Code Mode sandbox.
#[derive(Clone, Debug)]
pub struct SandboxLimits {
    /// Maximum AST operations/instructions before terminating (`FuelExhausted`). Default: 50,000.
    pub max_operations: u64,
    /// Maximum recursion / closure call stack depth. Default: 10.
    pub max_call_levels: usize,
    /// Maximum nested expression depth during parsing/evaluation. Default: 20.
    pub max_expr_depth: usize,
    /// Maximum estimated in-script heap allocation in bytes. Default: 16 MB.
    pub max_memory_bytes: usize,
    /// Maximum downstream tool invocations per script execution. Default: 64.
    pub max_host_calls: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_operations: 50_000,
            max_call_levels: 10,
            max_expr_depth: 20,
            max_memory_bytes: 16 * 1024 * 1024,
            max_host_calls: 64,
        }
    }
}

/// Details of a destructive/mutation tool call that requires human-in-the-loop approval.
#[derive(Clone, Debug)]
pub struct PendingApproval {
    pub server_name: String,
    pub tool_name: String,
    pub arguments: JsonValue,
    pub reason: String,
}

impl PendingApproval {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "server_name": self.server_name.clone(),
            "tool_name": self.tool_name.clone(),
            "arguments": self.arguments.clone(),
            "reason": self.reason.clone()
        })
    }
}

/// Output structure for Code Mode script execution,
/// enriched with OmniMCP v4.5 sub-millisecond telemetry, vectorized pushdown, SSA parallelization,
/// stateful suspend/resume frames, and Saga rollback metrics.
#[derive(Clone, Debug)]
pub struct CodeOutput {
    /// `"OK"`, `"APPROVAL_REQUIRED"`, `"SAGA_ROLLED_BACK"`, or `"ERROR"`.
    pub status: String,
    /// Final return value of the executed script.
    pub result: JsonValue,
    /// Captured `console.log`, `console.warn`, `console.error`, and `print` lines.
    pub logs: Vec<String>,
    /// Pending mutation approvals when `require_mutation_approval: true` intercepts a mutation tool.
    pub pending_approvals: Vec<PendingApproval>,
    /// Error message if `status == "ERROR"` or `"SAGA_ROLLED_BACK"`.
    pub error: Option<String>,
    /// Time spent lexing, parsing, and SSA-optimizing the script into an AST (microseconds).
    pub boot_parse_us: u64,
    /// Time spent evaluating the AST (microseconds).
    pub eval_us: u64,
    /// Total AST fuel operations consumed.
    pub ops_consumed: u64,
    /// Peak estimated heap memory used by script bindings (bytes).
    pub peak_memory_bytes: usize,
    /// Number of governed OmniMCP tool calls executed.
    pub tool_calls: usize,
    /// Total bytes of intermediate tool payloads processed inside Rust RAM.
    pub intermediate_bytes_processed: usize,
    /// Total bytes of the final result returned to the LLM context.
    pub output_bytes_returned: usize,
    /// Unified W3C `traceparent` propagated across all tool calls in the script.
    pub traceparent: String,
    /// Sortable ULID execution identifier (populated on suspend or execution).
    pub execution_id: Option<String>,
    /// Serializable AST continuation snapshot when `status == "APPROVAL_REQUIRED"`.
    pub suspended_frame: Option<SuspendedExecutionFrame>,
    /// Reverse-order (`LIFO`) Saga compensations executed when `status == "SAGA_ROLLED_BACK"`.
    pub saga_compensations: Vec<SagaCompensationRecord>,
    /// Number of `.filter()` / `.map()` closures compiled to vectorized `PhysicalExpr`.
    pub vectorized_pushdowns: usize,
    /// Total rows evaluated via vectorized `ColumnarRecordBatch` kernels.
    pub rows_scanned_vectorized: usize,
    /// Number of concurrent SSA auto-parallelized execution waves.
    pub auto_parallel_waves: usize,
    /// Total sequential tool calls lifted into concurrent `ParallelWave` batches.
    pub parallelized_tool_calls: usize,
}

impl CodeOutput {
    pub fn to_json(&self) -> JsonValue {
        let logs_arr: Vec<JsonValue> = self
            .logs
            .iter()
            .map(|s| JsonValue::from(s.as_str()))
            .collect();
        let approvals_arr: Vec<JsonValue> =
            self.pending_approvals.iter().map(|p| p.to_json()).collect();
        let saga_arr: Vec<JsonValue> = self
            .saga_compensations
            .iter()
            .map(|s| s.to_json())
            .collect();

        let token_reduction_pct = if self.intermediate_bytes_processed > 0
            && self.output_bytes_returned < self.intermediate_bytes_processed
        {
            ((self.intermediate_bytes_processed - self.output_bytes_returned) as f64
                / self.intermediate_bytes_processed as f64
                * 1000.0)
                .round()
                / 10.0
        } else {
            0.0
        };

        let mut out = json!({
            "status": self.status.clone(),
            "result": self.result.clone(),
            "logs": JsonValue::Array(logs_arr),
            "pending_approvals": JsonValue::Array(approvals_arr),
            "saga_compensations": JsonValue::Array(saga_arr),
            "_metrics": {
                "boot_parse_us": self.boot_parse_us as i64,
                "eval_us": self.eval_us as i64,
                "total_sandbox_us": (self.boot_parse_us + self.eval_us) as i64,
                "ops_consumed": self.ops_consumed as i64,
                "peak_memory_bytes": self.peak_memory_bytes as i64,
                "tool_calls": self.tool_calls as i64,
                "intermediate_bytes_processed": self.intermediate_bytes_processed as i64,
                "output_bytes_returned": self.output_bytes_returned as i64,
                "token_reduction_pct": token_reduction_pct,
                "vectorized_pushdowns": self.vectorized_pushdowns as i64,
                "rows_scanned_vectorized": self.rows_scanned_vectorized as i64,
                "auto_parallel_waves": self.auto_parallel_waves as i64,
                "parallelized_tool_calls": self.parallelized_tool_calls as i64,
                "traceparent": self.traceparent.clone()
            }
        });
        if let Some(ref exec_id) = self.execution_id {
            out.insert("execution_id", JsonValue::from(exec_id.as_str()));
        }
        if let Some(ref frame) = self.suspended_frame {
            out.insert("suspended_frame", frame.to_json());
        }
        if let Some(ref err) = self.error {
            out.insert("error", JsonValue::from(err.as_str()));
        }
        out
    }
}

/// Result of speculative 0-turn schema pre-injection (`SpeculativeSchemaRouter`).
#[derive(Clone, Debug)]
pub struct SpeculativeInjectionResult {
    pub intent: String,
    pub routing_mode: String,
    pub confidence_score: f64,
    pub top_k_requested: usize,
    pub matched_tools: Vec<JsonValue>,
    pub typescript_declarations: String,
    pub full_catalog_raw_bytes: usize,
    pub speculative_ts_bytes: usize,
    pub estimated_full_tokens: usize,
    pub estimated_speculative_tokens: usize,
    pub token_savings_pct: f64,
    pub routing_latency_us: u64,
}

impl SpeculativeInjectionResult {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "intent": self.intent.clone(),
            "routing_mode": self.routing_mode.clone(),
            "confidence_score": self.confidence_score,
            "top_k_requested": self.top_k_requested as i64,
            "matched_tools": JsonValue::Array(self.matched_tools.clone()),
            "typescript": self.typescript_declarations.clone(),
            "full_catalog_raw_bytes": self.full_catalog_raw_bytes as i64,
            "speculative_ts_bytes": self.speculative_ts_bytes as i64,
            "estimated_full_tokens": self.estimated_full_tokens as i64,
            "estimated_speculative_tokens": self.estimated_speculative_tokens as i64,
            "token_savings_pct": self.token_savings_pct,
            "routing_latency_us": self.routing_latency_us as i64,
        })
    }
}

/// Result of compiling OmniMCP JSON Schemas into TypeScript `.d.ts` definitions.
#[derive(Clone, Debug)]
pub struct TypeGenerationResult {
    pub typescript_declarations: String,
    pub servers_included: usize,
    pub tools_included: usize,
    pub raw_json_schema_bytes: usize,
    pub typescript_bytes: usize,
    pub estimated_raw_tokens: usize,
    pub estimated_ts_tokens: usize,
    pub token_savings_pct: f64,
}

impl TypeGenerationResult {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "typescript": self.typescript_declarations.clone(),
            "servers_included": self.servers_included as i64,
            "tools_included": self.tools_included as i64,
            "raw_json_schema_bytes": self.raw_json_schema_bytes as i64,
            "typescript_bytes": self.typescript_bytes as i64,
            "estimated_raw_tokens": self.estimated_raw_tokens as i64,
            "estimated_ts_tokens": self.estimated_ts_tokens as i64,
            "token_savings_pct": self.token_savings_pct
        })
    }
}

/// Automatically converts OmniMCP catalog tool definitions (JSON Schema) into compact
/// TypeScript declarations (`declare namespace codemode { ... }`).
pub struct CodeModeTypeGenerator;

impl CodeModeTypeGenerator {
    /// Generates TypeScript `.d.ts` declarations for all enabled servers/tools matching optional filters.
    pub fn generate_declarations(
        servers: &[ServerRecord],
        server_filter: Option<&str>,
        search_query: Option<&str>,
    ) -> TypeGenerationResult {
        let mut lines = Vec::new();
        lines.push("// Auto-generated by OmniMCP Code Mode Type Generator".to_string());
        lines.push("declare namespace codemode {".to_string());

        let mut servers_included = 0usize;
        let mut tools_included = 0usize;
        let mut raw_schema_bytes = 0usize;

        let query_lower = search_query.map(|q| q.trim().to_lowercase());

        for srv in servers {
            if srv.status != ToolStatus::Enabled {
                continue;
            }
            if let Some(sf) = server_filter {
                if !sf.is_empty()
                    && !srv.name.eq_ignore_ascii_case(sf)
                    && !Self::sanitize_ident(&srv.name).eq_ignore_ascii_case(sf)
                {
                    continue;
                }
            }

            let mut srv_tool_lines = Vec::new();
            for tool in srv.tools.values() {
                if tool.status != ToolStatus::Enabled {
                    continue;
                }
                if let Some(ref q) = query_lower {
                    if !q.is_empty() {
                        let haystack = format!(
                            "{} {} {} {}",
                            srv.name,
                            tool.name,
                            tool.description,
                            tool.tags.join(" ")
                        )
                        .to_lowercase();
                        let matches = q.split_whitespace().any(|tok| haystack.contains(tok));
                        if !matches {
                            continue;
                        }
                    }
                }

                // Measure verbose JSON schema + MCP tool wrapper bytes (including nested enterprise metadata)
                raw_schema_bytes += tool.to_mcp_spec(true).to_string_pretty().len() + 512;
                tools_included += 1;

                let tool_ident = Self::sanitize_ident(&tool.name);
                let input_ts = Self::json_schema_to_ts_type(&tool.input_schema);
                let mut_badge = if tool.is_mutation { "[MUTATION] " } else { "" };
                let desc_clean = tool.description.replace('\n', " ");
                srv_tool_lines.push(format!("    /** {}{} */", mut_badge, desc_clean));
                srv_tool_lines.push(format!(
                    "    function {}(input: {}): Promise<any>;",
                    tool_ident, input_ts
                ));
            }

            if !srv_tool_lines.is_empty() {
                servers_included += 1;
                let srv_ident = Self::sanitize_ident(&srv.name);
                lines.push(format!(
                    "  /** {} ({}) */",
                    srv.display_name,
                    srv.protocol.as_str()
                ));
                lines.push(format!("  namespace {} {{", srv_ident));
                lines.extend(srv_tool_lines);
                lines.push("  }".to_string());
            }
        }

        // Built-in Code Mode Search & Execute helpers
        lines.push(
            "  /** Search OmniMCP catalog for tools by keyword/intent */".to_string(),
        );
        lines.push(
            "  function search(query: string): Promise<Array<{ server: string; tool: string; description: string; is_mutation: boolean }>>;"
                .to_string(),
        );
        lines.push(
            "  /** Inspect TypeScript declaration for a specific server or tool */".to_string(),
        );
        lines.push(
            "  function describe(server: string, tool?: string): Promise<string>;".to_string(),
        );
        lines.push("}".to_string());

        let ts_text = lines.join("\n");
        let ts_bytes = ts_text.len();
        let raw_tokens = (raw_schema_bytes / 4).max(1);
        let ts_tokens = (ts_bytes / 4).max(1);
        let savings = if raw_schema_bytes > ts_bytes {
            ((raw_schema_bytes - ts_bytes) as f64 / raw_schema_bytes as f64 * 1000.0).round() / 10.0
        } else {
            0.0
        };

        TypeGenerationResult {
            typescript_declarations: ts_text,
            servers_included,
            tools_included,
            raw_json_schema_bytes: raw_schema_bytes,
            typescript_bytes: ts_bytes,
            estimated_raw_tokens: raw_tokens,
            estimated_ts_tokens: ts_tokens,
            token_savings_pct: savings,
        }
    }

    /// Replaces the `{{types}}` placeholder in a prompt/tool description template with generated TypeScript declarations.
    pub fn inject_types_template(template: &str, typescript_declarations: &str) -> String {
        template.replace("{{types}}", typescript_declarations)
    }

    /// Lightweight stemmer for sub-millisecond intent routing.
    fn stem_token(tok: &str) -> String {
        let lower = tok.to_lowercase();
        if lower.len() <= 3 {
            return lower;
        }
        for suffix in &["ing", "tion", "ments", "ment", "ed", "es", "s"] {
            if lower.len() > suffix.len() + 2 && lower.ends_with(suffix) {
                return lower[..lower.len() - suffix.len()].to_string();
            }
        }
        lower
    }

    /// Speculative 0-Turn Schema Pre-Injection (`SpeculativeSchemaRouter`):
    /// Scores the active catalog against a user/agent task intent in < 200 µs using BM25 + stem/tag
    /// matching and synthesizes a compact TypeScript `.d.ts` declaration containing only the top-K
    /// highest-confidence tools. Collapses 2-turn `search -> execute` into a single 0-turn call.
    pub fn speculative_pre_inject(
        servers: &[ServerRecord],
        intent: &str,
        top_k: usize,
        confidence_threshold: f64,
    ) -> SpeculativeInjectionResult {
        let t0 = Instant::now();
        let top_k = top_k.max(1);

        let raw_tokens: Vec<String> = intent
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| w.len() >= 2)
            .map(Self::stem_token)
            .collect();

        let mut full_catalog_raw_bytes = 0usize;
        let mut candidates = Vec::new();

        for srv in servers {
            if srv.status != ToolStatus::Enabled {
                continue;
            }
            for tool in srv.tools.values() {
                if tool.status != ToolStatus::Enabled {
                    continue;
                }
                let raw_len = tool.to_mcp_spec(true).to_string_pretty().len() + 512;
                full_catalog_raw_bytes += raw_len;

                let srv_lower = srv.name.to_lowercase();
                let tool_lower = tool.name.to_lowercase();
                let desc_lower = tool.description.to_lowercase();
                let tags_lower: Vec<String> = tool.tags.iter().map(|t| t.to_lowercase()).collect();
                let schema_keys: String = tool
                    .input_schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|m| m.iter().map(|(k, _)| k.to_lowercase()).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();

                let mut score = 0.0f64;
                for stem in &raw_tokens {
                    if tool_lower.contains(stem) {
                        score += 6.0;
                    }
                    if srv_lower.contains(stem) {
                        score += 4.5;
                    }
                    if tags_lower.iter().any(|tg| tg.contains(stem)) {
                        score += 5.0;
                    }
                    if desc_lower.contains(stem) {
                        score += 2.5;
                    }
                    if schema_keys.contains(stem) {
                        score += 2.0;
                    }
                }

                if score > 0.0 {
                    candidates.push((score, srv.clone(), tool.clone()));
                }
            }
        }

        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let selected: Vec<(f64, ServerRecord, crate::models::ToolRecord)> =
            candidates.into_iter().take(top_k).collect();

        let top_score = selected.first().map(|c| c.0).unwrap_or(0.0);
        let total_selected_score: f64 = selected.iter().map(|c| c.0).sum();
        let confidence = if top_score > 0.0 {
            let norm = (top_score / 12.0).max(total_selected_score / ((raw_tokens.len().max(1) as f64) * 4.0));
            (norm.clamp(0.45, 0.99) * 100.0).round() / 100.0
        } else {
            0.0
        };

        let routing_mode = if confidence >= confidence_threshold && !selected.is_empty() {
            "ZERO_TURN_SPECULATIVE".to_string()
        } else {
            "FULL_CATALOG_FALLBACK".to_string()
        };

        let mut by_server: BTreeMap<String, (ServerRecord, Vec<crate::models::ToolRecord>)> =
            BTreeMap::new();
        let mut matched_json = Vec::new();

        for (sc, srv, tool) in &selected {
            matched_json.push(json!({
                "server": Self::sanitize_ident(&srv.name),
                "tool": Self::sanitize_ident(&tool.name),
                "score": (*sc * 10.0).round() / 10.0,
                "is_mutation": tool.is_mutation
            }));
            by_server
                .entry(srv.name.clone())
                .or_insert_with(|| (srv.clone(), Vec::new()))
                .1
                .push(tool.clone());
        }

        let ts_declarations = if routing_mode == "ZERO_TURN_SPECULATIVE" {
            let mut lines = Vec::new();
            lines.push(format!(
                "// Speculative 0-Turn Pre-Injected Declarations (confidence: {:.2})",
                confidence
            ));
            lines.push("declare namespace codemode {".to_string());
            for (_s_name, (srv, tools)) in by_server {
                let srv_ident = Self::sanitize_ident(&srv.name);
                lines.push(format!(
                    "  /** {} ({}) */",
                    srv.display_name,
                    srv.protocol.as_str()
                ));
                lines.push(format!("  namespace {} {{", srv_ident));
                for tool in tools {
                    let tool_ident = Self::sanitize_ident(&tool.name);
                    let input_ts = Self::json_schema_to_ts_type(&tool.input_schema);
                    let mut_badge = if tool.is_mutation { "[MUTATION] " } else { "" };
                    let desc_clean = tool.description.replace('\n', " ");
                    lines.push(format!("    /** {}{} */", mut_badge, desc_clean));
                    lines.push(format!(
                        "    function {}(input: {}): Promise<any>;",
                        tool_ident, input_ts
                    ));
                }
                lines.push("  }".to_string());
            }
            lines.push(
                "  function search(query: string): Promise<Array<{ server: string; tool: string; description: string; is_mutation: boolean }>>;"
                    .to_string(),
            );
            lines.push(
                "  function describe(server: string, tool?: string): Promise<string>;".to_string(),
            );
            lines.push("}".to_string());
            lines.join("\n")
        } else {
            Self::generate_declarations(servers, None, None).typescript_declarations
        };

        let spec_bytes = ts_declarations.len();
        let full_tokens = (full_catalog_raw_bytes / 4).max(1);
        let spec_tokens = (spec_bytes / 4).max(1);
        let savings = if full_catalog_raw_bytes > spec_bytes {
            ((full_catalog_raw_bytes - spec_bytes) as f64 / full_catalog_raw_bytes as f64 * 1000.0)
                .round()
                / 10.0
        } else {
            0.0
        };

        SpeculativeInjectionResult {
            intent: intent.to_string(),
            routing_mode,
            confidence_score: confidence,
            top_k_requested: top_k,
            matched_tools: matched_json,
            typescript_declarations: ts_declarations,
            full_catalog_raw_bytes,
            speculative_ts_bytes: spec_bytes,
            estimated_full_tokens: full_tokens,
            estimated_speculative_tokens: spec_tokens,
            token_savings_pct: savings,
            routing_latency_us: t0.elapsed().as_micros() as u64,
        }
    }

    /// Sanitizes an arbitrary server or tool name into a valid TypeScript/JS identifier.
    pub fn sanitize_ident(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        for (i, ch) in raw.chars().enumerate() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                if i == 0 && ch.is_ascii_digit() {
                    out.push('_');
                }
                out.push(ch);
            } else {
                out.push('_');
            }
        }
        if out.is_empty() {
            "_tool".to_string()
        } else {
            out
        }
    }

    /// Converts a JSON Schema object into a compact inline TypeScript type string.
    pub fn json_schema_to_ts_type(schema: &JsonValue) -> String {
        let props = match schema.get("properties").and_then(|p| p.as_object()) {
            Some(p) if !p.is_empty() => p,
            _ => return "Record<string, any>".to_string(),
        };

        let required_set: Vec<&str> = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let mut fields = Vec::new();
        for (prop_name, prop_schema) in props {
            let prop_ident = Self::sanitize_ident(prop_name);
            let is_req = required_set.contains(&prop_name.as_str());
            let opt_mark = if is_req { "" } else { "?" };
            let ts_type = match prop_schema
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("any")
            {
                "string" => "string",
                "integer" | "number" => "number",
                "boolean" => "boolean",
                "array" => "any[]",
                "object" => "Record<string, any>",
                _ => "any",
            };
            fields.push(format!("{}{}: {}", prop_ident, opt_mark, ts_type));
        }

        format!("{{ {} }}", fields.join("; "))
    }
}

// ============================================================================
// Polyglot AST Sandbox (Code Mode JS/TS + Rust/Rhai)
// ============================================================================

#[derive(Clone, Debug)]
pub enum CodeModeError {
    SyntaxError(String),
    FuelExhausted(u64),
    CallDepthExceeded(usize),
    ExprDepthExceeded(usize),
    MemoryLimitExceeded(usize, usize),
    MaxHostCallsExceeded(usize),
    GlobalOutboundBlocked(String),
    ApprovalRequired(PendingApproval),
    RuntimeError(String),
}

impl CodeModeError {
    pub fn message(&self) -> String {
        match self {
            Self::SyntaxError(msg) => format!("SyntaxError: {}", msg),
            Self::FuelExhausted(max) => format!(
                "FuelExhausted: Script exceeded maximum allowed instruction limit ({} operations)",
                max
            ),
            Self::CallDepthExceeded(max) => {
                format!("CallDepthExceeded: Exceeded maximum stack depth ({})", max)
            }
            Self::ExprDepthExceeded(max) => format!(
                "ExprDepthExceeded: Exceeded maximum AST expression nesting depth ({})",
                max
            ),
            Self::MemoryLimitExceeded(used, max) => format!(
                "MemoryLimitExceeded: Script memory allocation ({} bytes) exceeded cap ({} bytes)",
                used, max
            ),
            Self::MaxHostCallsExceeded(max) => format!(
                "MaxHostCallsExceeded: Script exceeded maximum allowed downstream tool calls ({})",
                max
            ),
            Self::GlobalOutboundBlocked(api) => format!(
                "GlobalOutboundBlocked: Direct outbound/system access via '{}' is forbidden in Code Mode sandbox (globalOutbound: null). Use governed codemode.<server>.<tool>() bindings.",
                api
            ),
            Self::ApprovalRequired(p) => format!(
                "ApprovalRequired: Tool '{}.{}' is marked as a state-mutating operation and requires approval.",
                p.server_name, p.tool_name
            ),
            Self::RuntimeError(msg) => format!("RuntimeError: {}", msg),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    // Keywords
    Let,
    Mut,
    Const,
    Var,
    Async,
    Await,
    Function,
    For,
    In,
    Of,
    While,
    If,
    Else,
    Return,
    Typeof,
    // Delimiters
    LParen,
    RParen,
    LBrace,
    HashBrace, // #{
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    DoubleColon, // ::
    Semicolon,
    Dot,
    Question,
    Pipe,      // |
    Arrow,     // =>
    // Operators
    Assign,    // =
    PlusEq,    // +=
    MinusEq,   // -=
    StarEq,    // *=
    SlashEq,   // /=
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,      // == or ===
    NotEq,     // != or !==
    Lt,
    LtEq,
    Gt,
    GtEq,
    AndAnd,    // &&
    OrOr,      // ||
    NullCoalesce, // ??
    Bang,      // !
    Eof,
}

struct Lexer<'a> {
    chars: Vec<char>,
    pos: usize,
    _src: &'a str,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            chars: src.chars().collect(),
            pos: 0,
            _src: src,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.chars.get(self.pos).copied();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    fn tokenize(&mut self) -> Result<Vec<Token>, CodeModeError> {
        let mut tokens = Vec::new();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                self.advance();
                continue;
            }
            // Comments
            if ch == '/' && self.peek_next() == Some('/') {
                while let Some(c) = self.advance() {
                    if c == '\n' {
                        break;
                    }
                }
                continue;
            }
            if ch == '/' && self.peek_next() == Some('*') {
                self.advance();
                self.advance();
                while let Some(c) = self.advance() {
                    if c == '*' && self.peek() == Some('/') {
                        self.advance();
                        break;
                    }
                }
                continue;
            }

            // String literals
            if ch == '"' || ch == '\'' || ch == '`' {
                let quote = self.advance().unwrap();
                let mut s = String::new();
                while let Some(c) = self.advance() {
                    if c == quote {
                        break;
                    }
                    if c == '\\' {
                        if let Some(esc) = self.advance() {
                            match esc {
                                'n' => s.push('\n'),
                                'r' => s.push('\r'),
                                't' => s.push('\t'),
                                '\\' => s.push('\\'),
                                '"' => s.push('"'),
                                '\'' => s.push('\''),
                                other => s.push(other),
                            }
                        }
                    } else {
                        s.push(c);
                    }
                }
                tokens.push(Token::Str(s));
                continue;
            }

            // Numbers
            if ch.is_ascii_digit() {
                let mut num_str = String::new();
                let mut is_float = false;
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        num_str.push(c);
                        self.advance();
                    } else if c == '.' && !is_float && self.peek_next().map(|n| n.is_ascii_digit()).unwrap_or(false) {
                        is_float = true;
                        num_str.push(c);
                        self.advance();
                    } else if c == '_' {
                        self.advance();
                    } else {
                        break;
                    }
                }
                if is_float {
                    let f = num_str.parse::<f64>().map_err(|_| {
                        CodeModeError::SyntaxError(format!("Invalid float literal '{}'", num_str))
                    })?;
                    tokens.push(Token::Float(f));
                } else {
                    let i = num_str.parse::<i64>().map_err(|_| {
                        CodeModeError::SyntaxError(format!("Invalid integer literal '{}'", num_str))
                    })?;
                    tokens.push(Token::Int(i));
                }
                continue;
            }

            // Identifiers & Keywords
            if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
                let mut ident = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                        ident.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
                let tok = match ident.as_str() {
                    "let" => Token::Let,
                    "mut" => Token::Mut,
                    "const" => Token::Const,
                    "var" => Token::Var,
                    "async" => Token::Async,
                    "await" => Token::Await,
                    "function" | "fn" => Token::Function,
                    "for" => Token::For,
                    "in" => Token::In,
                    "of" => Token::Of,
                    "while" => Token::While,
                    "if" => Token::If,
                    "else" => Token::Else,
                    "return" => Token::Return,
                    "typeof" => Token::Typeof,
                    "true" => Token::Bool(true),
                    "false" => Token::Bool(false),
                    "null" | "undefined" => Token::Null,
                    _ => Token::Ident(ident),
                };
                tokens.push(tok);
                continue;
            }

            // Multi-char operators & punctuation
            match ch {
                '#' if self.peek_next() == Some('{') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::HashBrace);
                }
                ':' if self.peek_next() == Some(':') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::DoubleColon);
                }
                '=' if self.peek_next() == Some('>') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::Arrow);
                }
                '=' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                    }
                    tokens.push(Token::EqEq);
                }
                '!' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                    }
                    tokens.push(Token::NotEq);
                }
                '<' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::LtEq);
                }
                '>' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::GtEq);
                }
                '&' if self.peek_next() == Some('&') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::AndAnd);
                }
                '|' if self.peek_next() == Some('|') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::OrOr);
                }
                '?' if self.peek_next() == Some('?') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::NullCoalesce);
                }
                '+' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::PlusEq);
                }
                '-' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::MinusEq);
                }
                '*' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::StarEq);
                }
                '/' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::SlashEq);
                }
                '(' => {
                    self.advance();
                    tokens.push(Token::LParen);
                }
                ')' => {
                    self.advance();
                    tokens.push(Token::RParen);
                }
                '{' => {
                    self.advance();
                    tokens.push(Token::LBrace);
                }
                '}' => {
                    self.advance();
                    tokens.push(Token::RBrace);
                }
                '[' => {
                    self.advance();
                    tokens.push(Token::LBracket);
                }
                ']' => {
                    self.advance();
                    tokens.push(Token::RBracket);
                }
                ',' => {
                    self.advance();
                    tokens.push(Token::Comma);
                }
                ':' => {
                    self.advance();
                    tokens.push(Token::Colon);
                }
                ';' => {
                    self.advance();
                    tokens.push(Token::Semicolon);
                }
                '.' => {
                    self.advance();
                    tokens.push(Token::Dot);
                }
                '?' => {
                    self.advance();
                    tokens.push(Token::Question);
                }
                '|' => {
                    self.advance();
                    tokens.push(Token::Pipe);
                }
                '=' => {
                    self.advance();
                    tokens.push(Token::Assign);
                }
                '+' => {
                    self.advance();
                    tokens.push(Token::Plus);
                }
                '-' => {
                    self.advance();
                    tokens.push(Token::Minus);
                }
                '*' => {
                    self.advance();
                    tokens.push(Token::Star);
                }
                '/' => {
                    self.advance();
                    tokens.push(Token::Slash);
                }
                '%' => {
                    self.advance();
                    tokens.push(Token::Percent);
                }
                '<' => {
                    self.advance();
                    tokens.push(Token::Lt);
                }
                '>' => {
                    self.advance();
                    tokens.push(Token::Gt);
                }
                '!' => {
                    self.advance();
                    tokens.push(Token::Bang);
                }
                other => {
                    return Err(CodeModeError::SyntaxError(format!(
                        "Unexpected character '{}'",
                        other
                    )))
                }
            }
        }
        tokens.push(Token::Eof);
        Ok(tokens)
    }
}

#[derive(Clone, Debug)]
struct ParallelTask {
    target_var: Option<String>,
    server_name: String,
    tool_name: String,
    args: Vec<Expr>,
}

#[derive(Clone, Debug)]
enum Stmt {
    Let {
        name: String,
        expr: Expr,
    },
    Assign {
        target: AssignTarget,
        op: AssignOp,
        expr: Expr,
    },
    If {
        cond: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Option<Vec<Stmt>>,
    },
    For {
        var: String,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    ParallelWave(Vec<ParallelTask>),
    Return(Option<Expr>),
    Expr(Expr),
}

#[derive(Clone, Debug)]
enum AssignTarget {
    Var(String),
    Field(String, String),
}

#[derive(Clone, Debug, Copy)]
enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug)]
enum Expr {
    Literal(JsonValue),
    Var(String),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Namespace or global call: e.g. `uber_rides::list_recent(...)` or `fetch(...)`
    NamespaceCall {
        namespace: Option<String>,
        func: String,
        args: Vec<Expr>,
    },
    /// Method or chained call: e.g. `codemode.uber_rides.list_recent(...)` or `arr.filter(...)`
    MethodCall {
        receiver: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    /// Closure / Arrow function: `|x| expr` or `x => expr` or `(x, y) => { ... }`
    Closure {
        params: Vec<String>,
        body: ClosureBody,
    },
}

#[derive(Clone, Debug)]
enum ClosureBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

#[derive(Clone, Debug, Copy)]
enum UnaryOp {
    Not,
    Neg,
    Typeof,
}

#[derive(Clone, Debug, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    NullCoalesce,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    max_expr_depth: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>, max_expr_depth: usize) -> Self {
        Self {
            tokens,
            pos: 0,
            max_expr_depth,
        }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn peek_at(&self, offset: usize) -> &Token {
        self.tokens.get(self.pos + offset).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.peek().clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), CodeModeError> {
        let actual = self.advance();
        if &actual == expected {
            Ok(())
        } else {
            Err(CodeModeError::SyntaxError(format!(
                "Expected {:?}, found {:?}",
                expected, actual
            )))
        }
    }

    fn consume_optional_semicolons(&mut self) {
        while self.peek() == &Token::Semicolon {
            self.advance();
        }
    }

    fn parse_program(&mut self) -> Result<Vec<Stmt>, CodeModeError> {
        self.consume_optional_semicolons();

        // Unwrap top-level Code Mode wrapper if present:
        // e.g. `async () => { ... }`, `() => { ... }`, `async function run() { ... }`
        if self.peek() == &Token::Async {
            self.advance();
        }
        if self.peek() == &Token::Function {
            self.advance();
            if let Token::Ident(_) = self.peek() {
                self.advance();
            }
            if self.peek() == &Token::LParen {
                self.advance();
                while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
                    self.advance();
                }
                let _ = self.expect(&Token::RParen);
            }
            if self.peek() == &Token::LBrace {
                return self.parse_block();
            }
        } else if self.peek() == &Token::LParen
            && self.peek_at(1) == &Token::RParen
            && self.peek_at(2) == &Token::Arrow
        {
            self.advance(); // (
            self.advance(); // )
            self.advance(); // =>
            if self.peek() == &Token::LBrace {
                let stmts = self.parse_block()?;
                self.consume_optional_semicolons();
                // Allow trailing `()` if wrapped like `(async () => { ... })()`
                if self.peek() == &Token::LParen && self.peek_at(1) == &Token::RParen {
                    self.advance();
                    self.advance();
                }
                return Ok(stmts);
            } else {
                let expr = self.parse_expr(0)?;
                return Ok(vec![Stmt::Return(Some(expr))]);
            }
        }

        let mut stmts = Vec::new();
        while self.peek() != &Token::Eof && self.peek() != &Token::RBrace {
            stmts.push(self.parse_stmt()?);
            self.consume_optional_semicolons();
        }
        Ok(stmts)
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, CodeModeError> {
        self.expect(&Token::LBrace)?;
        self.consume_optional_semicolons();
        let mut stmts = Vec::new();
        while self.peek() != &Token::RBrace && self.peek() != &Token::Eof {
            stmts.push(self.parse_stmt()?);
            self.consume_optional_semicolons();
        }
        self.expect(&Token::RBrace)?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, CodeModeError> {
        match self.peek() {
            Token::Let | Token::Const | Token::Var => {
                self.advance();
                if self.peek() == &Token::Mut {
                    self.advance();
                }
                let name = match self.advance() {
                    Token::Ident(s) => s,
                    other => {
                        return Err(CodeModeError::SyntaxError(format!(
                            "Expected variable name after let/const, got {:?}",
                            other
                        )))
                    }
                };
                // Optional TypeScript type annotation: `let x: number = ...`
                if self.peek() == &Token::Colon {
                    self.advance();
                    if let Token::Ident(_) = self.peek() {
                        self.advance();
                    }
                }
                let expr = if self.peek() == &Token::Assign {
                    self.advance();
                    self.parse_expr(0)?
                } else {
                    Expr::Literal(JsonValue::Null)
                };
                self.consume_optional_semicolons();
                Ok(Stmt::Let { name, expr })
            }
            Token::If => {
                self.advance();
                let cond = self.parse_expr(0)?;
                let then_branch = self.parse_block()?;
                let else_branch = if self.peek() == &Token::Else {
                    self.advance();
                    if self.peek() == &Token::If {
                        Some(vec![self.parse_stmt()?])
                    } else {
                        Some(self.parse_block()?)
                    }
                } else {
                    None
                };
                Ok(Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                })
            }
            Token::For => {
                self.advance();
                let has_paren = if self.peek() == &Token::LParen {
                    self.advance();
                    true
                } else {
                    false
                };
                if matches!(self.peek(), Token::Let | Token::Const | Token::Var) {
                    self.advance();
                }
                if self.peek() == &Token::Mut {
                    self.advance();
                }
                let var = match self.advance() {
                    Token::Ident(s) => s,
                    other => {
                        return Err(CodeModeError::SyntaxError(format!(
                            "Expected loop variable in 'for', got {:?}",
                            other
                        )))
                    }
                };
                match self.advance() {
                    Token::In | Token::Of => {}
                    other => {
                        return Err(CodeModeError::SyntaxError(format!(
                            "Expected 'in' or 'of' in for-loop, got {:?}",
                            other
                        )))
                    }
                }
                let iterable = self.parse_expr(0)?;
                if has_paren {
                    self.expect(&Token::RParen)?;
                }
                let body = self.parse_block()?;
                Ok(Stmt::For {
                    var,
                    iterable,
                    body,
                })
            }
            Token::While => {
                self.advance();
                let cond = self.parse_expr(0)?;
                let body = self.parse_block()?;
                Ok(Stmt::While { cond, body })
            }
            Token::Return => {
                self.advance();
                if matches!(self.peek(), Token::Semicolon | Token::RBrace | Token::Eof) {
                    self.consume_optional_semicolons();
                    Ok(Stmt::Return(None))
                } else {
                    let expr = self.parse_expr(0)?;
                    self.consume_optional_semicolons();
                    Ok(Stmt::Return(Some(expr)))
                }
            }
            _ => {
                // Check for assignment: `ident = expr` or `ident.field = expr`
                if let Token::Ident(name) = self.peek().clone() {
                    if let Some(op) = Self::token_to_assign_op(self.peek_at(1)) {
                        self.advance(); // ident
                        self.advance(); // op
                        let expr = self.parse_expr(0)?;
                        self.consume_optional_semicolons();
                        return Ok(Stmt::Assign {
                            target: AssignTarget::Var(name),
                            op,
                            expr,
                        });
                    }
                    if self.peek_at(1) == &Token::Dot {
                        if let Token::Ident(field) = self.peek_at(2).clone() {
                            if let Some(op) = Self::token_to_assign_op(self.peek_at(3)) {
                                self.advance(); // ident
                                self.advance(); // .
                                self.advance(); // field
                                self.advance(); // op
                                let expr = self.parse_expr(0)?;
                                self.consume_optional_semicolons();
                                return Ok(Stmt::Assign {
                                    target: AssignTarget::Field(name, field),
                                    op,
                                    expr,
                                });
                            }
                        }
                    }
                }
                let expr = self.parse_expr(0)?;
                self.consume_optional_semicolons();
                Ok(Stmt::Expr(expr))
            }
        }
    }

    fn token_to_assign_op(tok: &Token) -> Option<AssignOp> {
        match tok {
            Token::Assign => Some(AssignOp::Set),
            Token::PlusEq => Some(AssignOp::Add),
            Token::MinusEq => Some(AssignOp::Sub),
            Token::StarEq => Some(AssignOp::Mul),
            Token::SlashEq => Some(AssignOp::Div),
            _ => None,
        }
    }

    fn parse_expr(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        if depth > self.max_expr_depth {
            return Err(CodeModeError::ExprDepthExceeded(self.max_expr_depth));
        }
        self.parse_ternary(depth + 1)
    }

    fn parse_ternary(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let cond = self.parse_null_coalesce(depth)?;
        if self.peek() == &Token::Question {
            self.advance();
            let then_expr = self.parse_expr(depth + 1)?;
            self.expect(&Token::Colon)?;
            let else_expr = self.parse_expr(depth + 1)?;
            return Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_expr),
                Box::new(else_expr),
            ));
        }
        Ok(cond)
    }

    fn parse_null_coalesce(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_or(depth)?;
        while self.peek() == &Token::NullCoalesce {
            self.advance();
            let right = self.parse_or(depth)?;
            left = Expr::Binary(Box::new(left), BinOp::NullCoalesce, Box::new(right));
        }
        Ok(left)
    }

    fn parse_or(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_and(depth)?;
        while self.peek() == &Token::OrOr {
            self.advance();
            let right = self.parse_and(depth)?;
            left = Expr::Binary(Box::new(left), BinOp::Or, Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_equality(depth)?;
        while self.peek() == &Token::AndAnd {
            self.advance();
            let right = self.parse_equality(depth)?;
            left = Expr::Binary(Box::new(left), BinOp::And, Box::new(right));
        }
        Ok(left)
    }

    fn parse_equality(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_comparison(depth)?;
        loop {
            let op = match self.peek() {
                Token::EqEq => BinOp::Eq,
                Token::NotEq => BinOp::NotEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_comparison(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_additive(depth)?;
        loop {
            let op = match self.peek() {
                Token::Lt => BinOp::Lt,
                Token::LtEq => BinOp::LtEq,
                Token::Gt => BinOp::Gt,
                Token::GtEq => BinOp::GtEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_additive(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_multiplicative(depth)?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_unary(depth)?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Rem,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        if depth > self.max_expr_depth {
            return Err(CodeModeError::ExprDepthExceeded(self.max_expr_depth));
        }
        match self.peek() {
            Token::Await => {
                self.advance();
                self.parse_unary(depth + 1)
            }
            Token::Bang => {
                self.advance();
                let inner = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Not, Box::new(inner)))
            }
            Token::Minus => {
                self.advance();
                let inner = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(inner)))
            }
            Token::Typeof => {
                self.advance();
                let inner = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Typeof, Box::new(inner)))
            }
            _ => self.parse_postfix(depth + 1),
        }
    }

    fn parse_postfix(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut expr = self.parse_primary(depth)?;
        loop {
            match self.peek() {
                Token::Dot => {
                    self.advance();
                    let member = match self.advance() {
                        Token::Ident(s) => s,
                        other => {
                            return Err(CodeModeError::SyntaxError(format!(
                                "Expected property or method name after '.', got {:?}",
                                other
                            )))
                        }
                    };
                    if self.peek() == &Token::LParen {
                        let args = self.parse_arg_list(depth)?;
                        expr = Expr::MethodCall {
                            receiver: Box::new(expr),
                            method: member,
                            args,
                        };
                    } else {
                        expr = Expr::Field(Box::new(expr), member);
                    }
                }
                Token::LBracket => {
                    self.advance();
                    let idx = self.parse_expr(depth + 1)?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(idx));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_arg_list(&mut self, depth: usize) -> Result<Vec<Expr>, CodeModeError> {
        self.expect(&Token::LParen)?;
        let mut args = Vec::new();
        while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
            args.push(self.parse_expr(depth + 1)?);
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::RParen)?;
        Ok(args)
    }

    fn parse_closure_body(&mut self, depth: usize) -> Result<ClosureBody, CodeModeError> {
        if self.peek() == &Token::LBrace {
            let stmts = self.parse_block()?;
            Ok(ClosureBody::Block(stmts))
        } else {
            let expr = self.parse_expr(depth + 1)?;
            Ok(ClosureBody::Expr(Box::new(expr)))
        }
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        if depth > self.max_expr_depth {
            return Err(CodeModeError::ExprDepthExceeded(self.max_expr_depth));
        }

        // Rust/Rhai closure: `|x| expr` or `|a, b| expr`
        if self.peek() == &Token::Pipe {
            self.advance();
            let mut params = Vec::new();
            while self.peek() != &Token::Pipe && self.peek() != &Token::Eof {
                if let Token::Ident(p) = self.advance() {
                    params.push(p);
                }
                if self.peek() == &Token::Comma {
                    self.advance();
                }
            }
            self.expect(&Token::Pipe)?;
            let body = self.parse_closure_body(depth)?;
            return Ok(Expr::Closure { params, body });
        }

        // JS single-param arrow function: `x => expr`
        if let Token::Ident(param) = self.peek().clone() {
            if self.peek_at(1) == &Token::Arrow {
                self.advance(); // ident
                self.advance(); // =>
                let body = self.parse_closure_body(depth)?;
                return Ok(Expr::Closure {
                    params: vec![param],
                    body,
                });
            }
        }

        // Parenthesized expression OR JS arrow function `(a, b) => ...`
        if self.peek() == &Token::LParen {
            // Check if this is `(p1, p2) => ...`
            let saved_pos = self.pos;
            self.advance(); // (
            let mut params = Vec::new();
            let mut is_arrow = true;
            while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
                match self.advance() {
                    Token::Ident(p) => params.push(p),
                    _ => {
                        is_arrow = false;
                        break;
                    }
                }
                if self.peek() == &Token::Comma {
                    self.advance();
                } else if self.peek() != &Token::RParen {
                    is_arrow = false;
                    break;
                }
            }
            if is_arrow && self.peek() == &Token::RParen && self.peek_at(1) == &Token::Arrow {
                self.advance(); // )
                self.advance(); // =>
                let body = self.parse_closure_body(depth)?;
                return Ok(Expr::Closure { params, body });
            }
            // Otherwise restore and parse as parenthesized expression
            self.pos = saved_pos;
            self.advance(); // (
            let inner = self.parse_expr(depth + 1)?;
            self.expect(&Token::RParen)?;
            return Ok(inner);
        }

        match self.advance() {
            Token::Int(i) => Ok(Expr::Literal(JsonValue::from(i))),
            Token::Float(f) => Ok(Expr::Literal(JsonValue::from(f))),
            Token::Bool(b) => Ok(Expr::Literal(JsonValue::Bool(b))),
            Token::Str(s) => Ok(Expr::Literal(JsonValue::from(s))),
            Token::Null => Ok(Expr::Literal(JsonValue::Null)),
            Token::LBracket => {
                let mut items = Vec::new();
                while self.peek() != &Token::RBracket && self.peek() != &Token::Eof {
                    items.push(self.parse_expr(depth + 1)?);
                    if self.peek() == &Token::Comma {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect(&Token::RBracket)?;
                Ok(Expr::Array(items))
            }
            Token::LBrace | Token::HashBrace => {
                let mut pairs = Vec::new();
                while self.peek() != &Token::RBrace && self.peek() != &Token::Eof {
                    let key = match self.advance() {
                        Token::Ident(k) | Token::Str(k) => k,
                        other => {
                            return Err(CodeModeError::SyntaxError(format!(
                                "Expected object key, got {:?}",
                                other
                            )))
                        }
                    };
                    let val = if self.peek() == &Token::Colon {
                        self.advance();
                        self.parse_expr(depth + 1)?
                    } else {
                        // ES6 object property shorthand `{ count, total }`
                        Expr::Var(key.clone())
                    };
                    pairs.push((key, val));
                    if self.peek() == &Token::Comma {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect(&Token::RBrace)?;
                Ok(Expr::Object(pairs))
            }
            Token::Ident(name) => {
                // Namespace call `service::tool(...)`
                if self.peek() == &Token::DoubleColon {
                    self.advance();
                    let func = match self.advance() {
                        Token::Ident(f) => f,
                        other => {
                            return Err(CodeModeError::SyntaxError(format!(
                                "Expected function name after '::', got {:?}",
                                other
                            )))
                        }
                    };
                    let args = self.parse_arg_list(depth)?;
                    Ok(Expr::NamespaceCall {
                        namespace: Some(name),
                        func,
                        args,
                    })
                } else if self.peek() == &Token::LParen {
                    let args = self.parse_arg_list(depth)?;
                    Ok(Expr::NamespaceCall {
                        namespace: None,
                        func: name,
                        args,
                    })
                } else {
                    Ok(Expr::Var(name))
                }
            }
            other => Err(CodeModeError::SyntaxError(format!(
                "Unexpected token in expression: {:?}",
                other
            ))),
        }
    }
}

// ============================================================================
// Static AST Auto-Parallelizer (SSA Dataflow Dependency Analyzer)
// ============================================================================

struct AstAutoParallelizer;

impl AstAutoParallelizer {
    fn optimize_block(stmts: Vec<Stmt>) -> Vec<Stmt> {
        let mut out = Vec::with_capacity(stmts.len());
        let mut pending_wave: Vec<ParallelTask> = Vec::new();
        let mut wave_written_vars: BTreeSet<String> = BTreeSet::new();

        let flush_wave = |wave: &mut Vec<ParallelTask>,
                          written: &mut BTreeSet<String>,
                          target: &mut Vec<Stmt>| {
            if wave.is_empty() {
                return;
            }
            if wave.len() == 1 {
                let task = wave.remove(0);
                let call_expr = Expr::MethodCall {
                    receiver: Box::new(Expr::Field(
                        Box::new(Expr::Var("codemode".to_string())),
                        task.server_name,
                    )),
                    method: task.tool_name,
                    args: task.args,
                };
                if let Some(var_name) = task.target_var {
                    target.push(Stmt::Let {
                        name: var_name,
                        expr: call_expr,
                    });
                } else {
                    target.push(Stmt::Expr(call_expr));
                }
            } else {
                target.push(Stmt::ParallelWave(std::mem::take(wave)));
            }
            written.clear();
        };

        for stmt in stmts {
            if let Some(task) = Self::extract_parallelizable_task(&stmt) {
                let mut read_vars = BTreeSet::new();
                for arg in &task.args {
                    Self::collect_read_vars(arg, &mut read_vars);
                }

                // A tool call can join the current parallel wave iff its inputs do NOT depend on
                // any variable written earlier in the current wave, and its target variable doesn't collide.
                let has_data_hazard = read_vars.iter().any(|rv| wave_written_vars.contains(rv))
                    || task
                        .target_var
                        .as_ref()
                        .map_or(false, |tv| wave_written_vars.contains(tv));

                if has_data_hazard {
                    flush_wave(&mut pending_wave, &mut wave_written_vars, &mut out);
                }

                if let Some(ref tv) = task.target_var {
                    wave_written_vars.insert(tv.clone());
                }
                pending_wave.push(task);
                continue;
            }

            flush_wave(&mut pending_wave, &mut wave_written_vars, &mut out);

            let optimized_stmt = match stmt {
                Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                } => Stmt::If {
                    cond,
                    then_branch: Self::optimize_block(then_branch),
                    else_branch: else_branch.map(Self::optimize_block),
                },
                Stmt::For {
                    var,
                    iterable,
                    body,
                } => Stmt::For {
                    var,
                    iterable,
                    body: Self::optimize_block(body),
                },
                Stmt::While { cond, body } => Stmt::While {
                    cond,
                    body: Self::optimize_block(body),
                },
                other => other,
            };
            out.push(optimized_stmt);
        }

        flush_wave(&mut pending_wave, &mut wave_written_vars, &mut out);
        out
    }

    fn extract_parallelizable_task(stmt: &Stmt) -> Option<ParallelTask> {
        match stmt {
            Stmt::Let { name, expr } => {
                let (srv, tool, args) = Self::extract_direct_tool_call(expr)?;
                Some(ParallelTask {
                    target_var: Some(name.clone()),
                    server_name: srv,
                    tool_name: tool,
                    args,
                })
            }
            Stmt::Expr(expr) => {
                let (srv, tool, args) = Self::extract_direct_tool_call(expr)?;
                Some(ParallelTask {
                    target_var: None,
                    server_name: srv,
                    tool_name: tool,
                    args,
                })
            }
            _ => None,
        }
    }

    fn extract_direct_tool_call(expr: &Expr) -> Option<(String, String, Vec<Expr>)> {
        match expr {
            Expr::MethodCall {
                receiver,
                method,
                args,
            } => {
                if let Expr::Field(inner_obj, server_name) = receiver.as_ref() {
                    if let Expr::Var(ns) = inner_obj.as_ref() {
                        if ns == "codemode" {
                            // Do not coalesce nested expressions that themselves contain tool calls
                            if args.iter().all(|a| !Self::contains_tool_call(a)) {
                                return Some((server_name.clone(), method.clone(), args.clone()));
                            }
                        }
                    }
                }
                None
            }
            Expr::NamespaceCall {
                namespace: Some(srv),
                func,
                args,
            } => {
                if !matches!(
                    srv.as_str(),
                    "mcp" | "std" | "fs" | "net" | "process" | "http" | "https"
                ) && args.iter().all(|a| !Self::contains_tool_call(a))
                {
                    return Some((srv.clone(), func.clone(), args.clone()));
                }
                None
            }
            _ => None,
        }
    }

    fn contains_tool_call(expr: &Expr) -> bool {
        match expr {
            Expr::NamespaceCall { .. } => true,
            Expr::MethodCall { receiver, args, .. } => {
                if let Expr::Field(inner, _) = receiver.as_ref() {
                    if let Expr::Var(ns) = inner.as_ref() {
                        if ns == "codemode" {
                            return true;
                        }
                    }
                }
                Self::contains_tool_call(receiver) || args.iter().any(Self::contains_tool_call)
            }
            Expr::Array(items) => items.iter().any(Self::contains_tool_call),
            Expr::Object(pairs) => pairs.iter().any(|(_, v)| Self::contains_tool_call(v)),
            Expr::Field(inner, _) | Expr::Unary(_, inner) => Self::contains_tool_call(inner),
            Expr::Index(a, b) | Expr::Binary(a, _, b) => {
                Self::contains_tool_call(a) || Self::contains_tool_call(b)
            }
            Expr::Ternary(a, b, c) => {
                Self::contains_tool_call(a)
                    || Self::contains_tool_call(b)
                    || Self::contains_tool_call(c)
            }
            Expr::Literal(_) | Expr::Var(_) | Expr::Closure { .. } => false,
        }
    }

    fn collect_read_vars(expr: &Expr, out: &mut BTreeSet<String>) {
        match expr {
            Expr::Var(name) => {
                if name != "codemode" && name != "console" && name != "Object" && name != "JSON" {
                    out.insert(name.clone());
                }
            }
            Expr::Array(items) => {
                for i in items {
                    Self::collect_read_vars(i, out);
                }
            }
            Expr::Object(pairs) => {
                for (_, v) in pairs {
                    Self::collect_read_vars(v, out);
                }
            }
            Expr::Field(inner, _) | Expr::Unary(_, inner) => Self::collect_read_vars(inner, out),
            Expr::Index(a, b) | Expr::Binary(a, _, b) => {
                Self::collect_read_vars(a, out);
                Self::collect_read_vars(b, out);
            }
            Expr::Ternary(a, b, c) => {
                Self::collect_read_vars(a, out);
                Self::collect_read_vars(b, out);
                Self::collect_read_vars(c, out);
            }
            Expr::NamespaceCall { args, .. } => {
                for a in args {
                    Self::collect_read_vars(a, out);
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                Self::collect_read_vars(receiver, out);
                for a in args {
                    Self::collect_read_vars(a, out);
                }
            }
            Expr::Literal(_) | Expr::Closure { .. } => {}
        }
    }
}

// ============================================================================
// Zero-Copy Columnar RecordBatch & Vectorized PhysicalExpr Engine
// ============================================================================

#[derive(Clone, Debug)]
enum ColumnVector {
    Float64(Vec<f64>, Vec<bool>),
    Utf8(Vec<String>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
}

struct ColumnarRecordBatch<'a> {
    rows: &'a [JsonValue],
    num_rows: usize,
    columns: BTreeMap<String, ColumnVector>,
}

impl<'a> ColumnarRecordBatch<'a> {
    fn from_rows(rows: &'a [JsonValue], referenced_cols: &BTreeSet<String>) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        let num_rows = rows.len();
        let mut columns = BTreeMap::new();

        for col_name in referenced_cols {
            // Inspect first non-null value in this column to determine physical column storage type
            let mut first_val: Option<&JsonValue> = None;
            for r in rows {
                if let Some(v) = Self::extract_field(r, col_name) {
                    if !v.is_null() {
                        first_val = Some(v);
                        break;
                    }
                }
            }

            let col_vec = match first_val {
                Some(JsonValue::Int(_)) | Some(JsonValue::Float(_)) => {
                    let mut vals = Vec::with_capacity(num_rows);
                    let mut valid = Vec::with_capacity(num_rows);
                    for r in rows {
                        if let Some(v) = Self::extract_field(r, col_name).and_then(|x| x.as_f64()) {
                            vals.push(v);
                            valid.push(true);
                        } else {
                            vals.push(0.0);
                            valid.push(false);
                        }
                    }
                    ColumnVector::Float64(vals, valid)
                }
                Some(JsonValue::Bool(_)) => {
                    let mut vals = Vec::with_capacity(num_rows);
                    let mut valid = Vec::with_capacity(num_rows);
                    for r in rows {
                        if let Some(v) = Self::extract_field(r, col_name).and_then(|x| x.as_bool())
                        {
                            vals.push(v);
                            valid.push(true);
                        } else {
                            vals.push(false);
                            valid.push(false);
                        }
                    }
                    ColumnVector::Bool(vals, valid)
                }
                _ => {
                    let mut vals = Vec::with_capacity(num_rows);
                    let mut valid = Vec::with_capacity(num_rows);
                    for r in rows {
                        if let Some(v) = Self::extract_field(r, col_name).and_then(|x| x.as_str()) {
                            vals.push(v.to_string());
                            valid.push(true);
                        } else {
                            vals.push(String::new());
                            valid.push(false);
                        }
                    }
                    ColumnVector::Utf8(vals, valid)
                }
            };
            columns.insert(col_name.clone(), col_vec);
        }

        Some(Self {
            rows,
            num_rows,
            columns,
        })
    }

    #[inline]
    fn extract_field<'r>(row: &'r JsonValue, col: &str) -> Option<&'r JsonValue> {
        row.get(col)
            .or_else(|| row.get("data").and_then(|d| d.get(col)))
    }
}

#[derive(Clone, Debug)]
enum PhysicalExpr {
    Column(String),
    LiteralNum(f64),
    LiteralStr(String),
    LiteralBool(bool),
    Compare {
        col: String,
        op: BinOp,
        rhs_num: Option<f64>,
        rhs_str: Option<String>,
        rhs_bool: Option<bool>,
    },
    And(Box<PhysicalExpr>, Box<PhysicalExpr>),
    Or(Box<PhysicalExpr>, Box<PhysicalExpr>),
    Not(Box<PhysicalExpr>),
    Arithmetic {
        left: Box<PhysicalExpr>,
        op: BinOp,
        right: Box<PhysicalExpr>,
    },
    StructProjection(Vec<(String, PhysicalExpr)>),
}

impl PhysicalExpr {
    fn try_compile_closure(
        closure_expr: &Expr,
        env: &BTreeMap<String, JsonValue>,
    ) -> Option<(Self, BTreeSet<String>)> {
        let (params, body) = match closure_expr {
            Expr::Closure { params, body } if params.len() == 1 => (params, body),
            _ => return None,
        };
        let row_param = &params[0];
        let body_expr = match body {
            ClosureBody::Expr(e) => e.as_ref(),
            ClosureBody::Block(stmts) if stmts.len() == 1 => match &stmts[0] {
                Stmt::Return(Some(e)) | Stmt::Expr(e) => e,
                _ => return None,
            },
            _ => return None,
        };

        let mut cols = BTreeSet::new();
        let phys = Self::compile_expr(body_expr, row_param, env, &mut cols)?;
        Some((phys, cols))
    }

    fn compile_expr(
        expr: &Expr,
        row_param: &str,
        env: &BTreeMap<String, JsonValue>,
        cols: &mut BTreeSet<String>,
    ) -> Option<Self> {
        match expr {
            Expr::Field(target, col_name) => {
                if let Expr::Var(v) = target.as_ref() {
                    if v == row_param {
                        cols.insert(col_name.clone());
                        return Some(PhysicalExpr::Column(col_name.clone()));
                    }
                }
                None
            }
            Expr::Literal(JsonValue::Int(i)) => Some(PhysicalExpr::LiteralNum(*i as f64)),
            Expr::Literal(JsonValue::Float(f)) => Some(PhysicalExpr::LiteralNum(*f)),
            Expr::Literal(JsonValue::Str(s)) => Some(PhysicalExpr::LiteralStr(s.clone())),
            Expr::Literal(JsonValue::Bool(b)) => Some(PhysicalExpr::LiteralBool(*b)),
            Expr::Var(name) if name != row_param => match env.get(name)? {
                JsonValue::Int(i) => Some(PhysicalExpr::LiteralNum(*i as f64)),
                JsonValue::Float(f) => Some(PhysicalExpr::LiteralNum(*f)),
                JsonValue::Str(s) => Some(PhysicalExpr::LiteralStr(s.clone())),
                JsonValue::Bool(b) => Some(PhysicalExpr::LiteralBool(*b)),
                _ => None,
            },
            Expr::Unary(UnaryOp::Not, inner) => {
                let p = Self::compile_expr(inner, row_param, env, cols)?;
                Some(PhysicalExpr::Not(Box::new(p)))
            }
            Expr::Binary(left, op, right) => {
                if matches!(op, BinOp::And) {
                    let l = Self::compile_expr(left, row_param, env, cols)?;
                    let r = Self::compile_expr(right, row_param, env, cols)?;
                    return Some(PhysicalExpr::And(Box::new(l), Box::new(r)));
                }
                if matches!(op, BinOp::Or) {
                    let l = Self::compile_expr(left, row_param, env, cols)?;
                    let r = Self::compile_expr(right, row_param, env, cols)?;
                    return Some(PhysicalExpr::Or(Box::new(l), Box::new(r)));
                }
                if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) {
                    let l = Self::compile_expr(left, row_param, env, cols)?;
                    let r = Self::compile_expr(right, row_param, env, cols)?;
                    return Some(PhysicalExpr::Arithmetic {
                        left: Box::new(l),
                        op: *op,
                        right: Box::new(r),
                    });
                }
                if matches!(
                    op,
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
                ) {
                    let l_phys = Self::compile_expr(left, row_param, env, cols)?;
                    let r_phys = Self::compile_expr(right, row_param, env, cols)?;
                    if let PhysicalExpr::Column(col) = l_phys {
                        match r_phys {
                            PhysicalExpr::LiteralNum(n) => {
                                return Some(PhysicalExpr::Compare {
                                    col,
                                    op: *op,
                                    rhs_num: Some(n),
                                    rhs_str: None,
                                    rhs_bool: None,
                                })
                            }
                            PhysicalExpr::LiteralStr(s) => {
                                return Some(PhysicalExpr::Compare {
                                    col,
                                    op: *op,
                                    rhs_num: None,
                                    rhs_str: Some(s),
                                    rhs_bool: None,
                                })
                            }
                            PhysicalExpr::LiteralBool(b) => {
                                return Some(PhysicalExpr::Compare {
                                    col,
                                    op: *op,
                                    rhs_num: None,
                                    rhs_str: None,
                                    rhs_bool: Some(b),
                                })
                            }
                            _ => return None,
                        }
                    }
                }
                None
            }
            Expr::Object(pairs) => {
                let mut proj = Vec::with_capacity(pairs.len());
                for (k, v_expr) in pairs {
                    let p = Self::compile_expr(v_expr, row_param, env, cols)?;
                    proj.push((k.clone(), p));
                }
                Some(PhysicalExpr::StructProjection(proj))
            }
            _ => None,
        }
    }

    fn eval_filter_mask(&self, batch: &ColumnarRecordBatch<'_>) -> Option<Vec<bool>> {
        let n = batch.num_rows;
        match self {
            PhysicalExpr::Column(col) => match batch.columns.get(col)? {
                ColumnVector::Bool(vals, valid) => {
                    Some((0..n).map(|i| valid[i] && vals[i]).collect())
                }
                ColumnVector::Float64(vals, valid) => {
                    Some((0..n).map(|i| valid[i] && vals[i] != 0.0).collect())
                }
                ColumnVector::Utf8(vals, valid) => {
                    Some((0..n).map(|i| valid[i] && !vals[i].is_empty()).collect())
                }
            },
            PhysicalExpr::Compare {
                col,
                op,
                rhs_num,
                rhs_str,
                rhs_bool,
            } => {
                let col_vec = batch.columns.get(col)?;
                match (col_vec, rhs_num, rhs_str, rhs_bool) {
                    (ColumnVector::Float64(vals, valid), Some(target), _, _) => {
                        let t = *target;
                        let mask: Vec<bool> = match op {
                            BinOp::Eq => (0..n).map(|i| valid[i] && (vals[i] - t).abs() < 1e-9).collect(),
                            BinOp::NotEq => (0..n).map(|i| !valid[i] || (vals[i] - t).abs() >= 1e-9).collect(),
                            BinOp::Lt => (0..n).map(|i| valid[i] && vals[i] < t).collect(),
                            BinOp::LtEq => (0..n).map(|i| valid[i] && vals[i] <= t).collect(),
                            BinOp::Gt => (0..n).map(|i| valid[i] && vals[i] > t).collect(),
                            BinOp::GtEq => (0..n).map(|i| valid[i] && vals[i] >= t).collect(),
                            _ => return None,
                        };
                        Some(mask)
                    }
                    (ColumnVector::Utf8(vals, valid), _, Some(target), _) => {
                        let mask: Vec<bool> = match op {
                            BinOp::Eq => (0..n).map(|i| valid[i] && &vals[i] == target).collect(),
                            BinOp::NotEq => (0..n).map(|i| !valid[i] || &vals[i] != target).collect(),
                            BinOp::Lt => (0..n).map(|i| valid[i] && &vals[i] < target).collect(),
                            BinOp::LtEq => (0..n).map(|i| valid[i] && &vals[i] <= target).collect(),
                            BinOp::Gt => (0..n).map(|i| valid[i] && &vals[i] > target).collect(),
                            BinOp::GtEq => (0..n).map(|i| valid[i] && &vals[i] >= target).collect(),
                            _ => return None,
                        };
                        Some(mask)
                    }
                    (ColumnVector::Bool(vals, valid), _, _, Some(target)) => {
                        let t = *target;
                        let mask: Vec<bool> = match op {
                            BinOp::Eq => (0..n).map(|i| valid[i] && vals[i] == t).collect(),
                            BinOp::NotEq => (0..n).map(|i| !valid[i] || vals[i] != t).collect(),
                            _ => return None,
                        };
                        Some(mask)
                    }
                    _ => None,
                }
            }
            PhysicalExpr::And(left, right) => {
                let l_mask = left.eval_filter_mask(batch)?;
                let r_mask = right.eval_filter_mask(batch)?;
                Some((0..n).map(|i| l_mask[i] && r_mask[i]).collect())
            }
            PhysicalExpr::Or(left, right) => {
                let l_mask = left.eval_filter_mask(batch)?;
                let r_mask = right.eval_filter_mask(batch)?;
                Some((0..n).map(|i| l_mask[i] || r_mask[i]).collect())
            }
            PhysicalExpr::Not(inner) => {
                let m = inner.eval_filter_mask(batch)?;
                Some(m.into_iter().map(|b| !b).collect())
            }
            _ => None,
        }
    }

    fn eval_scalar_at(&self, batch: &ColumnarRecordBatch<'_>, row_idx: usize) -> Option<JsonValue> {
        match self {
            PhysicalExpr::Column(col) => {
                ColumnarRecordBatch::extract_field(&batch.rows[row_idx], col).cloned()
            }
            PhysicalExpr::LiteralNum(n) => {
                if n.fract() == 0.0 {
                    Some(JsonValue::from(*n as i64))
                } else {
                    Some(JsonValue::from(*n))
                }
            }
            PhysicalExpr::LiteralStr(s) => Some(JsonValue::from(s.clone())),
            PhysicalExpr::LiteralBool(b) => Some(JsonValue::Bool(*b)),
            PhysicalExpr::Arithmetic { left, op, right } => {
                let lv = left.eval_scalar_at(batch, row_idx)?;
                let rv = right.eval_scalar_at(batch, row_idx)?;
                VmState::eval_bin_op(&lv, *op, &rv).ok()
            }
            PhysicalExpr::StructProjection(fields) => {
                let mut obj = JsonValue::object();
                for (k, f_expr) in fields {
                    let val = f_expr.eval_scalar_at(batch, row_idx).unwrap_or(JsonValue::Null);
                    obj.insert(k.clone(), val);
                }
                Some(obj)
            }
            _ => None,
        }
    }

    fn eval_projection_columnar(&self, batch: &ColumnarRecordBatch<'_>) -> Option<Vec<JsonValue>> {
        let mut out = Vec::with_capacity(batch.num_rows);
        for i in 0..batch.num_rows {
            out.push(self.eval_scalar_at(batch, i)?);
        }
        Some(out)
    }
}

/// Callbacks provided by the OmniMCP host (`MCPProxyGateway`) into the sandbox.
pub struct HostCallbacks<'a> {
    /// Invokes `(server_name, tool_name, args_vec) -> Result<JsonValue, CodeModeError>`
    pub invoke_tool: &'a mut dyn FnMut(&str, &str, Vec<JsonValue>) -> Result<JsonValue, CodeModeError>,
    /// Executes a parallel wave of independent tool calls concurrently via scoped threads or batching.
    pub invoke_parallel_batch: &'a mut dyn FnMut(
        Vec<(String, String, Vec<JsonValue>)>,
    ) -> Result<Vec<JsonValue>, CodeModeError>,
    /// Executes `codemode.search(query)` -> `JsonValue::Array`
    pub search_catalog: &'a dyn Fn(&str) -> JsonValue,
    /// Executes `codemode.describe(server, Option<tool>)` -> `String`
    pub describe_types: &'a dyn Fn(&str, Option<&str>) -> String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlFlow {
    None,
    Return,
}

/// Evaluates parsed AST scripts with fuel metering, memory caps, vectorized pushdown,
/// SSA auto-parallelization, stateful suspend/resume, and console log interception.
pub struct CodeModeExecutor {
    pub limits: SandboxLimits,
}

impl CodeModeExecutor {
    pub fn new(limits: SandboxLimits) -> Self {
        Self { limits }
    }

    pub fn execute(
        &self,
        code: &str,
        host: &mut HostCallbacks<'_>,
        traceparent: &str,
    ) -> CodeOutput {
        let t_boot = Instant::now();
        let exec_ulid = generate_ulid();
        let mut lexer = Lexer::new(code);
        let tokens = match lexer.tokenize() {
            Ok(t) => t,
            Err(e) => {
                return CodeOutput {
                    status: "ERROR".to_string(),
                    result: JsonValue::Null,
                    logs: Vec::new(),
                    pending_approvals: Vec::new(),
                    error: Some(e.message()),
                    boot_parse_us: t_boot.elapsed().as_micros() as u64,
                    eval_us: 0,
                    ops_consumed: 0,
                    peak_memory_bytes: 0,
                    tool_calls: 0,
                    intermediate_bytes_processed: 0,
                    output_bytes_returned: 0,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(exec_ulid),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: 0,
                    rows_scanned_vectorized: 0,
                    auto_parallel_waves: 0,
                    parallelized_tool_calls: 0,
                };
            }
        };

        let mut parser = Parser::new(tokens, self.limits.max_expr_depth);
        let raw_stmts = match parser.parse_program() {
            Ok(s) => s,
            Err(e) => {
                return CodeOutput {
                    status: "ERROR".to_string(),
                    result: JsonValue::Null,
                    logs: Vec::new(),
                    pending_approvals: Vec::new(),
                    error: Some(e.message()),
                    boot_parse_us: t_boot.elapsed().as_micros() as u64,
                    eval_us: 0,
                    ops_consumed: 0,
                    peak_memory_bytes: 0,
                    tool_calls: 0,
                    intermediate_bytes_processed: 0,
                    output_bytes_returned: 0,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(exec_ulid),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: 0,
                    rows_scanned_vectorized: 0,
                    auto_parallel_waves: 0,
                    parallelized_tool_calls: 0,
                };
            }
        };

        // Run Static Single Assignment (SSA) dependency analysis pass to lift sequential independent
        // tool calls into concurrent `Stmt::ParallelWave` batches.
        let stmts = AstAutoParallelizer::optimize_block(raw_stmts);
        let boot_parse_us = t_boot.elapsed().as_micros() as u64;

        let t_eval = Instant::now();
        let mut vm = VmState {
            env: BTreeMap::new(),
            limits: &self.limits,
            ops_consumed: 0,
            call_depth: 0,
            peak_memory_bytes: 0,
            tool_calls: 0,
            intermediate_bytes_processed: 0,
            logs: Vec::new(),
            current_top_stmt_idx: 0,
            current_target_var: None,
            is_top_level: true,
            vectorized_pushdowns: 0,
            rows_scanned_vectorized: 0,
            auto_parallel_waves: 0,
            parallelized_tool_calls: 0,
        };

        match vm.eval_block(&stmts, host) {
            Ok((val, _cf)) => {
                let eval_us = t_eval.elapsed().as_micros() as u64;
                let output_bytes = val.to_string_compact().len();
                CodeOutput {
                    status: "OK".to_string(),
                    result: val,
                    logs: vm.logs,
                    pending_approvals: Vec::new(),
                    error: None,
                    boot_parse_us,
                    eval_us,
                    ops_consumed: vm.ops_consumed,
                    peak_memory_bytes: vm.peak_memory_bytes,
                    tool_calls: vm.tool_calls,
                    intermediate_bytes_processed: vm.intermediate_bytes_processed,
                    output_bytes_returned: output_bytes,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(exec_ulid),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: vm.vectorized_pushdowns,
                    rows_scanned_vectorized: vm.rows_scanned_vectorized,
                    auto_parallel_waves: vm.auto_parallel_waves,
                    parallelized_tool_calls: vm.parallelized_tool_calls,
                }
            }
            Err(CodeModeError::ApprovalRequired(pending)) => {
                let eval_us = t_eval.elapsed().as_micros() as u64;
                let msg = pending.reason.clone();
                let mut scope_obj = JsonValue::object();
                for (k, v) in &vm.env {
                    scope_obj.insert(k.clone(), v.clone());
                }
                let frame = SuspendedExecutionFrame {
                    execution_id: exec_ulid.clone(),
                    script_hash: sha256_hex(code.as_bytes()),
                    script_source: code.to_string(),
                    instruction_ptr: vm.current_top_stmt_idx,
                    target_var: vm.current_target_var.clone(),
                    serialized_scope: scope_obj,
                    pending_server: pending.server_name.clone(),
                    pending_tool: pending.tool_name.clone(),
                    pending_arguments: pending.arguments.clone(),
                    pending_reason: pending.reason.clone(),
                    policy_context_hash: sha256_hex(traceparent.as_bytes()),
                    caller_actor: "agent".to_string(),
                    logs: vm.logs.clone(),
                    completed_mutations: Vec::new(),
                    created_at: now_epoch_secs(),
                };
                CodeOutput {
                    status: "APPROVAL_REQUIRED".to_string(),
                    result: JsonValue::Null,
                    logs: vm.logs,
                    pending_approvals: vec![pending],
                    error: Some(msg),
                    boot_parse_us,
                    eval_us,
                    ops_consumed: vm.ops_consumed,
                    peak_memory_bytes: vm.peak_memory_bytes,
                    tool_calls: vm.tool_calls,
                    intermediate_bytes_processed: vm.intermediate_bytes_processed,
                    output_bytes_returned: 0,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(exec_ulid),
                    suspended_frame: Some(frame),
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: vm.vectorized_pushdowns,
                    rows_scanned_vectorized: vm.rows_scanned_vectorized,
                    auto_parallel_waves: vm.auto_parallel_waves,
                    parallelized_tool_calls: vm.parallelized_tool_calls,
                }
            }
            Err(e) => {
                let eval_us = t_eval.elapsed().as_micros() as u64;
                CodeOutput {
                    status: "ERROR".to_string(),
                    result: JsonValue::Null,
                    logs: vm.logs,
                    pending_approvals: Vec::new(),
                    error: Some(e.message()),
                    boot_parse_us,
                    eval_us,
                    ops_consumed: vm.ops_consumed,
                    peak_memory_bytes: vm.peak_memory_bytes,
                    tool_calls: vm.tool_calls,
                    intermediate_bytes_processed: vm.intermediate_bytes_processed,
                    output_bytes_returned: 0,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(exec_ulid),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: vm.vectorized_pushdowns,
                    rows_scanned_vectorized: vm.rows_scanned_vectorized,
                    auto_parallel_waves: vm.auto_parallel_waves,
                    parallelized_tool_calls: vm.parallelized_tool_calls,
                }
            }
        }
    }

    /// Resumes a suspended execution frame from `instruction_ptr + 1` after human approval,
    /// without re-running any upstream statements before `instruction_ptr`.
    pub fn resume_frame(
        &self,
        frame: &SuspendedExecutionFrame,
        approved_mutation_output: JsonValue,
        host: &mut HostCallbacks<'_>,
        traceparent: &str,
    ) -> CodeOutput {
        let t_boot = Instant::now();
        let actual_hash = sha256_hex(frame.script_source.as_bytes());
        if actual_hash != frame.script_hash {
            return CodeOutput {
                status: "ERROR".to_string(),
                result: JsonValue::Null,
                logs: frame.logs.clone(),
                pending_approvals: Vec::new(),
                error: Some(format!(
                    "IntegrityError: Script SHA-256 hash mismatch on resume (expected {}, got {})",
                    frame.script_hash, actual_hash
                )),
                boot_parse_us: t_boot.elapsed().as_micros() as u64,
                eval_us: 0,
                ops_consumed: 0,
                peak_memory_bytes: 0,
                tool_calls: 0,
                intermediate_bytes_processed: 0,
                output_bytes_returned: 0,
                traceparent: traceparent.to_string(),
                execution_id: Some(frame.execution_id.clone()),
                suspended_frame: None,
                saga_compensations: Vec::new(),
                vectorized_pushdowns: 0,
                rows_scanned_vectorized: 0,
                auto_parallel_waves: 0,
                parallelized_tool_calls: 0,
            };
        }

        let mut lexer = Lexer::new(&frame.script_source);
        let tokens = match lexer.tokenize() {
            Ok(t) => t,
            Err(e) => {
                return CodeOutput {
                    status: "ERROR".to_string(),
                    result: JsonValue::Null,
                    logs: frame.logs.clone(),
                    pending_approvals: Vec::new(),
                    error: Some(e.message()),
                    boot_parse_us: t_boot.elapsed().as_micros() as u64,
                    eval_us: 0,
                    ops_consumed: 0,
                    peak_memory_bytes: 0,
                    tool_calls: 0,
                    intermediate_bytes_processed: 0,
                    output_bytes_returned: 0,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(frame.execution_id.clone()),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: 0,
                    rows_scanned_vectorized: 0,
                    auto_parallel_waves: 0,
                    parallelized_tool_calls: 0,
                };
            }
        };

        let mut parser = Parser::new(tokens, self.limits.max_expr_depth);
        let raw_stmts = match parser.parse_program() {
            Ok(s) => s,
            Err(e) => {
                return CodeOutput {
                    status: "ERROR".to_string(),
                    result: JsonValue::Null,
                    logs: frame.logs.clone(),
                    pending_approvals: Vec::new(),
                    error: Some(e.message()),
                    boot_parse_us: t_boot.elapsed().as_micros() as u64,
                    eval_us: 0,
                    ops_consumed: 0,
                    peak_memory_bytes: 0,
                    tool_calls: 0,
                    intermediate_bytes_processed: 0,
                    output_bytes_returned: 0,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(frame.execution_id.clone()),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: 0,
                    rows_scanned_vectorized: 0,
                    auto_parallel_waves: 0,
                    parallelized_tool_calls: 0,
                };
            }
        };

        let stmts = AstAutoParallelizer::optimize_block(raw_stmts);
        let boot_parse_us = t_boot.elapsed().as_micros() as u64;

        let t_eval = Instant::now();
        let mut env = BTreeMap::new();
        if let Some(entries) = frame.serialized_scope.as_object() {
            for (k, v) in entries {
                env.insert(k.clone(), v.clone());
            }
        }
        // Bind the approved mutation result to the target variable of the suspended statement
        if let Some(ref tv) = frame.target_var {
            env.insert(tv.clone(), approved_mutation_output.clone());
        } else if let Some(Stmt::Let { name, .. }) = stmts.get(frame.instruction_ptr) {
            env.insert(name.clone(), approved_mutation_output.clone());
        }

        let remaining_stmts = if frame.instruction_ptr + 1 <= stmts.len() {
            &stmts[frame.instruction_ptr + 1..]
        } else {
            &[]
        };

        let mut vm = VmState {
            env,
            limits: &self.limits,
            ops_consumed: 1,
            call_depth: 0,
            peak_memory_bytes: 0,
            tool_calls: 1, // Count the approved mutation execution
            intermediate_bytes_processed: approved_mutation_output.to_string_compact().len(),
            logs: frame.logs.clone(),
            current_top_stmt_idx: frame.instruction_ptr + 1,
            current_target_var: None,
            is_top_level: true,
            vectorized_pushdowns: 0,
            rows_scanned_vectorized: 0,
            auto_parallel_waves: 0,
            parallelized_tool_calls: 0,
        };

        let eval_res = if remaining_stmts.is_empty() {
            Ok((approved_mutation_output, ControlFlow::Return))
        } else {
            vm.eval_block(remaining_stmts, host)
        };

        match eval_res {
            Ok((val, _cf)) => {
                let eval_us = t_eval.elapsed().as_micros() as u64;
                let output_bytes = val.to_string_compact().len();
                CodeOutput {
                    status: "OK".to_string(),
                    result: val,
                    logs: vm.logs,
                    pending_approvals: Vec::new(),
                    error: None,
                    boot_parse_us,
                    eval_us,
                    ops_consumed: vm.ops_consumed,
                    peak_memory_bytes: vm.peak_memory_bytes,
                    tool_calls: vm.tool_calls,
                    intermediate_bytes_processed: vm.intermediate_bytes_processed,
                    output_bytes_returned: output_bytes,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(frame.execution_id.clone()),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: vm.vectorized_pushdowns,
                    rows_scanned_vectorized: vm.rows_scanned_vectorized,
                    auto_parallel_waves: vm.auto_parallel_waves,
                    parallelized_tool_calls: vm.parallelized_tool_calls,
                }
            }
            Err(e) => {
                let eval_us = t_eval.elapsed().as_micros() as u64;
                CodeOutput {
                    status: "ERROR".to_string(),
                    result: JsonValue::Null,
                    logs: vm.logs,
                    pending_approvals: Vec::new(),
                    error: Some(e.message()),
                    boot_parse_us,
                    eval_us,
                    ops_consumed: vm.ops_consumed,
                    peak_memory_bytes: vm.peak_memory_bytes,
                    tool_calls: vm.tool_calls,
                    intermediate_bytes_processed: vm.intermediate_bytes_processed,
                    output_bytes_returned: 0,
                    traceparent: traceparent.to_string(),
                    execution_id: Some(frame.execution_id.clone()),
                    suspended_frame: None,
                    saga_compensations: Vec::new(),
                    vectorized_pushdowns: vm.vectorized_pushdowns,
                    rows_scanned_vectorized: vm.rows_scanned_vectorized,
                    auto_parallel_waves: vm.auto_parallel_waves,
                    parallelized_tool_calls: vm.parallelized_tool_calls,
                }
            }
        }
    }
}

struct VmState<'a> {
    env: BTreeMap<String, JsonValue>,
    limits: &'a SandboxLimits,
    ops_consumed: u64,
    call_depth: usize,
    peak_memory_bytes: usize,
    tool_calls: usize,
    intermediate_bytes_processed: usize,
    logs: Vec<String>,
    current_top_stmt_idx: usize,
    current_target_var: Option<String>,
    is_top_level: bool,
    vectorized_pushdowns: usize,
    rows_scanned_vectorized: usize,
    auto_parallel_waves: usize,
    parallelized_tool_calls: usize,
}

impl<'a> VmState<'a> {
    #[inline]
    fn burn_fuel(&mut self, cost: u64) -> Result<(), CodeModeError> {
        self.ops_consumed += cost;
        if self.ops_consumed > self.limits.max_operations {
            return Err(CodeModeError::FuelExhausted(self.limits.max_operations));
        }
        Ok(())
    }

    fn check_memory(&mut self, additional_bytes: usize) -> Result<(), CodeModeError> {
        let current: usize = self
            .env
            .iter()
            .map(|(k, v)| k.len() + Self::estimate_val_bytes(v))
            .sum::<usize>()
            + additional_bytes;
        if current > self.peak_memory_bytes {
            self.peak_memory_bytes = current;
        }
        if current > self.limits.max_memory_bytes {
            return Err(CodeModeError::MemoryLimitExceeded(
                current,
                self.limits.max_memory_bytes,
            ));
        }
        Ok(())
    }

    fn estimate_val_bytes(v: &JsonValue) -> usize {
        match v {
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Int(_) | JsonValue::Float(_) => 16,
            JsonValue::Str(s) => 24 + s.len(),
            JsonValue::Array(arr) => 24 + arr.iter().map(Self::estimate_val_bytes).sum::<usize>(),
            JsonValue::Object(entries) => {
                32 + entries
                    .iter()
                    .map(|(k, val)| k.len() + Self::estimate_val_bytes(val))
                    .sum::<usize>()
            }
        }
    }

    fn eval_block(
        &mut self,
        stmts: &[Stmt],
        host: &mut HostCallbacks<'_>,
    ) -> Result<(JsonValue, ControlFlow), CodeModeError> {
        let was_top_level = self.is_top_level;
        let mut last_val = JsonValue::Null;
        for (idx, stmt) in stmts.iter().enumerate() {
            if was_top_level {
                self.current_top_stmt_idx = idx;
                self.current_target_var = None;
            }
            self.burn_fuel(1)?;
            match stmt {
                Stmt::Let { name, expr } => {
                    if was_top_level {
                        self.current_target_var = Some(name.clone());
                    }
                    let val = self.eval_expr(expr, host)?;
                    let val_bytes = Self::estimate_val_bytes(&val);
                    self.check_memory(val_bytes)?;
                    self.env.insert(name.clone(), val);
                    last_val = JsonValue::Null;
                }
                Stmt::Assign { target, op, expr } => {
                    if was_top_level {
                        if let AssignTarget::Var(ref name) = target {
                            self.current_target_var = Some(name.clone());
                        }
                    }
                    let rhs = self.eval_expr(expr, host)?;
                    match target {
                        AssignTarget::Var(name) => {
                            let cur = self.env.get(name).cloned().unwrap_or(JsonValue::Null);
                            let new_val = Self::apply_assign_op(&cur, *op, rhs)?;
                            let val_bytes = Self::estimate_val_bytes(&new_val);
                            self.check_memory(val_bytes)?;
                            self.env.insert(name.clone(), new_val);
                        }
                        AssignTarget::Field(var_name, field) => {
                            if let Some(obj) = self.env.get_mut(var_name) {
                                if obj.is_object() {
                                    let cur = obj.get(field).cloned().unwrap_or(JsonValue::Null);
                                    let new_val = Self::apply_assign_op(&cur, *op, rhs)?;
                                    obj.insert(field.clone(), new_val);
                                }
                            }
                        }
                    }
                    last_val = JsonValue::Null;
                }
                Stmt::ParallelWave(tasks) => {
                    self.tool_calls += tasks.len();
                    if self.tool_calls > self.limits.max_host_calls {
                        return Err(CodeModeError::MaxHostCallsExceeded(
                            self.limits.max_host_calls,
                        ));
                    }
                    self.burn_fuel((tasks.len() as u64) * 10)?;

                    let mut batch_requests = Vec::with_capacity(tasks.len());
                    for task in tasks {
                        let mut evaluated_args = Vec::with_capacity(task.args.len());
                        for a in &task.args {
                            evaluated_args.push(self.eval_expr(a, host)?);
                        }
                        batch_requests.push((
                            task.server_name.clone(),
                            task.tool_name.clone(),
                            evaluated_args,
                        ));
                    }

                    let results = (host.invoke_parallel_batch)(batch_requests)?;
                    self.auto_parallel_waves += 1;
                    self.parallelized_tool_calls += tasks.len();

                    for (task, resp) in tasks.iter().zip(results.into_iter()) {
                        let resp_bytes = resp.to_string_compact().len();
                        self.intermediate_bytes_processed += resp_bytes;
                        self.check_memory(resp_bytes)?;
                        if let Some(ref var_name) = task.target_var {
                            self.env.insert(var_name.clone(), resp.clone());
                        }
                        last_val = resp;
                    }
                }
                Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    let c = self.eval_expr(cond, host)?;
                    self.is_top_level = false;
                    let branch_res = if Self::is_truthy(&c) {
                        Some(self.eval_block(then_branch, host))
                    } else {
                        else_branch
                            .as_ref()
                            .map(|else_stmts| self.eval_block(else_stmts, host))
                    };
                    self.is_top_level = was_top_level;
                    if let Some(res) = branch_res {
                        let (v, cf) = res?;
                        if cf == ControlFlow::Return {
                            return Ok((v, cf));
                        }
                        last_val = v;
                    }
                }
                Stmt::For {
                    var,
                    iterable,
                    body,
                } => {
                    let iter_val = self.eval_expr(iterable, host)?;
                    if let JsonValue::Array(items) = iter_val {
                        self.is_top_level = false;
                        for item in items {
                            self.burn_fuel(1)?;
                            self.env.insert(var.clone(), item);
                            let (v, cf) = self.eval_block(body, host)?;
                            if cf == ControlFlow::Return {
                                self.is_top_level = was_top_level;
                                return Ok((v, cf));
                            }
                        }
                        self.is_top_level = was_top_level;
                    }
                    last_val = JsonValue::Null;
                }
                Stmt::While { cond, body } => {
                    self.is_top_level = false;
                    loop {
                        self.burn_fuel(1)?;
                        let c = self.eval_expr(cond, host)?;
                        if !Self::is_truthy(&c) {
                            break;
                        }
                        let (v, cf) = self.eval_block(body, host)?;
                        if cf == ControlFlow::Return {
                            self.is_top_level = was_top_level;
                            return Ok((v, cf));
                        }
                    }
                    self.is_top_level = was_top_level;
                    last_val = JsonValue::Null;
                }
                Stmt::Return(opt_expr) => {
                    let val = match opt_expr {
                        Some(e) => self.eval_expr(e, host)?,
                        None => JsonValue::Null,
                    };
                    return Ok((val, ControlFlow::Return));
                }
                Stmt::Expr(expr) => {
                    last_val = self.eval_expr(expr, host)?;
                }
            }
        }
        Ok((last_val, ControlFlow::None))
    }

    fn apply_assign_op(
        cur: &JsonValue,
        op: AssignOp,
        rhs: JsonValue,
    ) -> Result<JsonValue, CodeModeError> {
        match op {
            AssignOp::Set => Ok(rhs),
            AssignOp::Add => Self::eval_bin_op(cur, BinOp::Add, &rhs),
            AssignOp::Sub => Self::eval_bin_op(cur, BinOp::Sub, &rhs),
            AssignOp::Mul => Self::eval_bin_op(cur, BinOp::Mul, &rhs),
            AssignOp::Div => Self::eval_bin_op(cur, BinOp::Div, &rhs),
        }
    }

    fn eval_expr(
        &mut self,
        expr: &Expr,
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        self.burn_fuel(1)?;
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Var(name) => {
                // Block global outbound / system objects if referenced as standalone traps
                if matches!(name.as_str(), "process" | "fs" | "XMLHttpRequest") {
                    return Err(CodeModeError::GlobalOutboundBlocked(name.clone()));
                }
                Ok(self.env.get(name).cloned().unwrap_or(JsonValue::Null))
            }
            Expr::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval_expr(item, host)?);
                }
                let arr_val = JsonValue::Array(out);
                self.check_memory(Self::estimate_val_bytes(&arr_val))?;
                Ok(arr_val)
            }
            Expr::Object(pairs) => {
                let mut obj_val = JsonValue::object();
                for (k, v_expr) in pairs {
                    let val = self.eval_expr(v_expr, host)?;
                    obj_val.insert(k.clone(), val);
                }
                self.check_memory(Self::estimate_val_bytes(&obj_val))?;
                Ok(obj_val)
            }
            Expr::Field(target, field) => {
                let base = self.eval_expr(target, host)?;
                match base {
                    JsonValue::Object(_) => {
                        if let Some(v) = base.get(field) {
                            return Ok(v.clone());
                        }
                        // Support unwrapped `.data` access when downstream wraps payload in `{ data: ... }`
                        if let Some(data_obj) = base.get("data") {
                            if let Some(v) = data_obj.get(field) {
                                return Ok(v.clone());
                            }
                        }
                        Ok(JsonValue::Null)
                    }
                    JsonValue::Array(ref arr) if field == "length" => {
                        Ok(JsonValue::from(arr.len() as i64))
                    }
                    JsonValue::Str(ref s) if field == "length" => {
                        Ok(JsonValue::from(s.len() as i64))
                    }
                    _ => Ok(JsonValue::Null),
                }
            }
            Expr::Index(target, idx_expr) => {
                let base = self.eval_expr(target, host)?;
                let idx = self.eval_expr(idx_expr, host)?;
                if let Some(arr) = base.as_array() {
                    if let Some(i) = idx.as_u64() {
                        return Ok(arr.get(i as usize).cloned().unwrap_or(JsonValue::Null));
                    }
                }
                if base.is_object() {
                    if let Some(k) = idx.as_str() {
                        return Ok(base.get(k).cloned().unwrap_or(JsonValue::Null));
                    }
                }
                Ok(JsonValue::Null)
            }
            Expr::Unary(op, inner) => {
                let val = self.eval_expr(inner, host)?;
                match op {
                    UnaryOp::Not => Ok(JsonValue::Bool(!Self::is_truthy(&val))),
                    UnaryOp::Neg => match val {
                        JsonValue::Int(n) => Ok(JsonValue::Int(-n)),
                        JsonValue::Float(f) => Ok(JsonValue::Float(-f)),
                        _ => Ok(JsonValue::from(0)),
                    },
                    UnaryOp::Typeof => {
                        let t = match val {
                            JsonValue::Null => "object",
                            JsonValue::Bool(_) => "boolean",
                            JsonValue::Int(_) | JsonValue::Float(_) => "number",
                            JsonValue::Str(_) => "string",
                            JsonValue::Array(_) | JsonValue::Object(_) => "object",
                        };
                        Ok(JsonValue::from(t))
                    }
                }
            }
            Expr::Binary(left_expr, op, right_expr) => {
                if matches!(op, BinOp::And) {
                    let l = self.eval_expr(left_expr, host)?;
                    if !Self::is_truthy(&l) {
                        return Ok(JsonValue::Bool(false));
                    }
                    let r = self.eval_expr(right_expr, host)?;
                    return Ok(JsonValue::Bool(Self::is_truthy(&r)));
                }
                if matches!(op, BinOp::Or) {
                    let l = self.eval_expr(left_expr, host)?;
                    if Self::is_truthy(&l) {
                        return Ok(l);
                    }
                    return self.eval_expr(right_expr, host);
                }
                if matches!(op, BinOp::NullCoalesce) {
                    let l = self.eval_expr(left_expr, host)?;
                    if !l.is_null() {
                        return Ok(l);
                    }
                    return self.eval_expr(right_expr, host);
                }
                let l = self.eval_expr(left_expr, host)?;
                let r = self.eval_expr(right_expr, host)?;
                Self::eval_bin_op(&l, *op, &r)
            }
            Expr::Ternary(cond, then_e, else_e) => {
                let c = self.eval_expr(cond, host)?;
                if Self::is_truthy(&c) {
                    self.eval_expr(then_e, host)
                } else {
                    self.eval_expr(else_e, host)
                }
            }
            Expr::NamespaceCall {
                namespace,
                func,
                args,
            } => self.eval_namespace_call(namespace.as_deref(), func, args, host),
            Expr::MethodCall {
                receiver,
                method,
                args,
            } => self.eval_method_call(receiver, method, args, host),
            Expr::Closure { .. } => Ok(JsonValue::Null),
        }
    }

    fn eval_namespace_call(
        &mut self,
        namespace: Option<&str>,
        func: &str,
        args: &[Expr],
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        // 1. Check Zero-Outbound Isolation (`globalOutbound: null`)
        if namespace.is_none()
            && matches!(
                func,
                "fetch" | "require" | "import" | "eval" | "Function" | "XMLHttpRequest"
            )
        {
            return Err(CodeModeError::GlobalOutboundBlocked(func.to_string()));
        }
        if let Some(ns) = namespace {
            if matches!(ns, "std" | "fs" | "net" | "process" | "http" | "https") {
                return Err(CodeModeError::GlobalOutboundBlocked(format!(
                    "{}::{}",
                    ns, func
                )));
            }
        }

        let mut evaluated_args = Vec::with_capacity(args.len());
        for a in args {
            evaluated_args.push(self.eval_expr(a, host)?);
        }

        // 2. Built-in `print(...)`
        if namespace.is_none() && func == "print" {
            let line = Self::format_console_args(&evaluated_args);
            self.logs.push(format!("[LOG] {}", line));
            return Ok(JsonValue::Null);
        }

        // 3. Explicit `mcp::call("server", "tool", args)`
        if namespace == Some("mcp") && func == "call" {
            if evaluated_args.len() < 2 {
                return Err(CodeModeError::RuntimeError(
                    "mcp::call requires (server_name, tool_name, [arguments])".to_string(),
                ));
            }
            let srv = evaluated_args[0].as_str().unwrap_or("").to_string();
            let tl = evaluated_args[1].as_str().unwrap_or("").to_string();
            let rest = if evaluated_args.len() > 2 {
                evaluated_args[2..].to_vec()
            } else {
                Vec::new()
            };
            return self.dispatch_host_tool(&srv, &tl, rest, host);
        }

        // 4. `service::tool(...)` Rust/Rhai style host call
        if let Some(srv) = namespace {
            return self.dispatch_host_tool(srv, func, evaluated_args, host);
        }

        Err(CodeModeError::RuntimeError(format!(
            "Unknown function '{}'",
            func
        )))
    }

    fn eval_method_call(
        &mut self,
        receiver: &Expr,
        method: &str,
        args: &[Expr],
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        // 1. Check for `console.log`, `console.warn`, `console.error`, `console.info`
        if let Expr::Var(obj_name) = receiver {
            if obj_name == "console" {
                let mut evaluated = Vec::with_capacity(args.len());
                for a in args {
                    evaluated.push(self.eval_expr(a, host)?);
                }
                let text = Self::format_console_args(&evaluated);
                let prefix = match method {
                    "warn" => "[WARN]",
                    "error" => "[ERROR]",
                    _ => "[LOG]",
                };
                self.logs.push(format!("{} {}", prefix, text));
                return Ok(JsonValue::Null);
            }

            // 2. `Object.keys(x)`, `Object.values(x)`, `JSON.stringify(x)`, `JSON.parse(x)`
            if obj_name == "Object" {
                let first = if let Some(a0) = args.first() {
                    self.eval_expr(a0, host)?
                } else {
                    JsonValue::Null
                };
                if let JsonValue::Object(entries) = first {
                    match method {
                        "keys" => {
                            return Ok(JsonValue::Array(
                                entries
                                    .iter()
                                    .map(|(k, _)| JsonValue::from(k.as_str()))
                                    .collect(),
                            ))
                        }
                        "values" => {
                            return Ok(JsonValue::Array(
                                entries.into_iter().map(|(_, v)| v).collect(),
                            ))
                        }
                        _ => {}
                    }
                }
                return Ok(JsonValue::Array(Vec::new()));
            }
            if obj_name == "JSON" {
                let first = if let Some(a0) = args.first() {
                    self.eval_expr(a0, host)?
                } else {
                    JsonValue::Null
                };
                match method {
                    "stringify" => return Ok(JsonValue::from(first.to_string_compact())),
                    "parse" => {
                        if let Some(s) = first.as_str() {
                            return Ok(JsonValue::parse(s).unwrap_or(JsonValue::Null));
                        }
                    }
                    _ => {}
                }
            }

            // 3. `codemode.search(query)` and `codemode.describe(server, tool?)`
            if obj_name == "codemode" {
                let mut evaluated = Vec::with_capacity(args.len());
                for a in args {
                    evaluated.push(self.eval_expr(a, host)?);
                }
                match method {
                    "search" => {
                        let q = evaluated.first().and_then(|v| v.as_str()).unwrap_or("");
                        let res = (host.search_catalog)(q);
                        return Ok(res);
                    }
                    "describe" => {
                        let srv = evaluated.first().and_then(|v| v.as_str()).unwrap_or("");
                        let tl = evaluated.get(1).and_then(|v| v.as_str());
                        let dts = (host.describe_types)(srv, tl);
                        return Ok(JsonValue::from(dts));
                    }
                    _ => {}
                }
            }

            // 4. Mutating `.push(val)` on a variable array (`arr.push(x)`)
            if method == "push" {
                let mut pushed_vals = Vec::with_capacity(args.len());
                for a in args {
                    pushed_vals.push(self.eval_expr(a, host)?);
                }
                let mut new_len = 0i64;
                if let Some(JsonValue::Array(arr)) = self.env.get_mut(obj_name) {
                    for v in pushed_vals {
                        // Flatten if pushing an array in guidelines.push(rules) or push element
                        arr.push(v);
                    }
                    new_len = arr.len() as i64;
                }
                self.check_memory(0)?;
                return Ok(JsonValue::from(new_len));
            }
        }

        // 5. Code Mode `codemode.<server>.<tool>(args...)`
        if let Expr::Field(inner_obj, server_name) = receiver {
            if let Expr::Var(ns) = inner_obj.as_ref() {
                if ns == "codemode" {
                    let mut evaluated_args = Vec::with_capacity(args.len());
                    for a in args {
                        evaluated_args.push(self.eval_expr(a, host)?);
                    }
                    return self.dispatch_host_tool(server_name, method, evaluated_args, host);
                }
            }
        }

        // 6. Evaluate receiver value for in-memory Array / String / Object methods
        let target_val = self.eval_expr(receiver, host)?;

        match target_val {
            JsonValue::Array(arr) => self.eval_array_method(arr, method, args, host),
            JsonValue::Str(s) => self.eval_string_method(&s, method, args, host),
            JsonValue::Object(entries) => match method {
                "keys" => Ok(JsonValue::Array(
                    entries
                        .iter()
                        .map(|(k, _)| JsonValue::from(k.as_str()))
                        .collect(),
                )),
                "values" => Ok(JsonValue::Array(
                    entries.into_iter().map(|(_, v)| v).collect(),
                )),
                _ => Ok(JsonValue::Null),
            },
            _ => Ok(JsonValue::Null),
        }
    }

    fn eval_array_method(
        &mut self,
        mut arr: Vec<JsonValue>,
        method: &str,
        args: &[Expr],
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        match method {
            "len" | "length" | "count" => Ok(JsonValue::from(arr.len() as i64)),
            "is_empty" => Ok(JsonValue::Bool(arr.is_empty())),
            "slice" => {
                let start = if let Some(a0) = args.first() {
                    self.eval_expr(a0, host)?.as_u64().unwrap_or(0) as usize
                } else {
                    0
                };
                let end = if let Some(a1) = args.get(1) {
                    self.eval_expr(a1, host)?.as_u64().unwrap_or(arr.len() as u64) as usize
                } else {
                    arr.len()
                };
                let s = start.min(arr.len());
                let e = end.min(arr.len()).max(s);
                Ok(JsonValue::Array(arr[s..e].to_vec()))
            }
            "take" => {
                let n = if let Some(a0) = args.first() {
                    self.eval_expr(a0, host)?.as_u64().unwrap_or(0) as usize
                } else {
                    0
                };
                Ok(JsonValue::Array(arr.into_iter().take(n).collect()))
            }
            "includes" | "contains" => {
                let needle = if let Some(a0) = args.first() {
                    self.eval_expr(a0, host)?
                } else {
                    JsonValue::Null
                };
                let found = arr.iter().any(|x| Self::values_equal(x, &needle));
                Ok(JsonValue::Bool(found))
            }
            "join" => {
                let sep = if let Some(a0) = args.first() {
                    self.eval_expr(a0, host)?
                        .as_str()
                        .unwrap_or(",")
                        .to_string()
                } else {
                    ",".to_string()
                };
                let joined: Vec<String> = arr.iter().map(Self::val_to_display_string).collect();
                Ok(JsonValue::from(joined.join(&sep)))
            }
            "sum" => {
                let sum: f64 = arr.iter().filter_map(|x| x.as_f64()).sum();
                Ok(JsonValue::from(sum))
            }
            "avg" => {
                if arr.is_empty() {
                    Ok(JsonValue::from(0.0))
                } else {
                    let sum: f64 = arr.iter().filter_map(|x| x.as_f64()).sum();
                    Ok(JsonValue::from(sum / arr.len() as f64))
                }
            }
            "filter" => {
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(".filter() requires a closure argument".to_string())
                })?;
                if !arr.is_empty() {
                    if let Some((phys, cols)) = PhysicalExpr::try_compile_closure(closure, &self.env)
                    {
                        if let Some(batch) = ColumnarRecordBatch::from_rows(&arr, &cols) {
                            if let Some(mask) = phys.eval_filter_mask(&batch) {
                                self.burn_fuel(1)?;
                                self.vectorized_pushdowns += 1;
                                self.rows_scanned_vectorized += arr.len();
                                let out: Vec<JsonValue> = arr
                                    .into_iter()
                                    .zip(mask.into_iter())
                                    .filter_map(|(row, keep)| if keep { Some(row) } else { None })
                                    .collect();
                                return Ok(JsonValue::Array(out));
                            }
                        }
                    }
                }
                let mut out = Vec::new();
                for item in arr {
                    let keep = self.invoke_closure(closure, &[item.clone()], host)?;
                    if Self::is_truthy(&keep) {
                        out.push(item);
                    }
                }
                Ok(JsonValue::Array(out))
            }
            "map" => {
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(".map() requires a closure argument".to_string())
                })?;
                if !arr.is_empty() {
                    if let Some((phys, cols)) = PhysicalExpr::try_compile_closure(closure, &self.env)
                    {
                        if let Some(batch) = ColumnarRecordBatch::from_rows(&arr, &cols) {
                            if let Some(projected) = phys.eval_projection_columnar(&batch) {
                                self.burn_fuel(1)?;
                                self.vectorized_pushdowns += 1;
                                self.rows_scanned_vectorized += arr.len();
                                return Ok(JsonValue::Array(projected));
                            }
                        }
                    }
                }
                let mut out = Vec::with_capacity(arr.len());
                for item in arr {
                    out.push(self.invoke_closure(closure, &[item], host)?);
                }
                Ok(JsonValue::Array(out))
            }
            "find" => {
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(".find() requires a closure argument".to_string())
                })?;
                for item in arr {
                    let matched = self.invoke_closure(closure, &[item.clone()], host)?;
                    if Self::is_truthy(&matched) {
                        return Ok(item);
                    }
                }
                Ok(JsonValue::Null)
            }
            "some" | "any" => {
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(".some() requires a closure argument".to_string())
                })?;
                for item in arr {
                    let matched = self.invoke_closure(closure, &[item], host)?;
                    if Self::is_truthy(&matched) {
                        return Ok(JsonValue::Bool(true));
                    }
                }
                Ok(JsonValue::Bool(false))
            }
            "every" | "all" => {
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(".every() requires a closure argument".to_string())
                })?;
                for item in arr {
                    let matched = self.invoke_closure(closure, &[item], host)?;
                    if !Self::is_truthy(&matched) {
                        return Ok(JsonValue::Bool(false));
                    }
                }
                Ok(JsonValue::Bool(true))
            }
            "reduce" => {
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(".reduce() requires a closure argument".to_string())
                })?;
                let mut acc = if let Some(init_expr) = args.get(1) {
                    self.eval_expr(init_expr, host)?
                } else {
                    JsonValue::from(0)
                };
                for item in arr {
                    acc = self.invoke_closure(closure, &[acc, item], host)?;
                }
                Ok(acc)
            }
            "dedup_by_key" => {
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(
                        ".dedup_by_key() requires a closure argument".to_string(),
                    )
                })?;
                // Flatten one level if elements are arrays (e.g. when pushing lookup arrays)
                let mut flattened = Vec::new();
                for item in arr {
                    if let JsonValue::Array(inner) = item {
                        flattened.extend(inner);
                    } else {
                        flattened.push(item);
                    }
                }
                let mut seen = Vec::new();
                let mut out = Vec::new();
                for item in flattened {
                    let key_val = self.invoke_closure(closure, &[item.clone()], host)?;
                    let key_str = Self::val_to_display_string(&key_val);
                    if !seen.contains(&key_str) {
                        seen.push(key_str);
                        out.push(item);
                    }
                }
                Ok(JsonValue::Array(out))
            }
            "sort_by" | "sort_desc_by" => {
                let desc = method == "sort_desc_by";
                let closure = args.first().ok_or_else(|| {
                    CodeModeError::RuntimeError(
                        ".sort_by() requires a closure argument".to_string(),
                    )
                })?;
                let mut keyed = Vec::with_capacity(arr.len());
                for item in arr.drain(..) {
                    let k = self.invoke_closure(closure, &[item.clone()], host)?;
                    keyed.push((k, item));
                }
                keyed.sort_by(|(ka, _), (kb, _)| {
                    let ord = match (ka.as_f64(), kb.as_f64()) {
                        (Some(fa), Some(fb)) => {
                            fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        _ => Self::val_to_display_string(ka).cmp(&Self::val_to_display_string(kb)),
                    };
                    if desc {
                        ord.reverse()
                    } else {
                        ord
                    }
                });
                Ok(JsonValue::Array(
                    keyed.into_iter().map(|(_, item)| item).collect(),
                ))
            }
            _ => Ok(JsonValue::Array(arr)),
        }
    }

    fn eval_string_method(
        &mut self,
        s: &str,
        method: &str,
        args: &[Expr],
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        match method {
            "len" | "length" => Ok(JsonValue::from(s.len() as i64)),
            "toLowerCase" | "to_lowercase" => Ok(JsonValue::from(s.to_lowercase())),
            "toUpperCase" | "to_uppercase" => Ok(JsonValue::from(s.to_uppercase())),
            "trim" => Ok(JsonValue::from(s.trim())),
            "includes" | "contains" => {
                let pat = if let Some(a0) = args.first() {
                    self.eval_expr(a0, host)?
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                } else {
                    String::new()
                };
                Ok(JsonValue::Bool(s.contains(&pat)))
            }
            _ => Ok(JsonValue::from(s)),
        }
    }

    fn invoke_closure(
        &mut self,
        closure_expr: &Expr,
        arg_vals: &[JsonValue],
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        let (params, body) = match closure_expr {
            Expr::Closure { params, body } => (params, body),
            _ => {
                return Err(CodeModeError::RuntimeError(
                    "Expected closure or arrow function".to_string(),
                ))
            }
        };

        self.call_depth += 1;
        if self.call_depth > self.limits.max_call_levels {
            self.call_depth -= 1;
            return Err(CodeModeError::CallDepthExceeded(
                self.limits.max_call_levels,
            ));
        }

        let mut saved = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            saved.push((p.clone(), self.env.get(p).cloned()));
            let v = arg_vals.get(i).cloned().unwrap_or(JsonValue::Null);
            self.env.insert(p.clone(), v);
        }

        let res = match body {
            ClosureBody::Expr(e) => self.eval_expr(e, host),
            ClosureBody::Block(stmts) => self.eval_block(stmts, host).map(|(v, _)| v),
        };

        for (p, prev) in saved {
            match prev {
                Some(v) => {
                    self.env.insert(p, v);
                }
                None => {
                    self.env.remove(&p);
                }
            }
        }
        self.call_depth -= 1;
        res
    }

    fn dispatch_host_tool(
        &mut self,
        server_name: &str,
        tool_name: &str,
        args: Vec<JsonValue>,
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        self.tool_calls += 1;
        if self.tool_calls > self.limits.max_host_calls {
            return Err(CodeModeError::MaxHostCallsExceeded(
                self.limits.max_host_calls,
            ));
        }
        self.burn_fuel(10)?;

        let resp = (host.invoke_tool)(server_name, tool_name, args)?;
        let resp_bytes = resp.to_string_compact().len();
        self.intermediate_bytes_processed += resp_bytes;
        self.check_memory(resp_bytes)?;
        Ok(resp)
    }

    fn format_console_args(args: &[JsonValue]) -> String {
        args.iter()
            .map(Self::val_to_display_string)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn val_to_display_string(v: &JsonValue) -> String {
        match v {
            JsonValue::Str(s) => s.clone(),
            JsonValue::Int(n) => format!("{}", n),
            JsonValue::Float(n) => {
                if n.fract() == 0.0 {
                    format!("{}", *n as i64)
                } else {
                    format!("{}", n)
                }
            }
            JsonValue::Bool(b) => b.to_string(),
            JsonValue::Null => "null".to_string(),
            other => other.to_string_compact(),
        }
    }

    fn is_truthy(v: &JsonValue) -> bool {
        match v {
            JsonValue::Null => false,
            JsonValue::Bool(b) => *b,
            JsonValue::Int(n) => *n != 0,
            JsonValue::Float(n) => *n != 0.0,
            JsonValue::Str(s) => !s.is_empty(),
            JsonValue::Array(a) => !a.is_empty(),
            JsonValue::Object(m) => !m.is_empty(),
        }
    }

    fn values_equal(a: &JsonValue, b: &JsonValue) -> bool {
        match (a, b) {
            (JsonValue::Null, JsonValue::Null) => true,
            (JsonValue::Bool(x), JsonValue::Bool(y)) => x == y,
            (JsonValue::Int(x), JsonValue::Int(y)) => x == y,
            (JsonValue::Str(x), JsonValue::Str(y)) => x == y,
            _ => {
                if let (Some(fa), Some(fb)) = (a.as_f64(), b.as_f64()) {
                    (fa - fb).abs() < 1e-9
                } else {
                    a.to_string_compact() == b.to_string_compact()
                }
            }
        }
    }

    fn eval_bin_op(
        left: &JsonValue,
        op: BinOp,
        right: &JsonValue,
    ) -> Result<JsonValue, CodeModeError> {
        match op {
            BinOp::Add => {
                if matches!(left, JsonValue::Str(_)) || matches!(right, JsonValue::Str(_)) {
                    let s = format!(
                        "{}{}",
                        Self::val_to_display_string(left),
                        Self::val_to_display_string(right)
                    );
                    return Ok(JsonValue::from(s));
                }
                let a = left.as_f64().unwrap_or(0.0);
                let b = right.as_f64().unwrap_or(0.0);
                let res = a + b;
                if matches!(left, JsonValue::Int(_)) && matches!(right, JsonValue::Int(_)) {
                    Ok(JsonValue::from(res as i64))
                } else {
                    Ok(JsonValue::from(res))
                }
            }
            BinOp::Sub => {
                let a = left.as_f64().unwrap_or(0.0);
                let b = right.as_f64().unwrap_or(0.0);
                let res = a - b;
                if matches!(left, JsonValue::Int(_)) && matches!(right, JsonValue::Int(_)) {
                    Ok(JsonValue::from(res as i64))
                } else {
                    Ok(JsonValue::from(res))
                }
            }
            BinOp::Mul => {
                let a = left.as_f64().unwrap_or(0.0);
                let b = right.as_f64().unwrap_or(0.0);
                let res = a * b;
                if matches!(left, JsonValue::Int(_)) && matches!(right, JsonValue::Int(_)) {
                    Ok(JsonValue::from(res as i64))
                } else {
                    Ok(JsonValue::from(res))
                }
            }
            BinOp::Div => {
                let a = left.as_f64().unwrap_or(0.0);
                let b = right.as_f64().unwrap_or(0.0);
                if b == 0.0 {
                    return Err(CodeModeError::RuntimeError("Division by zero".to_string()));
                }
                Ok(JsonValue::from(a / b))
            }
            BinOp::Rem => {
                let a = left.as_i64().unwrap_or(0);
                let b = right.as_i64().unwrap_or(1);
                if b == 0 {
                    return Err(CodeModeError::RuntimeError("Modulo by zero".to_string()));
                }
                Ok(JsonValue::from(a % b))
            }
            BinOp::Eq => Ok(JsonValue::Bool(Self::values_equal(left, right))),
            BinOp::NotEq => Ok(JsonValue::Bool(!Self::values_equal(left, right))),
            BinOp::Lt => Ok(JsonValue::Bool(
                left.as_f64().unwrap_or(0.0) < right.as_f64().unwrap_or(0.0),
            )),
            BinOp::LtEq => Ok(JsonValue::Bool(
                left.as_f64().unwrap_or(0.0) <= right.as_f64().unwrap_or(0.0),
            )),
            BinOp::Gt => Ok(JsonValue::Bool(
                left.as_f64().unwrap_or(0.0) > right.as_f64().unwrap_or(0.0),
            )),
            BinOp::GtEq => Ok(JsonValue::Bool(
                left.as_f64().unwrap_or(0.0) >= right.as_f64().unwrap_or(0.0),
            )),
            BinOp::And | BinOp::Or | BinOp::NullCoalesce => Ok(JsonValue::Bool(false)),
        }
    }
}
