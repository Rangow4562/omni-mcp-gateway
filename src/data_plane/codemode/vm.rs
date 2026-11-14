use super::*;

/// Callbacks provided by the OmniMCP host (`MCPProxyGateway`) into the sandbox.
#[allow(clippy::type_complexity)]
pub struct HostCallbacks<'a> {
    /// Invokes `(server_name, tool_name, args_vec) -> Result<JsonValue, CodeModeError>`
    pub invoke_tool:
        &'a mut dyn FnMut(&str, &str, Vec<JsonValue>) -> Result<JsonValue, CodeModeError>,
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
pub(super) enum ControlFlow {
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
                    status: FeatureStatus::Error.as_str().to_string(),
                    feature_status: e.feature_status(),
                    error_code: Some(e.error_code()),
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
                    status: FeatureStatus::Error.as_str().to_string(),
                    feature_status: e.feature_status(),
                    error_code: Some(e.error_code()),
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
            start_time: t_eval,
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

        let eval_res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.eval_block(&stmts, host)));

        let eval_block_res = match eval_res {
            Ok(res) => res,
            Err(panic_payload) => {
                let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    format!("Panic: {}", s)
                } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                    format!("Panic: {}", s)
                } else {
                    "Panic: script execution panicked".to_string()
                };
                Err(CodeModeError::RuntimeError(panic_msg))
            }
        };

        match eval_block_res {
            Ok((val, _cf)) => {
                let eval_us = t_eval.elapsed().as_micros() as u64;
                let output_bytes = val.to_string_compact().len();
                CodeOutput {
                    status: FeatureStatus::Ok.as_str().to_string(),
                    feature_status: FeatureStatus::Ok,
                    error_code: None,
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
                let pending_args_hash =
                    sha256_hex(pending.arguments.to_string_compact().as_bytes());
                let created_at = now_epoch_secs();
                let expires_at = created_at + 3600.0;
                let mut frame = SuspendedExecutionFrame {
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
                    created_at,
                    pending_arguments_hash: pending_args_hash,
                    expires_at,
                    approver_identity: None,
                    is_consumed: false,
                    frame_hmac: String::new(),
                };
                frame.frame_hmac = frame.compute_hmac(crate::models::continuation_key());
                CodeOutput {
                    status: FeatureStatus::ApprovalRequired.as_str().to_string(),
                    feature_status: FeatureStatus::ApprovalRequired,
                    error_code: Some(APPROVAL_REQUIRED),
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
                    status: FeatureStatus::Error.as_str().to_string(),
                    feature_status: e.feature_status(),
                    error_code: Some(e.error_code()),
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
                status: FeatureStatus::Error.as_str().to_string(),
                feature_status: FeatureStatus::Error,
                error_code: Some(INTERNAL_ERROR),
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
                    status: FeatureStatus::Error.as_str().to_string(),
                    feature_status: e.feature_status(),
                    error_code: Some(e.error_code()),
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
                    status: FeatureStatus::Error.as_str().to_string(),
                    feature_status: e.feature_status(),
                    error_code: Some(e.error_code()),
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

        let remaining_stmts = if frame.instruction_ptr < stmts.len() {
            &stmts[frame.instruction_ptr + 1..]
        } else {
            &[]
        };

        let mut vm = VmState {
            env,
            limits: &self.limits,
            start_time: Instant::now(),
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
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                vm.eval_block(remaining_stmts, host)
            }));
            match res {
                Ok(r) => r,
                Err(panic_payload) => {
                    let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                        format!("Panic: {}", s)
                    } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                        format!("Panic: {}", s)
                    } else {
                        "Panic: script execution panicked".to_string()
                    };
                    Err(CodeModeError::RuntimeError(panic_msg))
                }
            }
        };

        match eval_res {
            Ok((val, _cf)) => {
                let eval_us = t_eval.elapsed().as_micros() as u64;
                let output_bytes = val.to_string_compact().len();
                CodeOutput {
                    status: FeatureStatus::Ok.as_str().to_string(),
                    feature_status: FeatureStatus::Ok,
                    error_code: None,
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
                    status: FeatureStatus::Error.as_str().to_string(),
                    feature_status: e.feature_status(),
                    error_code: Some(e.error_code()),
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

pub(super) struct VmState<'a> {
    env: BTreeMap<String, JsonValue>,
    limits: &'a SandboxLimits,
    start_time: Instant,
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
    pub(super) fn burn_fuel(&mut self, cost: u64) -> Result<(), CodeModeError> {
        self.ops_consumed += cost;
        if self.ops_consumed > self.limits.max_operations {
            return Err(CodeModeError::FuelExhausted(self.limits.max_operations));
        }
        if self.start_time.elapsed().as_millis() as u64 > self.limits.max_execution_time_ms {
            return Err(CodeModeError::Timeout(self.limits.max_execution_time_ms));
        }
        Ok(())
    }

    pub(super) fn check_memory(&mut self, additional_bytes: usize) -> Result<(), CodeModeError> {
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

    pub(super) fn estimate_val_bytes(v: &JsonValue) -> usize {
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

    pub(super) fn eval_block(
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
                Stmt::LetDestructureObject { bindings, expr } => {
                    let val = self.eval_expr(expr, host)?;
                    for (key, local) in bindings {
                        let prop = val.get(key).cloned().unwrap_or(JsonValue::Null);
                        self.check_memory(Self::estimate_val_bytes(&prop))?;
                        self.env.insert(local.clone(), prop);
                    }
                    last_val = JsonValue::Null;
                }
                Stmt::LetDestructureArray { names, expr } => {
                    let val = self.eval_expr(expr, host)?;
                    let arr = val.as_array();
                    for (idx, local) in names.iter().enumerate() {
                        let elem = arr
                            .and_then(|a| a.get(idx))
                            .cloned()
                            .unwrap_or(JsonValue::Null);
                        self.check_memory(Self::estimate_val_bytes(&elem))?;
                        self.env.insert(local.clone(), elem);
                    }
                    last_val = JsonValue::Null;
                }
                Stmt::TryCatch {
                    try_body,
                    catch_var,
                    catch_body,
                    finally_body,
                } => {
                    let try_res = self.eval_block(try_body, host);
                    let mut final_res = match try_res {
                        Ok((val, cf)) => {
                            last_val = val;
                            if cf == ControlFlow::Return {
                                Ok((last_val.clone(), ControlFlow::Return))
                            } else {
                                Ok((last_val.clone(), ControlFlow::None))
                            }
                        }
                        Err(CodeModeError::RuntimeError(msg)) => {
                            if let Some(ref c_body) = catch_body {
                                if let Some(ref var_name) = catch_var {
                                    self.env.insert(var_name.clone(), JsonValue::from(msg));
                                }
                                match self.eval_block(c_body, host) {
                                    Ok((cval, ccf)) => Ok((cval, ccf)),
                                    Err(ce) => Err(ce),
                                }
                            } else {
                                Err(CodeModeError::RuntimeError(msg))
                            }
                        }
                        Err(non_catchable) => Err(non_catchable),
                    };
                    if let Some(ref f_body) = finally_body {
                        if let Err(fe) = self.eval_block(f_body, host) {
                            final_res = Err(fe);
                        }
                    }
                    let (v, cf) = final_res?;
                    last_val = v;
                    if cf == ControlFlow::Return {
                        return Ok((last_val, ControlFlow::Return));
                    }
                }
                Stmt::Throw(expr) => {
                    let val = self.eval_expr(expr, host)?;
                    let msg = match val.as_str() {
                        Some(s) => s.to_string(),
                        None => val.to_string_compact(),
                    };
                    return Err(CodeModeError::RuntimeError(msg));
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

                    let chunk_size = self.limits.max_parallel_threads.max(1);
                    for chunk in tasks.chunks(chunk_size) {
                        let mut batch_requests = Vec::with_capacity(chunk.len());
                        for task in chunk {
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
                        self.parallelized_tool_calls += chunk.len();

                        for (task, resp) in chunk.iter().zip(results) {
                            let resp_bytes = resp.to_string_compact().len();
                            self.intermediate_bytes_processed += resp_bytes;
                            self.check_memory(resp_bytes)?;
                            if let Some(ref var_name) = task.target_var {
                                self.env.insert(var_name.clone(), resp.clone());
                            }
                            last_val = resp;
                        }
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

    pub(super) fn apply_assign_op(
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

    pub(super) fn eval_expr(
        &mut self,
        expr: &Expr,
        host: &mut HostCallbacks<'_>,
    ) -> Result<JsonValue, CodeModeError> {
        self.burn_fuel(1)?;
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Template(parts) => {
                let mut out = String::new();
                for (lit, expr_opt) in parts {
                    out.push_str(lit);
                    if let Some(e) = expr_opt {
                        let v = self.eval_expr(e, host)?;
                        if let Some(s) = v.as_str() {
                            out.push_str(s);
                        } else {
                            out.push_str(&v.to_string_compact());
                        }
                    }
                }
                self.check_memory(out.len())?;
                Ok(JsonValue::from(out))
            }
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

    pub(super) fn eval_namespace_call(
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

        // 3. Explicit `mcp::call("server", "tool", args)` or `call_tool("server", "tool", args)`
        if (namespace == Some("mcp") && func == "call")
            || (namespace.is_none() && (func == "call_tool" || func == "callTool"))
        {
            if evaluated_args.len() < 2 {
                return Err(CodeModeError::RuntimeError(
                    "call_tool requires (server_name, tool_name, [arguments])".to_string(),
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

        // 4. `service::tool(...)` namespace style host call
        if let Some(srv) = namespace {
            return self.dispatch_host_tool(srv, func, evaluated_args, host);
        }

        Err(CodeModeError::RuntimeError(format!(
            "Unknown function '{}'",
            func
        )))
    }

    pub(super) fn eval_method_call(
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
            if obj_name == "Promise" && method == "all" {
                if let Some(Expr::Array(items)) = args.first() {
                    // Fast-path: if all items are `codemode.<srv>.<tool>(...)`, run via invoke_parallel_batch
                    let mut batch_tasks: Vec<(String, String, Vec<JsonValue>)> = Vec::new();
                    let mut all_codemode = !items.is_empty();
                    for item in items {
                        if let Expr::MethodCall {
                            receiver: inner_rec,
                            method: tool_name,
                            args: call_args,
                        } = item
                        {
                            if let Expr::Field(ns_var, srv_name) = inner_rec.as_ref() {
                                if matches!(ns_var.as_ref(), Expr::Var(ns) if ns == "codemode") {
                                    let mut ev_args = Vec::with_capacity(call_args.len());
                                    for a in call_args {
                                        ev_args.push(self.eval_expr(a, host)?);
                                    }
                                    batch_tasks.push((
                                        srv_name.clone(),
                                        tool_name.clone(),
                                        ev_args,
                                    ));
                                    continue;
                                }
                            }
                        }
                        all_codemode = false;
                        break;
                    }
                    if all_codemode {
                        let results = (host.invoke_parallel_batch)(batch_tasks)?;
                        let arr_val = JsonValue::Array(results);
                        self.check_memory(Self::estimate_val_bytes(&arr_val))?;
                        return Ok(arr_val);
                    }
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(self.eval_expr(item, host)?);
                    }
                    let arr_val = JsonValue::Array(out);
                    self.check_memory(Self::estimate_val_bytes(&arr_val))?;
                    return Ok(arr_val);
                }
                let arr_val = args
                    .first()
                    .map(|a| self.eval_expr(a, host))
                    .transpose()?
                    .unwrap_or_else(|| JsonValue::Array(Vec::new()));
                return Ok(arr_val);
            }

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

    pub(super) fn eval_array_method(
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
                    self.eval_expr(a1, host)?
                        .as_u64()
                        .unwrap_or(arr.len() as u64) as usize
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
                    if let Some((phys, cols)) =
                        PhysicalExpr::try_compile_closure(closure, &self.env)
                    {
                        if let Some(batch) = ColumnarRecordBatch::from_rows(&arr, &cols) {
                            if let Some(mask) = phys.eval_filter_mask(&batch) {
                                self.burn_fuel(1)?;
                                self.vectorized_pushdowns += 1;
                                self.rows_scanned_vectorized += arr.len();
                                let out: Vec<JsonValue> = arr
                                    .into_iter()
                                    .zip(mask)
                                    .filter_map(|(row, keep)| if keep { Some(row) } else { None })
                                    .collect();
                                return Ok(JsonValue::Array(out));
                            }
                        }
                    }
                }
                let mut out = Vec::new();
                for item in arr {
                    let keep = self.invoke_closure(closure, std::slice::from_ref(&item), host)?;
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
                    if let Some((phys, cols)) =
                        PhysicalExpr::try_compile_closure(closure, &self.env)
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
                    let matched =
                        self.invoke_closure(closure, std::slice::from_ref(&item), host)?;
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
                    let key_val =
                        self.invoke_closure(closure, std::slice::from_ref(&item), host)?;
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
                    let k = self.invoke_closure(closure, std::slice::from_ref(&item), host)?;
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

    pub(super) fn eval_string_method(
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
                    self.eval_expr(a0, host)?.as_str().unwrap_or("").to_string()
                } else {
                    String::new()
                };
                Ok(JsonValue::Bool(s.contains(&pat)))
            }
            _ => Ok(JsonValue::from(s)),
        }
    }

    pub(super) fn invoke_closure(
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

    pub(super) fn dispatch_host_tool(
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

    pub(super) fn format_console_args(args: &[JsonValue]) -> String {
        args.iter()
            .map(Self::val_to_display_string)
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(super) fn val_to_display_string(v: &JsonValue) -> String {
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

    pub(super) fn is_truthy(v: &JsonValue) -> bool {
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

    pub(super) fn values_equal(a: &JsonValue, b: &JsonValue) -> bool {
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

    pub(super) fn eval_bin_op(
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

/// Strict WebAssembly capability-isolated sandbox profile (v6.0.0, enabled via `sandbox-wasm`).
/// Validates Wasm module magic/version headers and enforces linear memory & fuel ceilings.
#[cfg(feature = "sandbox-wasm")]
#[derive(Clone, Debug)]
pub struct WasmSandboxProfile {
    pub max_linear_memory_bytes: usize,
    pub max_fuel: u64,
    pub allow_wasi_stdio: bool,
}

#[cfg(feature = "sandbox-wasm")]
impl Default for WasmSandboxProfile {
    fn default() -> Self {
        Self {
            max_linear_memory_bytes: 16 * 1024 * 1024,
            max_fuel: 1_000_000,
            allow_wasi_stdio: false,
        }
    }
}

#[cfg(feature = "sandbox-wasm")]
impl WasmSandboxProfile {
    pub fn validate_module_bytes(&self, wasm_bytes: &[u8]) -> Result<(), String> {
        if wasm_bytes.len() < 8 {
            return Err("Invalid Wasm module: fewer than 8 bytes".to_string());
        }
        if &wasm_bytes[0..4] != b"\0asm" {
            return Err("Invalid Wasm module: missing \\0asm magic header".to_string());
        }
        if wasm_bytes[4..8] != [1, 0, 0, 0] {
            return Err("Unsupported Wasm binary version (expected v1)".to_string());
        }
        Ok(())
    }
}
