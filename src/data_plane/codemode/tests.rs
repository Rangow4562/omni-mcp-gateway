use super::*;
use crate::models::{to_legacy_error_code, ProtocolType, ToolRecord};

fn run_noop_script(code: &str, limits: SandboxLimits) -> CodeOutput {
    let mut invoke_cb = |_srv: &str,
                         _tool: &str,
                         _args: Vec<JsonValue>|
     -> Result<JsonValue, CodeModeError> { Ok(json!({"ok": true})) };
    let mut parallel_cb =
        |calls: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
            Ok(calls.into_iter().map(|_| json!({"ok": true})).collect())
        };
    let search_cb = |_q: &str| JsonValue::Array(Vec::new());
    let describe_cb = |_s: &str, _t: Option<&str>| String::new();
    let mut host = HostCallbacks {
        invoke_tool: &mut invoke_cb,
        invoke_parallel_batch: &mut parallel_cb,
        search_catalog: &search_cb,
        describe_types: &describe_cb,
    };
    CodeModeExecutor::new(limits).execute(
        code,
        &mut host,
        "00-00000000000000000000000000000001-0000000000000001-01",
    )
}

#[test]
fn test_parser_errors_and_nesting_depth_guard() {
    let out_unterm = run_noop_script("const x = \"unterminated string;", SandboxLimits::default());
    assert_eq!(out_unterm.status, "ERROR");
    assert_eq!(out_unterm.feature_status, FeatureStatus::Error);
    assert_eq!(out_unterm.error_code, Some(INTERNAL_ERROR));
    assert!(out_unterm
        .error
        .as_deref()
        .unwrap_or("")
        .contains("SyntaxError"));

    let shallow_limits = SandboxLimits {
        max_expr_depth: 4,
        ..Default::default()
    };
    let deep_expr = "return ((((((1 + 2) + 3) + 4) + 5) + 6) + 7);";
    let out_deep = run_noop_script(deep_expr, shallow_limits);
    assert_eq!(out_deep.status, "ERROR");
    assert!(out_deep
        .error
        .as_deref()
        .unwrap_or("")
        .contains("ExprDepthExceeded"));
}

#[test]
fn test_evaluator_semantics_closures_and_console_logs() {
    let script = r#"
        async () => {
            const nums = [10, 25, 30, 5];
            const filtered = nums.filter(n => n >= 20);
            const doubled = filtered.map(n => n * 2);
            const sum = doubled.reduce((acc, v) => acc + v, 0);
            console.log("computed sum:", sum);
            return { count: doubled.length, sum: sum };
        }
        "#;
    let out = run_noop_script(script, SandboxLimits::default());
    assert_eq!(out.status, "OK");
    assert_eq!(out.feature_status, FeatureStatus::Ok);
    assert_eq!(out.error_code, None);
    assert_eq!(out.result["count"].as_i64(), Some(2));
    assert_eq!(out.result["sum"].as_i64(), Some(110));
    assert_eq!(out.logs.len(), 1);
    assert!(out.logs[0].contains("110"));
}

#[test]
fn test_fuel_exhaustion_returns_32032_error_contract() {
    let limits = SandboxLimits {
        max_operations: 120,
        ..Default::default()
    };
    let out = run_noop_script("let i = 0; while (true) { i += 1; }", limits);
    assert_eq!(out.status, "ERROR");
    assert_eq!(out.feature_status, FeatureStatus::FuelExhausted);
    assert_eq!(out.error_code, Some(FUEL_EXHAUSTED));
    assert_eq!(out.error_code.map(to_legacy_error_code), Some(-32032));
    assert!(out.error.as_deref().unwrap_or("").contains("FuelExhausted"));
}

#[test]
fn test_outbound_isolation_returns_32033_error_contract() {
    let out_fetch = run_noop_script(
        r#"async () => { return await fetch("https://example.com"); }"#,
        SandboxLimits::default(),
    );
    assert_eq!(out_fetch.status, "ERROR");
    assert_eq!(out_fetch.feature_status, FeatureStatus::OutboundBlocked);
    assert_eq!(out_fetch.error_code, Some(OUTBOUND_BLOCKED));
    assert_eq!(out_fetch.error_code.map(to_legacy_error_code), Some(-32033));

    let out_fs = run_noop_script(
        r#"async () => { return fs.readFileSync("/etc/passwd"); }"#,
        SandboxLimits::default(),
    );
    assert_eq!(out_fs.feature_status, FeatureStatus::OutboundBlocked);
    assert_eq!(out_fs.error_code, Some(OUTBOUND_BLOCKED));
}

#[test]
fn test_physical_expr_vectorized_columnar_eval() {
    let script = r#"
        async () => {
            const rows = [
                { id: "r1", score: 0.92, active: true, tier: "gold" },
                { id: "r2", score: 0.45, active: true, tier: "silver" },
                { id: "r3", score: 0.88, active: false, tier: "gold" },
                { id: "r4", score: 0.95, active: true, tier: "gold" }
            ];
            const selected = rows
                .filter(r => r.score >= 0.85 && r.tier == "gold" && r.active == true)
                .map(r => ({ id: r.id, pct: r.score * 100 }));
            return selected;
        }
        "#;
    let out = run_noop_script(script, SandboxLimits::default());
    assert_eq!(out.status, "OK");
    assert_eq!(out.vectorized_pushdowns, 2);
    assert_eq!(out.rows_scanned_vectorized, 6);
    let arr = out.result.as_array().expect("Expected array result");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"].as_str(), Some("r1"));
    assert_eq!(arr[1]["id"].as_str(), Some("r4"));
}

#[test]
fn test_suspended_execution_frame_serialize_and_resume_identity() {
    let script = r#"
        async () => {
            const base = await codemode.orders.fetch_order({ id: "ord_7" });
            const mut_res = await codemode.orders.capture_payment({ id: base.id, amount: 42 });
            return { order_id: base.id, receipt: mut_res.tx_id };
        }
        "#;

    let mut invoke_cb =
        |srv: &str, tool: &str, args: Vec<JsonValue>| -> Result<JsonValue, CodeModeError> {
            if srv == "orders" && tool == "fetch_order" {
                Ok(json!({"id": "ord_7", "status": "pending"}))
            } else if srv == "orders" && tool == "capture_payment" {
                Err(CodeModeError::ApprovalRequired(PendingApproval {
                    server_name: srv.to_string(),
                    tool_name: tool.to_string(),
                    arguments: args.first().cloned().unwrap_or_else(JsonValue::object),
                    reason: "Requires human approval".to_string(),
                }))
            } else {
                Ok(JsonValue::Null)
            }
        };
    let mut parallel_cb =
        |_calls: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
            Ok(Vec::new())
        };
    let search_cb = |_q: &str| JsonValue::Array(Vec::new());
    let describe_cb = |_s: &str, _t: Option<&str>| String::new();

    let executor = CodeModeExecutor::new(SandboxLimits::default());
    let suspended = {
        let mut host = HostCallbacks {
            invoke_tool: &mut invoke_cb,
            invoke_parallel_batch: &mut parallel_cb,
            search_catalog: &search_cb,
            describe_types: &describe_cb,
        };
        executor.execute(
            script,
            &mut host,
            "00-00000000000000000000000000000001-0000000000000001-01",
        )
    };

    assert_eq!(suspended.status, "APPROVAL_REQUIRED");
    assert_eq!(suspended.feature_status, FeatureStatus::ApprovalRequired);
    assert_eq!(suspended.error_code, Some(APPROVAL_REQUIRED));
    assert_eq!(suspended.error_code.map(to_legacy_error_code), Some(-32031));

    let orig_frame = suspended
        .suspended_frame
        .expect("Must capture SuspendedExecutionFrame");
    let round_trip_json = orig_frame.to_json();
    let restored_frame = SuspendedExecutionFrame::from_json(&round_trip_json)
        .expect("Must deserialize SuspendedExecutionFrame");
    assert_eq!(orig_frame, restored_frame);

    // Resume using restored frame without re-running step 0
    let mut resume_invoke_cb =
        |_srv: &str, _tool: &str, _args: Vec<JsonValue>| -> Result<JsonValue, CodeModeError> {
            panic!("Step 0 must not be re-invoked during continuation resume");
        };
    let resumed = {
        let mut host = HostCallbacks {
            invoke_tool: &mut resume_invoke_cb,
            invoke_parallel_batch: &mut parallel_cb,
            search_catalog: &search_cb,
            describe_types: &describe_cb,
        };
        executor.resume_frame(
            &restored_frame,
            json!({"tx_id": "tx_999"}),
            &mut host,
            "00-00000000000000000000000000000001-0000000000000002-01",
        )
    };
    assert_eq!(resumed.status, "OK");
    assert_eq!(resumed.result["order_id"].as_str(), Some("ord_7"));
    assert_eq!(resumed.result["receipt"].as_str(), Some("tx_999"));
}

#[test]
fn test_okapi_bm25_idf_and_length_normalization() {
    // 1. IDF preference: "service" appears in all 3 docs (low IDF), while "settlement" appears in only doc 2 (high IDF).
    let corpus = vec![
        vec![(1.0, "service status check".to_string())],
        vec![(1.0, "service health ping".to_string())],
        vec![(1.0, "service ledger settlement".to_string())],
    ];
    let scores = CodeModeTypeGenerator::bm25_score_corpus("service settlement", &corpus);
    assert!(
        scores[2] > scores[0] && scores[2] > scores[1],
        "Document matching rare high-IDF term 'settlement' must rank highest: {:?}",
        scores
    );

    // 2. Length normalization (b = 0.75): two docs with identical TF = 1 for "refund",
    // but doc 0 is concise while doc 1 has many unrelated filler words.
    let len_corpus = vec![
        vec![(1.0, "refund payment".to_string())],
        vec![(
            1.0,
            "refund payment general utility helper miscellaneous extra verbose metadata"
                .to_string(),
        )],
    ];
    let len_scores = CodeModeTypeGenerator::bm25_score_corpus("refund", &len_corpus);
    assert!(
        len_scores[0] > len_scores[1],
        "Shorter document with equal TF must score higher under BM25 length normalization: {:?}",
        len_scores
    );

    // 3. Speculative 0-turn pre-injection threshold
    let mut srv = ServerRecord::new(
        "billing_service",
        "Billing Service",
        "Handles customer invoices and refunds",
        "fintech",
    );
    srv.protocol = ProtocolType::Grpc;
    srv.status = ToolStatus::Enabled;
    let mut t1 = ToolRecord::new(
        "issue_refund",
        "Issue refund for cancelled customer invoice",
        json!({"type": "object", "properties": {"invoice_id": {"type": "string"}}}),
    );
    t1.status = ToolStatus::Enabled;
    t1.tags = vec!["refund".to_string(), "billing".to_string()];
    srv.add_tool(t1);

    let spec = CodeModeTypeGenerator::speculative_pre_inject(
        &[srv],
        "issue customer billing refund for invoice",
        2,
        0.80,
    );
    assert_eq!(spec.routing_mode, "ZERO_TURN_SPECULATIVE");
    assert!(spec.confidence_score >= 0.80);
    assert!(spec.typescript_declarations.contains("issue_refund"));
}

#[test]
fn test_sandbox_memory_exhaustion_returns_32036_error_contract() {
    let limits = SandboxLimits {
        max_memory_bytes: 200,
        ..Default::default()
    };
    let script = r#"
        async () => {
            let big_str = "a very long string that will easily exceed the two hundred byte allocation limit configured for this sandbox execution test run";
            let arr = [big_str, big_str, big_str, big_str];
            return arr;
        }
        "#;
    let out = run_noop_script(script, limits);
    assert_eq!(out.error_code, Some(MEMORY_EXHAUSTED));
    assert_eq!(out.feature_status, FeatureStatus::MemoryExhausted);
    assert!(out.error.unwrap().contains("MemoryLimitExceeded"));
}

#[test]
fn test_sandbox_wallclock_timeout_returns_32037_error_contract() {
    let limits = SandboxLimits {
        max_execution_time_ms: 1,
        max_operations: 1_000_000,
        ..Default::default()
    };
    let script = r#"
        async () => {
            let mut sum = 0;
            let mut i = 0;
            while (i < 50000) {
                sum = sum + i;
                i = i + 1;
            }
            return sum;
        }
        "#;
    let out = run_noop_script(script, limits);
    assert_eq!(out.error_code, Some(TIMEOUT));
    assert_eq!(out.feature_status, FeatureStatus::Timeout);
    assert!(out.error.unwrap().contains("Timeout"));
}

#[test]
fn test_sandbox_panic_containment_does_not_abort_process() {
    let limits = SandboxLimits::default();
    let mut invoke_cb =
        |_srv: &str, _tool: &str, _args: Vec<JsonValue>| -> Result<JsonValue, CodeModeError> {
            panic!("intentional host callback panic to test sandbox containment");
        };
    let mut parallel_cb =
        |_calls: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
            Ok(Vec::new())
        };
    let search_cb = |_q: &str| JsonValue::Array(Vec::new());
    let describe_cb = |_s: &str, _t: Option<&str>| String::new();
    let mut host = HostCallbacks {
        invoke_tool: &mut invoke_cb,
        invoke_parallel_batch: &mut parallel_cb,
        search_catalog: &search_cb,
        describe_types: &describe_cb,
    };
    let script = r#"
        async () => {
            const res = await codemode.orders.get_order({ id: "ord_1" });
            return res;
        }
        "#;
    let out = CodeModeExecutor::new(limits).execute(
        script,
        &mut host,
        "00-00000000000000000000000000000001-0000000000000001-01",
    );
    assert_eq!(out.feature_status, FeatureStatus::Error);
    assert_eq!(out.error_code, Some(INTERNAL_ERROR));
    assert!(out.error.unwrap().contains("Panic:"));
}

#[test]
fn test_physical_expr_vectorized_vs_row_by_row_differential() {
    // Generate diverse dataset of 100 rows
    let mut raw_rows = Vec::new();
    for i in 0..100 {
        let score = ((i * 17) % 100) as f64;
        let tier = if i % 3 == 0 {
            "gold"
        } else if i % 3 == 1 {
            "silver"
        } else {
            "bronze"
        };
        let active = i % 2 == 0;
        raw_rows.push(json!({
            "id": format!("row_{}", i),
            "score": score,
            "tier": tier,
            "active": active
        }));
    }

    // Expected results evaluated row-by-row in Rust
    let expected: Vec<JsonValue> = raw_rows
        .iter()
        .filter(|r| {
            let score = r["score"].as_f64().unwrap();
            let tier = r["tier"].as_str().unwrap();
            let active = r["active"].as_bool().unwrap();
            (score >= 50.0 && tier == "gold") || active
        })
        .cloned()
        .collect();

    // Vectorized pushdown evaluated via CodeMode
    let script = format!(
        r#"
            async () => {{
                const rows = {};
                return rows.filter(r => (r.score >= 50.0 && r.tier == "gold") || r.active == true);
            }}
            "#,
        JsonValue::Array(raw_rows).to_string_compact()
    );

    let out = run_noop_script(&script, SandboxLimits::default());
    assert_eq!(out.status, "OK");
    assert!(
        out.vectorized_pushdowns > 0,
        "Expected vectorized pushdown execution"
    );
    let actual = out.result.as_array().expect("Result must be an array");
    assert_eq!(
        actual.len(),
        expected.len(),
        "Differential row count mismatch"
    );
    for (i, (act, exp)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            act["id"].as_str(),
            exp["id"].as_str(),
            "Row {} ID mismatch",
            i
        );
        assert_eq!(
            act["score"].as_f64(),
            exp["score"].as_f64(),
            "Row {} score mismatch",
            i
        );
        assert_eq!(
            act["tier"].as_str(),
            exp["tier"].as_str(),
            "Row {} tier mismatch",
            i
        );
        assert_eq!(
            act["active"].as_bool(),
            exp["active"].as_bool(),
            "Row {} active mismatch",
            i
        );
    }
}

#[test]
fn test_codemode_v2_typegen_and_vm_features() {
    // 1. Recursive $ref / $defs, enum, oneOf, typed arrays
    let schema = json!({
        "$defs": {
            "Status": { "enum": ["open", "closed"] }
        },
        "type": "object",
        "required": ["status", "tags"],
        "properties": {
            "status": { "$ref": "#/$defs/Status" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "payload": { "oneOf": [{"type": "string"}, {"type": "number"}] }
        }
    });
    let ts = CodeModeTypeGenerator::json_schema_to_ts_type(&schema);
    assert!(ts.contains("\"open\" | \"closed\""), "got: {}", ts);
    assert!(ts.contains("string[]"), "got: {}", ts);
    assert!(ts.contains("string | number"), "got: {}", ts);

    // 2. VM: template literals, destructuring, try/catch/finally, Promise.all
    let mut invoke =
        |_srv: &str, tool: &str, _args: Vec<JsonValue>| -> Result<JsonValue, CodeModeError> {
            Ok(json!({"tool": tool, "ok": true}))
        };
    let mut batch =
        |tasks: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
            Ok(tasks
                .into_iter()
                .map(|(_, t, _)| json!({"tool": t}))
                .collect())
        };
    let search = |_: &str| -> JsonValue { JsonValue::Array(vec![]) };
    let desc = |_: &str, _: Option<&str>| -> String { String::new() };
    let mut host = HostCallbacks {
        invoke_tool: &mut invoke,
        invoke_parallel_batch: &mut batch,
        search_catalog: &search,
        describe_types: &desc,
    };
    let exec = CodeModeExecutor::new(SandboxLimits::default());
    let code = r#"
            const { a, b: renamed } = { a: 10, b: 20 };
            const [x, y] = [1, 2];
            let caught = "";
            let fin = false;
            try {
                throw "boom";
            } catch (err) {
                caught = err;
            } finally {
                fin = true;
            }
            const res = Promise.all([
                codemode.srv.t1({}),
                codemode.srv.t2({})
            ]);
            return {
                msg: `sum=${a + renamed + x + y}`,
                caught: caught,
                fin: fin,
                batchLen: res.length
            };
        "#;
    let out = exec.execute(code, &mut host, "");
    assert_eq!(out.status, "OK", "error: {:?}", out.error);
    assert_eq!(out.result["msg"].as_str(), Some("sum=33"));
    assert_eq!(out.result["caught"].as_str(), Some("boom"));
    assert_eq!(out.result["fin"].as_bool(), Some(true));
    assert_eq!(out.result["batchLen"].as_i64(), Some(2));
}
