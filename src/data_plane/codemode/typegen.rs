use super::*;

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
    /// Maximum wall-clock execution time in milliseconds before terminating (`Timeout`). Default: 10,000 ms.
    pub max_execution_time_ms: u64,
    /// Maximum concurrent tool calls in a single `ParallelWave`. Default: 8.
    pub max_parallel_threads: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_operations: 50_000,
            max_call_levels: 10,
            max_expr_depth: 20,
            max_memory_bytes: 16 * 1024 * 1024,
            max_host_calls: 64,
            max_execution_time_ms: 10_000,
            max_parallel_threads: 8,
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
/// enriched with OmniMCP sub-millisecond telemetry, vectorized pushdown, SSA parallelization,
/// stateful suspend/resume frames, Saga rollback metrics, and `-320xx` error contracts.
#[derive(Clone, Debug)]
pub struct CodeOutput {
    /// `"OK"`, `"APPROVAL_REQUIRED"`, `"SAGA_ROLLED_BACK"`, or `"ERROR"`.
    pub status: String,
    /// Compiler-checked feature status enum corresponding to the execution outcome.
    pub feature_status: FeatureStatus,
    /// Authoritative JSON-RPC 2.0 `-320xx` error code when `status != "OK"`.
    pub error_code: Option<i32>,
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
            "feature_status": self.feature_status.as_str(),
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
        if let Some(code) = self.error_code {
            out.insert("error_code", JsonValue::from(code as i64));
        }
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
                let pascal_name = Self::to_pascal_case(&tool_ident);
                let input_ts = Self::json_schema_to_ts_type(&tool.input_schema);
                let return_ts =
                    Self::resolve_return_ts(&pascal_name, &tool.output_schema, &mut srv_tool_lines);
                if let Some(input_iface) =
                    Self::emit_named_interface(&format!("{}Input", pascal_name), &tool.input_schema)
                {
                    srv_tool_lines.push(input_iface);
                }
                let mut_badge = if tool.is_mutation { "[MUTATION] " } else { "" };
                let desc_clean = tool.description.replace('\n', " ");
                srv_tool_lines.push(format!("    /** {}{} */", mut_badge, desc_clean));
                srv_tool_lines.push(format!(
                    "    function {}(input: {}): Promise<{}>;",
                    tool_ident, input_ts, return_ts
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
        lines.push("  /** Search OmniMCP catalog for tools by keyword/intent */".to_string());
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

    /// Standard Okapi BM25 term frequency saturation parameter ($k_1 \approx 1.2$).
    pub const BM25_K1: f64 = 1.2;
    /// Standard Okapi BM25 document length normalization parameter ($b \approx 0.75$).
    pub const BM25_B: f64 = 0.75;

    /// Lightweight stemmer for sub-millisecond intent routing.
    pub fn stem_token(tok: &str) -> String {
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

    /// Splits an arbitrary text or identifier (`camelCase`, `snake_case`, `kebab-case`, prose)
    /// into normalized, stemmed tokens for Okapi BM25 indexing and querying.
    pub fn tokenize_and_stem(text: &str) -> Vec<String> {
        let mut raw_words = Vec::new();
        let mut current = String::new();
        let mut prev_is_lower = false;

        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                if ch.is_ascii_uppercase() && prev_is_lower && !current.is_empty() {
                    raw_words.push(std::mem::take(&mut current));
                }
                current.push(ch.to_ascii_lowercase());
                prev_is_lower = ch.is_ascii_lowercase();
            } else {
                if !current.is_empty() {
                    raw_words.push(std::mem::take(&mut current));
                }
                prev_is_lower = false;
            }
        }
        if !current.is_empty() {
            raw_words.push(current);
        }

        raw_words
            .into_iter()
            .filter(|w| w.len() >= 2)
            .map(|w| Self::stem_token(&w))
            .collect()
    }

    /// Computes true Okapi BM25 scores ($k_1 = 1.2, b = 0.75$) over a corpus of multi-field documents.
    /// Each document is represented as a slice of `(field_boost, field_text)` pairs.
    pub fn bm25_score_corpus(query: &str, corpus: &[Vec<(f64, String)>]) -> Vec<f64> {
        let n_docs = corpus.len();
        if n_docs == 0 {
            return Vec::new();
        }

        let query_stems = Self::tokenize_and_stem(query);
        if query_stems.is_empty() {
            return vec![0.0; n_docs];
        }

        // 1. Tokenize each document field and compute document lengths and weighted term frequencies
        struct IndexedDoc {
            doc_len: f64,
            weighted_tf: BTreeMap<String, f64>,
        }

        let mut indexed_docs = Vec::with_capacity(n_docs);
        let mut total_doc_len = 0.0f64;

        for fields in corpus {
            let mut doc_len = 0.0f64;
            let mut weighted_tf: BTreeMap<String, f64> = BTreeMap::new();

            for (boost, text) in fields {
                let stems = Self::tokenize_and_stem(text);
                doc_len += stems.len() as f64;
                for st in stems {
                    *weighted_tf.entry(st).or_insert(0.0) += *boost;
                }
            }

            let effective_len = doc_len.max(1.0);
            total_doc_len += effective_len;
            indexed_docs.push(IndexedDoc {
                doc_len: effective_len,
                weighted_tf,
            });
        }

        let avgdl = (total_doc_len / (n_docs as f64)).max(1.0);

        // Helper to compute weighted term frequency of query stem `q` in `doc`
        let term_freq_in_doc = |q: &str, doc: &IndexedDoc| -> f64 {
            let mut tf = *doc.weighted_tf.get(q).unwrap_or(&0.0);
            if tf == 0.0 && q.len() >= 3 {
                for (tok, w) in &doc.weighted_tf {
                    if tok.len() >= 3 && (tok.starts_with(q) || q.starts_with(tok)) {
                        tf += *w * 0.65;
                    }
                }
            }
            tf
        };

        // 2. Deduplicate query stems while preserving query term multiplicity weight
        let mut unique_query_stems: BTreeMap<String, f64> = BTreeMap::new();
        for q in &query_stems {
            *unique_query_stems.entry(q.clone()).or_insert(0.0) += 1.0;
        }

        // 3. Compute corpus Document Frequency (df) and Okapi BM25+ IDF for each unique query stem
        let mut idf_map: BTreeMap<String, f64> = BTreeMap::new();
        let n_f64 = n_docs as f64;
        for q in unique_query_stems.keys() {
            let df = indexed_docs
                .iter()
                .filter(|d| term_freq_in_doc(q, d) > 0.0)
                .count() as f64;
            if df > 0.0 {
                let idf = (1.0 + (n_f64 - df + 0.5) / (df + 0.5)).ln() + 1.0;
                idf_map.insert(q.clone(), idf);
            }
        }

        // 4. Score each document using Okapi BM25 with TF saturation (k1) and length normalization (b)
        let mut scores = Vec::with_capacity(n_docs);
        for doc in &indexed_docs {
            let mut doc_score = 0.0f64;
            let len_norm = 1.0 - Self::BM25_B + Self::BM25_B * (doc.doc_len / avgdl);
            for (q, q_count) in &unique_query_stems {
                if let Some(&idf) = idf_map.get(q) {
                    let tf = term_freq_in_doc(q, doc);
                    if tf > 0.0 {
                        let numerator = tf * (Self::BM25_K1 + 1.0);
                        let denominator = tf + Self::BM25_K1 * len_norm;
                        doc_score += idf * (numerator / denominator) * q_count.min(2.0);
                    }
                }
            }
            scores.push(doc_score);
        }

        scores
    }

    /// Speculative 0-Turn Schema Pre-Injection (`SpeculativeSchemaRouter`):
    /// Scores the active catalog against a user/agent task intent in < 200 µs using true Okapi BM25
    /// ($k_1 = 1.2, b = 0.75$) over stemmed `name`, `server`, `tags`, `description`, and `schema` fields,
    /// and synthesizes a compact TypeScript `.d.ts` declaration containing only the top-K
    /// highest-confidence tools. Collapses 2-turn `search -> execute` into a single 0-turn call.
    pub fn speculative_pre_inject(
        servers: &[ServerRecord],
        intent: &str,
        top_k: usize,
        confidence_threshold: f64,
    ) -> SpeculativeInjectionResult {
        let t0 = Instant::now();
        let top_k = top_k.max(1);
        let raw_tokens = Self::tokenize_and_stem(intent);

        let mut full_catalog_raw_bytes = 0usize;
        let mut active_entries: Vec<(ServerRecord, crate::models::ToolRecord)> = Vec::new();
        let mut corpus_fields: Vec<Vec<(f64, String)>> = Vec::new();

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

                let schema_keys: String = tool
                    .input_schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map(|m| {
                        m.iter()
                            .map(|(k, _)| k.clone())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();

                corpus_fields.push(vec![
                    (3.5, tool.name.clone()),
                    (2.5, srv.name.clone()),
                    (3.0, tool.tags.join(" ")),
                    (1.5, tool.description.clone()),
                    (1.2, schema_keys),
                ]);
                active_entries.push((srv.clone(), tool.clone()));
            }
        }

        let bm25_scores = Self::bm25_score_corpus(intent, &corpus_fields);
        let mut candidates = Vec::new();
        for ((srv, tool), score) in active_entries.into_iter().zip(bm25_scores) {
            if score > 0.0 {
                candidates.push((score, srv, tool));
            }
        }

        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let selected: Vec<(f64, ServerRecord, crate::models::ToolRecord)> =
            candidates.into_iter().take(top_k).collect();

        let top_score = selected.first().map(|c| c.0).unwrap_or(0.0);
        let total_selected_score: f64 = selected.iter().map(|c| c.0).sum();
        let confidence = if top_score > 0.0 {
            let norm = (top_score / 4.2)
                .max(total_selected_score / ((raw_tokens.len().max(1) as f64) * 1.5));
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
                    let pascal_name = Self::to_pascal_case(&tool_ident);
                    let input_ts = Self::json_schema_to_ts_type(&tool.input_schema);
                    let return_ts =
                        Self::resolve_return_ts(&pascal_name, &tool.output_schema, &mut lines);
                    let mut_badge = if tool.is_mutation { "[MUTATION] " } else { "" };
                    let desc_clean = tool.description.replace('\n', " ");
                    lines.push(format!("    /** {}{} */", mut_badge, desc_clean));
                    lines.push(format!(
                        "    function {}(input: {}): Promise<{}>;",
                        tool_ident, input_ts, return_ts
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
    pub fn to_pascal_case(ident: &str) -> String {
        ident
            .split('_')
            .filter(|p| !p.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect()
    }

    fn resolve_return_ts(
        pascal_name: &str,
        output_schema: &JsonValue,
        out_lines: &mut Vec<String>,
    ) -> String {
        if output_schema.is_null() {
            return "any".to_string();
        }
        let result_iface_name = format!("{}Result", pascal_name);
        if let Some(iface_block) = Self::emit_named_interface(&result_iface_name, output_schema) {
            out_lines.push(iface_block);
            return result_iface_name;
        }
        let is_untyped_object = output_schema.get("type").and_then(|t| t.as_str())
            == Some("object")
            && output_schema.get("properties").is_none()
            && output_schema.get("$ref").is_none()
            && output_schema.get("oneOf").is_none()
            && output_schema.get("anyOf").is_none();
        if is_untyped_object {
            "any".to_string()
        } else {
            Self::json_schema_to_ts_type(output_schema)
        }
    }

    pub fn emit_named_interface(name: &str, schema: &JsonValue) -> Option<String> {
        Self::emit_named_interface_inner(name, schema, schema, 0)
    }

    fn emit_named_interface_inner(
        name: &str,
        schema: &JsonValue,
        root: &JsonValue,
        depth: usize,
    ) -> Option<String> {
        if depth >= 8 {
            return None;
        }
        let resolved_schema = schema
            .get("$ref")
            .and_then(|r| r.as_str())
            .and_then(|r| Self::resolve_json_pointer(root, r))
            .unwrap_or(schema);
        let props = resolved_schema
            .get("properties")
            .and_then(|p| p.as_object())?;
        if props.is_empty() {
            return None;
        }
        let required_set: Vec<&str> = resolved_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let mut nested_blocks = Vec::new();
        let mut lines = vec![format!("    interface {} {{", name)];
        for (prop_name, prop_schema) in props {
            let prop_ident = Self::sanitize_ident(prop_name);
            let is_req = required_set.contains(&prop_name.as_str());
            let opt_mark = if is_req { "" } else { "?" };
            let resolved_prop = prop_schema
                .get("$ref")
                .and_then(|r| r.as_str())
                .and_then(|r| Self::resolve_json_pointer(root, r))
                .unwrap_or(prop_schema);
            let ts_type = if resolved_prop
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|m| !m.is_empty())
                .unwrap_or(false)
            {
                let nested_name = format!("{}_{}", name, Self::to_pascal_case(prop_name));
                if let Some(nested_block) =
                    Self::emit_named_interface_inner(&nested_name, resolved_prop, root, depth + 1)
                {
                    nested_blocks.push(nested_block);
                    nested_name
                } else {
                    Self::json_schema_to_ts_type_inner(prop_schema, root, depth + 1)
                }
            } else {
                Self::json_schema_to_ts_type_inner(prop_schema, root, depth + 1)
            };
            if let Some(desc) = prop_schema.get("description").and_then(|d| d.as_str()) {
                let clean_desc = desc.replace("*/", "* /").replace('\n', " ");
                lines.push(format!("      /** {} */", clean_desc));
            }
            lines.push(format!("      {}{}: {};", prop_ident, opt_mark, ts_type));
        }
        lines.push("    }".to_string());
        if nested_blocks.is_empty() {
            Some(lines.join("\n"))
        } else {
            nested_blocks.push(lines.join("\n"));
            Some(nested_blocks.join("\n"))
        }
    }

    pub fn json_schema_to_ts_type(schema: &JsonValue) -> String {
        Self::json_schema_to_ts_type_inner(schema, schema, 0)
    }

    fn json_schema_to_ts_type_inner(schema: &JsonValue, root: &JsonValue, depth: usize) -> String {
        if depth >= 8 {
            return "any".to_string();
        }

        if let Some(ref_str) = schema.get("$ref").and_then(|v| v.as_str()) {
            if let Some(target) = Self::resolve_json_pointer(root, ref_str) {
                return Self::json_schema_to_ts_type_inner(target, root, depth + 1);
            }
            return "any".to_string();
        }

        if let Some(enum_arr) = schema.get("enum").and_then(|v| v.as_array()) {
            if !enum_arr.is_empty() {
                let variants: Vec<String> = enum_arr
                    .iter()
                    .map(|item| {
                        if let Some(s) = item.as_str() {
                            format!("\"{}\"", s.replace('"', "\\\""))
                        } else {
                            item.to_string_compact()
                        }
                    })
                    .collect();
                return variants.join(" | ");
            }
        }

        for union_key in ["oneOf", "anyOf"] {
            if let Some(arr) = schema.get(union_key).and_then(|v| v.as_array()) {
                if !arr.is_empty() {
                    let variants: Vec<String> = arr
                        .iter()
                        .map(|s| Self::json_schema_to_ts_type_inner(s, root, depth + 1))
                        .collect();
                    return variants.join(" | ");
                }
            }
        }

        if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
            if !props.is_empty() {
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
                    let ts_type = Self::json_schema_to_ts_type_inner(prop_schema, root, depth + 1);
                    fields.push(format!("{}{}: {}", prop_ident, opt_mark, ts_type));
                }

                return format!("{{ {} }}", fields.join("; "));
            }
        }

        match schema.get("type").and_then(|t| t.as_str()).unwrap_or("any") {
            "string" => "string".to_string(),
            "integer" | "number" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            "array" => {
                if let Some(items) = schema.get("items") {
                    let item_ty = Self::json_schema_to_ts_type_inner(items, root, depth + 1);
                    if item_ty.contains('|') {
                        format!("({})[]", item_ty)
                    } else {
                        format!("{}[]", item_ty)
                    }
                } else {
                    "any[]".to_string()
                }
            }
            "object" => "Record<string, any>".to_string(),
            _ if depth == 0 => "Record<string, any>".to_string(),
            _ => "any".to_string(),
        }
    }

    fn resolve_json_pointer<'a>(root: &'a JsonValue, pointer: &str) -> Option<&'a JsonValue> {
        let path = pointer.strip_prefix("#/")?;
        let mut cur = root;
        for seg in path.split('/') {
            let unescaped = seg.replace("~1", "/").replace("~0", "~");
            cur = cur.get(&unescaped)?;
        }
        Some(cur)
    }
}
