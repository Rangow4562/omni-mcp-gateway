//! Comprehensive `v6.5.0` End-to-End Integration & Verification Suite (all 8 upgrade pillars).

use omni_mcp::config::OmniConfig;
use omni_mcp::control_plane::MCPRegistry;
use omni_mcp::data_plane::{
    CodeModeTypeGenerator, GrpcDownstreamClient, HttpDownstreamClient, InMemoryConfigCache,
    MCPProxyGateway, ProtocolTranslator, SemanticResponseCache, TenantQuotaTracker,
};
use omni_mcp::extension::OmniMCPServer;
use omni_mcp::json;
use omni_mcp::json::JsonValue;
#[cfg(feature = "transport-http")]
use omni_mcp::models::ProtocolType;
#[cfg(feature = "storage-postgres")]
use omni_mcp::models::SuspendedExecutionFrame;
use omni_mcp::models::{
    ArgumentPolicyRule, RPCRequest, SamplingElicitationPolicy, ServerRecord, SessionMode,
    TenantQuotaPolicy, ToolRecord, ToolStatus, TraceSpan, RATE_LIMITED, UNAUTHORIZED,
};
use omni_mcp::server::ProductionGatewayContext;
#[cfg(feature = "storage-postgres")]
use omni_mcp::storage::{AuditSink, MerkleAuditStore, RegistryStorageBackend};
use omni_mcp::telemetry::OtlpBatchWorker;
use omni_mcp::upstream::{
    clear_mock_http, register_mock_http, HttpUpstreamSession, OAuthTokenRecord,
    UpstreamCallbackHub, UpstreamOAuthTokenStore,
};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temp_test_dir(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{}_{}_{}", prefix, std::process::id(), nanos));
    let _ = fs::create_dir_all(&dir);
    dir.to_string_lossy().into_owned()
}

// ============================================================================
// Pillar 1: Real PostgreSQL Wire Protocol v3.0, Schema, Atomic CAS & Audit Chain
// ============================================================================

#[cfg(feature = "storage-postgres")]
#[test]
fn test_v6_5_postgres_backend_schema_and_cas() {
    use omni_mcp::storage::{PgDsn, PostgresRegistryBackend};

    // 1. DSN parser verification
    let parsed = PgDsn::parse("postgres://omni:s3cr3t@127.0.0.1:5433/omnimcp?sslmode=disable")
        .expect("Valid DSN must parse");
    assert_eq!(parsed.user, "omni");
    assert_eq!(parsed.password, "s3cr3t");
    assert_eq!(parsed.host, "127.0.0.1");
    assert_eq!(parsed.port, 5433);
    assert_eq!(parsed.database, "omnimcp");
    assert_eq!(parsed.sslmode, "disable");
    assert!(PgDsn::parse("mysql://bad:5432/db").is_err());

    // 2. Spin up an in-process PostgreSQL Wire Protocol v3.0 TCP server on 127.0.0.1:0
    #[derive(Default)]
    struct MockPgDb {
        schema_initialized: bool,
        servers: BTreeMap<String, String>,
        frames: BTreeMap<String, String>,
        audit: BTreeMap<i64, (String, String)>,
        notify_txs: Vec<std::sync::mpsc::Sender<(String, String)>>,
    }

    let db = Arc::new(Mutex::new(MockPgDb::default()));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock PG server");
    let addr = listener.local_addr().unwrap();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_srv = stop_flag.clone();
    let db_srv = db.clone();

    let _ = listener.set_nonblocking(true);
    let srv_handle = thread::spawn(move || {
        fn send_msg(s: &mut std::net::TcpStream, kind: u8, body: &[u8]) -> std::io::Result<()> {
            s.write_all(&[kind])?;
            s.write_all(&((4 + body.len()) as u32).to_be_bytes())?;
            s.write_all(body)?;
            Ok(())
        }
        fn send_data_row(s: &mut std::net::TcpStream, cols: &[&str]) -> std::io::Result<()> {
            let mut body = Vec::new();
            body.extend_from_slice(&(cols.len() as u16).to_be_bytes());
            for c in cols {
                body.extend_from_slice(&(c.len() as i32).to_be_bytes());
                body.extend_from_slice(c.as_bytes());
            }
            send_msg(s, b'D', &body)
        }

        while !stop_srv.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                    let db_c = db_srv.clone();
                    let stop_c = stop_srv.clone();
                    thread::spawn(move || {
                        // Read StartupMessage (length + code + params)
                        let mut len_buf = [0u8; 4];
                        if stream.read_exact(&mut len_buf).is_err() {
                            return;
                        }
                        let len = u32::from_be_bytes(len_buf) as usize;
                        let mut startup = vec![0u8; len.saturating_sub(4)];
                        if stream.read_exact(&mut startup).is_err() {
                            return;
                        }
                        // Send AuthenticationOk + ReadyForQuery ('I')
                        let _ = send_msg(&mut stream, b'R', &0u32.to_be_bytes());
                        let _ = send_msg(&mut stream, b'Z', b"I");
                        let _ = stream.flush();

                        let mut last_sql = String::new();
                        let mut last_params: Vec<String> = Vec::new();
                        let mut listen_rx: Option<std::sync::mpsc::Receiver<(String, String)>> =
                            None;

                        while !stop_c.load(Ordering::Relaxed) {
                            if let Some(ref rx) = listen_rx {
                                while let Ok((ch, pl)) = rx.try_recv() {
                                    let mut nbody = Vec::new();
                                    nbody.extend_from_slice(&42i32.to_be_bytes());
                                    nbody.extend_from_slice(ch.as_bytes());
                                    nbody.push(0);
                                    nbody.extend_from_slice(pl.as_bytes());
                                    nbody.push(0);
                                    let _ = send_msg(&mut stream, b'A', &nbody);
                                    let _ = stream.flush();
                                }
                            }
                            let mut hdr = [0u8; 5];
                            match stream.read_exact(&mut hdr) {
                                Ok(()) => {}
                                Err(ref e)
                                    if e.kind() == std::io::ErrorKind::TimedOut
                                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                                {
                                    continue;
                                }
                                Err(_) => break,
                            }
                            let mtype = hdr[0];
                            let mlen =
                                u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
                            let mut body = vec![0u8; mlen.saturating_sub(4)];
                            if !body.is_empty() && stream.read_exact(&mut body).is_err() {
                                break;
                            }
                            match mtype {
                                b'Q' => {
                                    let sql = String::from_utf8_lossy(&body)
                                        .trim_end_matches('\0')
                                        .to_string();
                                    if sql.contains("CREATE TABLE IF NOT EXISTS omni_servers") {
                                        db_c.lock().unwrap().schema_initialized = true;
                                    } else if sql.starts_with("LISTEN ") {
                                        let (tx, rx) = std::sync::mpsc::channel();
                                        db_c.lock().unwrap().notify_txs.push(tx);
                                        listen_rx = Some(rx);
                                    } else if let Some(rest) = sql.strip_prefix("NOTIFY ") {
                                        let parts: Vec<&str> = rest.splitn(2, ',').collect();
                                        let ch = parts[0].trim().to_string();
                                        let pl = parts
                                            .get(1)
                                            .map(|s| s.trim().trim_matches('\'').to_string())
                                            .unwrap_or_default();
                                        let txs = db_c.lock().unwrap().notify_txs.clone();
                                        for tx in txs {
                                            let _ = tx.send((ch.clone(), pl.clone()));
                                        }
                                    }
                                    let _ = send_msg(&mut stream, b'C', b"OK\0");
                                    let _ = send_msg(&mut stream, b'Z', b"I");
                                    let _ = stream.flush();
                                }
                                b'P' => {
                                    // Parse: stmt\0 query\0 ...
                                    let parts: Vec<&[u8]> = body.split(|&b| b == 0).collect();
                                    if parts.len() >= 2 {
                                        last_sql = String::from_utf8_lossy(parts[1]).into_owned();
                                    }
                                    let _ = send_msg(&mut stream, b'1', &[]);
                                }
                                b'B' => {
                                    // Bind: portal\0 stmt\0 i16(0) i16(N) [i32 len + bytes]...
                                    let mut pos = 2; // skip two empty NUL strings
                                    if pos + 4 <= body.len() {
                                        let fmt_count =
                                            u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
                                        pos += 2 + fmt_count * 2;
                                        let p_count =
                                            u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
                                        pos += 2;
                                        last_params.clear();
                                        for _ in 0..p_count {
                                            if pos + 4 > body.len() {
                                                break;
                                            }
                                            let plen = i32::from_be_bytes([
                                                body[pos],
                                                body[pos + 1],
                                                body[pos + 2],
                                                body[pos + 3],
                                            ])
                                                as usize;
                                            pos += 4;
                                            let end = (pos + plen).min(body.len());
                                            last_params.push(
                                                String::from_utf8_lossy(&body[pos..end])
                                                    .into_owned(),
                                            );
                                            pos = end;
                                        }
                                    }
                                    let _ = send_msg(&mut stream, b'2', &[]);
                                }
                                b'E' => {
                                    let mut g = db_c.lock().unwrap();
                                    if last_sql.starts_with("INSERT INTO omni_servers") {
                                        g.servers
                                            .insert(last_params[0].clone(), last_params[1].clone());
                                    } else if last_sql
                                        .starts_with("SELECT payload::text FROM omni_servers")
                                    {
                                        for payload in g.servers.values() {
                                            let _ = send_data_row(&mut stream, &[payload.as_str()]);
                                        }
                                    } else if last_sql.starts_with("DELETE FROM omni_servers") {
                                        if let Some(p) = g.servers.remove(&last_params[0]) {
                                            let _ = send_data_row(&mut stream, &[p.as_str()]);
                                        }
                                    } else if last_sql
                                        .starts_with("INSERT INTO omni_suspended_frames")
                                    {
                                        g.frames
                                            .insert(last_params[0].clone(), last_params[1].clone());
                                    } else if last_sql
                                        .starts_with("DELETE FROM omni_suspended_frames")
                                    {
                                        // Atomic single-winner DELETE ... RETURNING
                                        if let Some(p) = g.frames.remove(&last_params[0]) {
                                            let _ = send_data_row(&mut stream, &[p.as_str()]);
                                        }
                                    } else if last_sql.starts_with("INSERT INTO omni_audit_log") {
                                        let idx: i64 = last_params[0].parse().unwrap_or(0);
                                        g.audit.insert(
                                            idx,
                                            (last_params[1].clone(), last_params[2].clone()),
                                        );
                                    } else if last_sql.starts_with(
                                        "SELECT current_hash, payload::text FROM omni_audit_log",
                                    ) {
                                        for (h, p) in g.audit.values() {
                                            let _ = send_data_row(
                                                &mut stream,
                                                &[h.as_str(), p.as_str()],
                                            );
                                        }
                                    }
                                    let _ = send_msg(&mut stream, b'C', b"OK\0");
                                }
                                b'S' => {
                                    let _ = send_msg(&mut stream, b'Z', b"I");
                                    let _ = stream.flush();
                                }
                                _ => {}
                            }
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    // 3. Connect real PostgresRegistryBackend over TCP wire protocol v3.0
    let dsn = format!(
        "postgres://omni:secret@127.0.0.1:{}/omnimcp?sslmode=disable",
        addr.port()
    );
    let pg = Arc::new(PostgresRegistryBackend::connect(&dsn).expect("connect to mock PG wire"));
    assert!(
        db.lock().unwrap().schema_initialized,
        "SCHEMA_SQL must be executed on connect"
    );

    // 4. Verify LISTEN/NOTIFY invalidation callback fires on mutation
    let notify_received = Arc::new(Mutex::new(Vec::<String>::new()));
    let notify_cb = notify_received.clone();
    pg.start_invalidation_listener(move |payload| {
        notify_cb.lock().unwrap().push(payload.to_string());
    })
    .expect("start LISTEN/NOTIFY");
    thread::sleep(Duration::from_millis(80));

    let srv = ServerRecord::new("pg_cluster_srv", "PG Cluster", "Distributed", "infra");
    pg.save_server(&srv);
    let loaded = pg.load_all_servers();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].name, "pg_cluster_srv");

    // Wait briefly for LISTEN/NOTIFY delivery
    for _ in 0..20 {
        if !notify_received.lock().unwrap().is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        notify_received
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("pg_cluster_srv")),
        "LISTEN/NOTIFY must deliver invalidation payload; got {:?}",
        notify_received.lock().unwrap()
    );

    // 5. Distributed Atomic CAS: 4 concurrent replicas attempt consume_frame_cas -> strictly 1 winner
    let frame = SuspendedExecutionFrame {
        execution_id: "exec_pg_cas_01".to_string(),
        script_hash: "h".to_string(),
        script_source: "async () => 1".to_string(),
        instruction_ptr: 0,
        target_var: None,
        serialized_scope: json!({}),
        pending_server: "payments".to_string(),
        pending_tool: "refund".to_string(),
        pending_arguments: json!({"amount": 50}),
        pending_reason: "HITL".to_string(),
        policy_context_hash: "p".to_string(),
        caller_actor: "agent_requester".to_string(),
        logs: Vec::new(),
        completed_mutations: Vec::new(),
        created_at: 1.0,
        pending_arguments_hash: "ah".to_string(),
        expires_at: 9_999_999_999.0,
        approver_identity: None,
        is_consumed: false,
        frame_hmac: "hmac".to_string(),
    };
    pg.save_suspended_frame(&frame);
    let mut handles = Vec::new();
    for i in 0..4 {
        let b = pg.clone();
        handles.push(thread::spawn(move || {
            b.consume_frame_cas("exec_pg_cas_01", &format!("approver_{}", i))
        }));
    }
    let winners = handles
        .into_iter()
        .map(|h| h.join().unwrap().is_ok())
        .filter(|ok| *ok)
        .count();
    assert_eq!(
        winners, 1,
        "Atomic PostgreSQL DELETE ... RETURNING CAS must yield strictly 1 winner"
    );

    // 6. AuditSink cryptographic hash-chain verification & tamper detection
    let audit = MerkleAuditStore::new(None);
    let r1 = audit.record_invocation("alice", "pg_cluster_srv", "op1", &json!({}), "OK", 10);
    let r2 = audit.record_invocation("bob", "pg_cluster_srv", "op2", &json!({}), "OK", 20);
    pg.emit_record(&r1).unwrap();
    pg.emit_record(&r2).unwrap();
    assert_eq!(pg.verify_integrity(), (true, 2));

    let mut tampered = r2.clone();
    tampered.actor = "mallory".to_string();
    pg.emit_record(&tampered).unwrap();
    assert_eq!(
        pg.verify_integrity(),
        (false, 1),
        "Postgres AuditSink must detect payload tampering"
    );

    // 7. Verify dynamic ProductionGatewayContext::new() selects postgres backend cleanly
    let dir = temp_test_dir("omni_v65_pg_ctx");
    let ctx = ProductionGatewayContext::new(Some(OmniConfig {
        data_dir: dir.clone(),
        storage_backend: "postgres".to_string(),
        database_url: ":memory:".to_string(),
        demo_seed: false,
        ..Default::default()
    }));
    assert_eq!(ctx.storage_backend_name, "postgres");

    pg.stop_invalidation_listener();
    stop_flag.store(true, Ordering::Relaxed);
    let _ = srv_handle.join();
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================================
// Pillar 2: Wasm Guest ABI v1 (JSON Linear Memory, omni::call_tool, LRU Cache, WasmPlugin)
// ============================================================================

#[cfg(feature = "sandbox-wasm")]
#[test]
fn test_v6_5_wasm_guest_abi_json_and_host_tool_call() {
    use omni_mcp::data_plane::codemode::{WasmSandboxExecutor, WasmToolCallback};

    // Valid WebAssembly module implementing Omni Wasm Guest ABI v1:
    // - Imports:
    //     func 0: ("omni", "log_utf8") (param i32 i32)
    //     func 1: ("omni", "call_tool") (param i32 i32) -> (result i64)
    // - Memory: 1 page (exported as "memory"), bump pointer in global 0 (initial 1024)
    // - Exports:
    //     "memory"
    //     "omni_alloc" (param i32) -> (result i32)  [bump allocator]
    //     "omni_run" (param i32 i32) -> (result i64) [calls log_utf8(in_ptr, in_len), then returns call_tool(in_ptr, in_len)]
    let mut wasm: Vec<u8> = vec![
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + v1
    ];
    let push_section = |out: &mut Vec<u8>, id: u8, payload: &[u8]| {
        out.push(id);
        out.push(payload.len() as u8);
        out.extend_from_slice(payload);
    };
    // Type section (id 1):
    // type 0: (i32, i32) -> ()
    // type 1: (i32, i32) -> (i64)
    // type 2: (i32) -> (i32)
    push_section(
        &mut wasm,
        1,
        &[
            0x03, // 3 types
            0x60, 0x02, 0x7f, 0x7f, 0x00, // type 0
            0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7e, // type 1
            0x60, 0x01, 0x7f, 0x01, 0x7f, // type 2
        ],
    );
    // Import section (id 2): 2 imports
    let mut imports = vec![0x02];
    // ("omni", "log_utf8") func type 0
    imports.extend_from_slice(&[0x04, b'o', b'm', b'n', b'i', 0x08]);
    imports.extend_from_slice(b"log_utf8");
    imports.extend_from_slice(&[0x00, 0x00]);
    // ("omni", "call_tool") func type 1
    imports.extend_from_slice(&[0x04, b'o', b'm', b'n', b'i', 0x09]);
    imports.extend_from_slice(b"call_tool");
    imports.extend_from_slice(&[0x00, 0x01]);
    push_section(&mut wasm, 2, &imports);

    // Function section (id 3): 2 defined funcs: func 2 = type 2 (omni_alloc), func 3 = type 1 (omni_run)
    push_section(&mut wasm, 3, &[0x02, 0x02, 0x01]);

    // Memory section (id 5): 1 memory, min 1 page
    push_section(&mut wasm, 5, &[0x01, 0x00, 0x01]);

    // Global section (id 6): 1 mutable i32 global initialized to 1024 (LEB128 0x80 0x08)
    push_section(&mut wasm, 6, &[0x01, 0x7f, 0x01, 0x41, 0x80, 0x08, 0x0b]);

    // Export section (id 7): 3 exports ("memory", "omni_alloc" -> func 2, "omni_run" -> func 3)
    let mut exports = vec![0x03];
    exports.extend_from_slice(&[0x06]);
    exports.extend_from_slice(b"memory");
    exports.extend_from_slice(&[0x02, 0x00]);
    exports.extend_from_slice(&[0x0a]);
    exports.extend_from_slice(b"omni_alloc");
    exports.extend_from_slice(&[0x00, 0x02]);
    exports.extend_from_slice(&[0x08]);
    exports.extend_from_slice(b"omni_run");
    exports.extend_from_slice(&[0x00, 0x03]);
    push_section(&mut wasm, 7, &exports);

    // Code section (id 10): 2 function bodies
    // func 2 (omni_alloc): local 1 (i32 ptr) = global.get 0; global.set 0 (ptr + param 0); return ptr
    let body_alloc: Vec<u8> = vec![
        0x01, 0x01, 0x7f, // 1 local of type i32 (index 1)
        0x23, 0x00, // global.get 0
        0x22, 0x01, // local.tee 1
        0x20, 0x00, // local.get 0
        0x6a, // i32.add
        0x24, 0x00, // global.set 0
        0x20, 0x01, // local.get 1
        0x0b, // end
    ];
    // func 3 (omni_run): call 0 (log_utf8(p0, p1)); call 1 (call_tool(p0, p1))
    let body_run: Vec<u8> = vec![
        0x00, // 0 locals
        0x20, 0x00, // local.get 0
        0x20, 0x01, // local.get 1
        0x10, 0x00, // call 0 (omni::log_utf8)
        0x20, 0x00, // local.get 0
        0x20, 0x01, // local.get 1
        0x10, 0x01, // call 1 (omni::call_tool)
        0x0b, // end
    ];
    let mut code = vec![0x02];
    code.push(body_alloc.len() as u8);
    code.extend_from_slice(&body_alloc);
    code.push(body_run.len() as u8);
    code.extend_from_slice(&body_run);
    push_section(&mut wasm, 10, &code);

    let exec = WasmSandboxExecutor::new(200_000, 4 * 1024 * 1024);
    let cb: WasmToolCallback = Arc::new(|srv, tool, args| {
        Ok(json!({
            "bridged_server": srv,
            "bridged_tool": tool,
            "echo_email": args["email"].clone(),
            "amount": args["amount"].clone(),
        }))
    });

    let input = json!({
        "server": "payments",
        "tool": "quote_fee",
        "arguments": {
            "email": "alice@example.com",
            "amount": 125
        }
    });

    let (hits_before, _) = WasmSandboxExecutor::module_cache_stats();
    let out1 = exec.execute_wasm_abi_v1(&wasm, &input, Some(cb.clone()));
    assert_eq!(out1.status, "OK", "Wasm ABI v1 failed: {:?}", out1.error);
    assert_eq!(out1.tool_calls, 1);
    assert_eq!(out1.result["ok"].as_bool(), Some(true));
    assert_eq!(
        out1.result["result"]["bridged_server"].as_str(),
        Some("payments")
    );
    assert_eq!(
        out1.result["result"]["bridged_tool"].as_str(),
        Some("quote_fee")
    );
    assert_eq!(
        out1.result["result"]["echo_email"].as_str(),
        Some("[REDACTED_EMAIL]"),
        "PII in Wasm tool result must be redacted"
    );
    assert!(
        out1.logs.iter().any(|l| l.contains("[REDACTED_EMAIL]")),
        "omni::log_utf8 must capture & redact PII: {:?}",
        out1.logs
    );

    // Second invocation must hit the SHA-256 compiled module LRU cache
    let out2 = exec.execute_wasm_abi_v1(&wasm, &input, Some(cb));
    assert_eq!(out2.status, "OK");
    let (hits_after, _) = WasmSandboxExecutor::module_cache_stats();
    assert!(
        hits_after > hits_before,
        "Warm Wasm execution must hit compiled module LRU cache"
    );

    // Register via REST API POST /api/v1/wasm-tools and execute via MCPProxyGateway
    let dir = temp_test_dir("omni_v65_wasm_plugin");
    let ctx = ProductionGatewayContext::new(Some(OmniConfig {
        data_dir: dir.clone(),
        demo_seed: true,
        ..Default::default()
    }));
    let wasm_b64 = encode_base64_for_test(&wasm);
    let mut admin_hdrs = HashMap::new();
    admin_hdrs.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );
    let reg_body = json!({
        "server_name": "wasm-plugins",
        "tool_name": "bridge_driver",
        "description": "Zero-process Wasm tool plugin",
        "wasm_module_base64": wasm_b64
    });
    let (status, _, _, resp_bytes) = ctx.handle_http(
        "POST",
        "/api/v1/wasm-tools",
        &admin_hdrs,
        reg_body.to_string_compact().as_bytes(),
    );
    assert_eq!(
        status,
        200,
        "Register Wasm tool failed: {}",
        String::from_utf8_lossy(&resp_bytes)
    );

    // Invoke the registered ProtocolType::WasmPlugin tool via MCPProxyGateway
    let call_req = RPCRequest::new(
        1,
        "tools/call",
        json!({
            "name": "bridge_driver",
            "arguments": {
                "server": "driverdispatchservice",
                "tool": "GetDriverLocation",
                "arguments": {"driver_id": "drv_wasm_1", "include_telemetry": false}
            }
        }),
    );
    let rpc_resp = ctx
        .gateway
        .handle_request("wasm-plugins", &call_req, "agent", None);
    assert!(
        rpc_resp.error.is_none(),
        "WasmPlugin tool call failed: {:?}",
        rpc_resp.error
    );
    let res_text = rpc_resp.result.unwrap().to_string_compact();
    assert!(
        res_text.contains("\"ok\":true"),
        "Expected governed WasmPlugin host bridge result, got: {}",
        res_text
    );
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================================
// Pillar 3A: Non-Blocking Async HTTPS Listener & Concurrent SSE Stream
// ============================================================================

#[cfg(all(feature = "async-runtime", feature = "tls-rustls"))]
#[test]
fn test_v6_5_async_https_listener_and_sse_nonblocking() {
    let (cert_pem, key_pem, cert_der) = generate_test_ed25519_cert();
    let server_tls = omni_mcp::server::build_inbound_tls_config_from_pem(&cert_pem, &key_pem, None)
        .expect("build ServerConfig");

    let dir = temp_test_dir("omni_v65_async_https");
    let ctx = Arc::new(ProductionGatewayContext::new(Some(OmniConfig {
        data_dir: dir.clone(),
        demo_seed: true,
        ..Default::default()
    })));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .unwrap();

    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind async HTTPS loopback");
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let srv_ctx = ctx.clone();
        let srv_task = tokio::spawn(async move {
            omni_mcp::server::serve_async_listener(listener, srv_ctx, Some(server_tls), shutdown_rx)
                .await;
        });

        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(cert_der))
            .unwrap();
        let client_cfg = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        // Client 1: Open long-lived HTTPS SSE stream (/api/v1/events)
        let sse_cfg = client_cfg.clone();
        let sse_handle = thread::spawn(move || {
            let tcp = std::net::TcpStream::connect(addr).expect("connect SSE HTTPS");
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(5)));
            let sni = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let conn = rustls::ClientConnection::new(sse_cfg, sni).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let req = "GET /api/v1/events HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer admin_dev_token\r\n\r\n";
            tls.write_all(req.as_bytes()).unwrap();
            tls.flush().unwrap();

            let mut reader = BufReader::new(tls);
            let mut collected = String::new();
            for _ in 0..30 {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        collected.push_str(&line);
                        if collected.contains("event: v65.https_sse_ping") {
                            break;
                        }
                    }
                }
            }
            collected
        });

        // Give SSE client 150ms to complete TLS handshake and subscribe, then publish event AND run concurrent HTTPS POST /omni/mcp
        tokio::time::sleep(Duration::from_millis(150)).await;
        ctx.event_bus
            .publish("v65.https_sse_ping", json!({"ok": true}));

        // Client 2: Concurrent HTTPS POST /omni/mcp while SSE stream is open
        let mcp_cfg = client_cfg.clone();
        let mcp_resp = tokio::task::spawn_blocking(move || {
            let tcp = std::net::TcpStream::connect(addr).expect("connect MCP HTTPS");
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(5)));
            let sni = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
            let conn = rustls::ClientConnection::new(mcp_cfg, sni).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
            let req = format!(
                "POST /omni/mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            tls.write_all(req.as_bytes()).unwrap();
            tls.flush().unwrap();
            let mut resp = String::new();
            let _ = tls.read_to_string(&mut resp);
            resp
        })
        .await
        .unwrap();

        assert!(
            mcp_resp.starts_with("HTTP/1.1 200 OK") && mcp_resp.contains("discover_server"),
            "Concurrent HTTPS POST /omni/mcp must succeed: {}",
            mcp_resp
        );

        let sse_data = sse_handle.join().unwrap();
        assert!(
            sse_data.contains("text/event-stream")
                && sse_data.contains("event: v65.https_sse_ping"),
            "Non-blocking HTTPS SSE must deliver live pushed event: {}",
            sse_data
        );

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), srv_task).await;
    });

    let _ = fs::remove_dir_all(&dir);
}

// ============================================================================
// Pillar 3B: Full REST Verbs, OpenAPI URL Templates, HTTPS Routing & Live H2 gRPC/Connect
// ============================================================================

#[test]
fn test_v6_5_downstream_rest_verbs_url_templates_and_https() {
    // 1. Path traversal rejection on URL templates
    let traversal = HttpDownstreamClient::interpolate_url_template(
        "http://127.0.0.1:8080/v1/items/{id}",
        &json!({"id": "../../etc/passwd"}),
        true,
    );
    assert!(
        traversal.is_err(),
        "Path traversal in URL template must be rejected"
    );

    // 2. Local loopback HTTP/1.1 REST echo server
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock REST server");
    let rest_addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    let _ = listener.set_nonblocking(true);

    let rest_srv = thread::spawn(move || {
        while !stop_c.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let parts: Vec<&str> = req_line.split_whitespace().collect();
                    let method = parts.first().copied().unwrap_or("").to_string();
                    let path = parts.get(1).copied().unwrap_or("").to_string();
                    let mut cl = 0usize;
                    let mut connect_ver = String::new();
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            if k.eq_ignore_ascii_case("content-length") {
                                cl = v.trim().parse().unwrap_or(0);
                            } else if k.eq_ignore_ascii_case("connect-protocol-version") {
                                connect_ver = v.trim().to_string();
                            }
                        }
                    }
                    let mut body = vec![0u8; cl];
                    if cl > 0 {
                        let _ = reader.read_exact(&mut body);
                    }
                    let resp_json = json!({
                        "method": method,
                        "path": path,
                        "body": String::from_utf8_lossy(&body).into_owned(),
                        "connect_protocol_version": connect_ver
                    })
                    .to_string_compact();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_json.len(),
                        resp_json
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let client = HttpDownstreamClient::new();
    // GET /v1/orders/{id}?filter=active
    let get_bytes = client
        .request(
            "GET",
            &format!("http://127.0.0.1:{}/v1/orders/{{id}}", rest_addr.port()),
            &json!({"id": "ord 42", "filter": "active"}),
            None,
        )
        .expect("GET with template & query must succeed");
    let get_json = JsonValue::parse(&String::from_utf8(get_bytes).unwrap()).unwrap();
    assert_eq!(get_json["method"].as_str(), Some("GET"));
    assert_eq!(
        get_json["path"].as_str(),
        Some("/v1/orders/ord%2042?filter=active")
    );

    // PUT /v1/orders/{id} with remaining fields in JSON body
    let put_bytes = client
        .request(
            "PUT",
            &format!("http://127.0.0.1:{}/v1/orders/{{id}}", rest_addr.port()),
            &json!({"id": "ord99", "status": "shipped"}),
            None,
        )
        .expect("PUT with template & JSON body must succeed");
    let put_json = JsonValue::parse(&String::from_utf8(put_bytes).unwrap()).unwrap();
    assert_eq!(put_json["method"].as_str(), Some("PUT"));
    assert_eq!(put_json["path"].as_str(), Some("/v1/orders/ord99"));
    assert_eq!(put_json["body"].as_str(), Some(r#"{"status":"shipped"}"#));

    // DELETE /v1/orders/{id}
    let del_bytes = client
        .request(
            "DELETE",
            &format!("http://127.0.0.1:{}/v1/orders/{{id}}", rest_addr.port()),
            &json!({"id": "ord99"}),
            None,
        )
        .expect("DELETE must succeed");
    let del_json = JsonValue::parse(&String::from_utf8(del_bytes).unwrap()).unwrap();
    assert_eq!(del_json["method"].as_str(), Some("DELETE"));

    // Verify Connect-RPC invocation over HTTP/1.1 via GrpcDownstreamClient
    let grpc_client = GrpcDownstreamClient::new();
    let connect_bytes = grpc_client
        .invoke(
            &format!(
                "http://127.0.0.1:{}/payments.v1.Ledger/Charge",
                rest_addr.port()
            ),
            "payments.v1.Ledger/Charge",
            &json!({"amount": 42}),
            None,
        )
        .expect("Connect-RPC invoke must succeed");
    let connect_json = JsonValue::parse(&String::from_utf8(connect_bytes).unwrap()).unwrap();
    assert_eq!(connect_json["connect_protocol_version"].as_str(), Some("1"));
    assert_eq!(
        connect_json["path"].as_str(),
        Some("/payments.v1.Ledger/Charge")
    );

    // 3. Verify live HTTP/2 gRPC (h2c) wire framing against a local loopback H2 server
    let h2_listener = TcpListener::bind("127.0.0.1:0").expect("bind mock H2 gRPC server");
    let h2_addr = h2_listener.local_addr().unwrap();
    let h2_srv = thread::spawn(move || {
        if let Ok((mut stream, _)) = h2_listener.accept() {
            let mut preface = [0u8; 24];
            stream.read_exact(&mut preface).unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            // Read client SETTINGS, HEADERS, DATA
            let mut req_payload = Vec::new();
            loop {
                let (ftype, flags, _sid, body) =
                    omni_mcp::data_plane::downstream_grpc::read_h2_frame(&mut stream).unwrap();
                if ftype == 0x00 {
                    req_payload.extend_from_slice(&body);
                }
                if (flags & 0x01) != 0 && ftype == 0x00 {
                    break;
                }
            }
            let decoded_req = GrpcDownstreamClient::decode_grpc_frame(&req_payload).unwrap();
            let req_text = String::from_utf8_lossy(&decoded_req);
            // Send server SETTINGS, response HEADERS (:status 200), gRPC DATA, and trailers (grpc-status 0)
            omni_mcp::data_plane::downstream_grpc::write_h2_frame(&mut stream, 0x04, 0, 0, &[])
                .unwrap();
            let mut resp_hdrs = Vec::new();
            omni_mcp::data_plane::downstream_grpc::encode_hpack_literal(
                &mut resp_hdrs,
                ":status",
                "200",
            );
            omni_mcp::data_plane::downstream_grpc::encode_hpack_literal(
                &mut resp_hdrs,
                "content-type",
                "application/grpc+json",
            );
            omni_mcp::data_plane::downstream_grpc::write_h2_frame(
                &mut stream,
                0x01,
                0x04,
                1,
                &resp_hdrs,
            )
            .unwrap();
            let resp_body = format!(r#"{{"h2_wire":true,"echo":{}}}"#, req_text);
            let framed = GrpcDownstreamClient::encode_grpc_frame(resp_body.as_bytes());
            omni_mcp::data_plane::downstream_grpc::write_h2_frame(&mut stream, 0x00, 0, 1, &framed)
                .unwrap();
            let mut trailers = Vec::new();
            omni_mcp::data_plane::downstream_grpc::encode_hpack_literal(
                &mut trailers,
                "grpc-status",
                "0",
            );
            omni_mcp::data_plane::downstream_grpc::write_h2_frame(
                &mut stream,
                0x01,
                0x05,
                1,
                &trailers,
            )
            .unwrap();
            let _ = stream.flush();
            // Half-close write side and drain any client SETTINGS ACK to avoid Windows TCP RST (10053)
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut drain = [0u8; 64];
            while stream.read(&mut drain).unwrap_or(0) > 0 {}
        }
    });

    let h2_bytes = grpc_client
        .invoke(
            &format!("grpc://127.0.0.1:{}/fleet.Driver/Locate", h2_addr.port()),
            "fleet.Driver/Locate",
            &json!({"driver_id": "d_h2"}),
            None,
        )
        .expect("Live HTTP/2 gRPC h2c call must succeed");
    let h2_json = JsonValue::parse(&String::from_utf8(h2_bytes).unwrap()).unwrap();
    assert_eq!(h2_json["h2_wire"].as_bool(), Some(true));
    assert_eq!(h2_json["echo"]["driver_id"].as_str(), Some("d_h2"));
    let _ = h2_srv.join();

    // 4. Verify ProtocolTranslator routes https:// endpoints to HttpDownstreamClient (not MuttleyMeshClient)
    #[cfg(feature = "transport-http")]
    {
        let translator = ProtocolTranslator::new(None, None);
        let mut https_tool = ToolRecord::new("ext_https", "HTTPS tool", json!({"type": "object"}));
        https_tool.protocol = ProtocolType::Http;
        https_tool.downstream_endpoint = "https://127.0.0.1:1/unreachable".to_string();
        let err = translator
            .execute_tool_call("srv", &https_tool, &json!({}), None, None)
            .unwrap_err();
        assert!(
            err.contains("Downstream HTTP") || err.contains("UPSTREAM_TLS_UNSUPPORTED"),
            "https:// must route through HttpDownstreamClient, not MuttleyMeshClient; got: {}",
            err
        );
    }

    stop.store(true, Ordering::Relaxed);
    let _ = rest_srv.join();
}

// ============================================================================
// Pillar 4: Bidirectional Upstream MCP (Sampling, Elicitation, Progress, Cancel)
// ============================================================================

#[test]
fn test_v6_5_bidirectional_upstream_sampling_elicitation_and_cancel() {
    use omni_mcp::upstream::stdio::StdioUpstreamSession;

    // Locate compiled omni-gateway binary to spawn `mock-bidirectional-mcp`
    let bin_path = std::path::PathBuf::from(env!("CARGO_BIN_EXE_omni-gateway"));
    assert!(
        bin_path.exists(),
        "omni-gateway binary must exist at {:?}",
        bin_path
    );

    let progress_events = Arc::new(Mutex::new(Vec::<JsonValue>::new()));
    let prog_sink_ref = progress_events.clone();

    // 1. Policy::Deny -> sampling/createMessage rejected with -32001, progress forwarded
    let hub_deny = Arc::new(UpstreamCallbackHub::new());
    hub_deny.set_progress_sink(Arc::new(move |_srv, notif| {
        prog_sink_ref.lock().unwrap().push(notif.clone());
    }));

    let sess_deny = StdioUpstreamSession::new(
        "bidir-deny",
        bin_path.clone(),
        vec!["mock-bidirectional-mcp".to_string()],
        BTreeMap::new(),
        None,
        std::env::temp_dir(),
        5000,
    )
    .with_bidirectional_hub(hub_deny, SamplingElicitationPolicy::Deny);

    let res_deny = sess_deny
        .call_tool("trigger_sampling", &json!({"prompt": "Hello"}))
        .expect("call_tool under Deny must complete cleanly");
    let err_code = res_deny["structuredContent"]["sampling_reply"]["error"]["code"].as_i64();
    assert_eq!(
        err_code,
        Some(UNAUTHORIZED as i64),
        "Deny policy must reject reverse sampling/createMessage with -32001; got {:?}",
        res_deny
    );
    assert!(
        progress_events.lock().unwrap().len() >= 2,
        "Streaming notifications/progress (50%, 100%) must be forwarded to progress sink"
    );
    sess_deny.shutdown();

    // 2. Policy::ApprovalGate -> PII redacted, queued in callback hub, resumed on operator approval
    let hub_gate = Arc::new(UpstreamCallbackHub::new());
    let sess_gate = Arc::new(
        StdioUpstreamSession::new(
            "bidir-gate",
            bin_path.clone(),
            vec!["mock-bidirectional-mcp".to_string()],
            BTreeMap::new(),
            None,
            std::env::temp_dir(),
            5000,
        )
        .with_bidirectional_hub(hub_gate.clone(), SamplingElicitationPolicy::ApprovalGate),
    );

    let sess_call = sess_gate.clone();
    let call_thread = thread::spawn(move || {
        sess_call.call_tool(
            "trigger_sampling",
            &json!({"prompt": "Contact alice@example.com for approval"}),
        )
    });

    // Wait for pending callback to arrive in ApprovalGate inbox
    let mut pending_cb = None;
    for _ in 0..60 {
        let list = hub_gate.list_pending_callbacks();
        if let Some(first) = list.into_iter().next() {
            pending_cb = Some(first);
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let cb = pending_cb.expect("Reverse sampling/createMessage must queue in ApprovalGate hub");
    assert_eq!(cb.method, "sampling/createMessage");
    assert!(
        cb.params.to_string_compact().contains("[REDACTED_EMAIL]"),
        "ApprovalGate must redact PII before queuing: {}",
        cb.params.to_string_compact()
    );

    // Operator approves and supplies completion result
    hub_gate
        .resolve_callback(
            &cb.callback_id,
            true,
            Some(json!({"model": "omni-approved", "content": {"type": "text", "text": "APPROVED_TEXT"}})),
            None,
        )
        .expect("resolve_callback must succeed");

    let gate_res = call_thread
        .join()
        .unwrap()
        .expect("tool call must resume after approval");
    assert_eq!(
        gate_res["structuredContent"]["sampling_reply"]["result"]["content"]["text"].as_str(),
        Some("APPROVED_TEXT")
    );

    // 3. Cancellation bridging (notifications/cancelled unblocks waiting caller immediately)
    let sess_cancel = sess_gate.clone();
    let cancel_thread = thread::spawn(move || sess_cancel.call_tool("slow_cancel", &json!({})));
    thread::sleep(Duration::from_millis(100));
    let cancelled = sess_gate.cancel_request(0, Some("User aborted"));
    assert!(cancelled, "cancel_request must abort in-flight request");
    let cancel_res = cancel_thread.join().unwrap();
    assert!(
        cancel_res.is_err() && cancel_res.unwrap_err().contains("cancelled"),
        "Cancelled call must return cancellation error immediately"
    );
    sess_gate.shutdown();
}

// ============================================================================
// Pillar 5: Hybrid BM25 + Vector RRF Synonym Discovery & Canonical Semantic Cache
// ============================================================================

#[test]
fn test_v6_5_hybrid_rrf_synonym_discovery_and_canonical_cache() {
    let registry = MCPRegistry::new(None);

    let mut pay_srv = ServerRecord::new(
        "payments",
        "Payments & Refunds",
        "Handles customer refunds and transaction credits",
        "fintech",
    );
    pay_srv.status = ToolStatus::Enabled;
    let mut refund_tool = ToolRecord::new(
        "issue_refund",
        "Issue monetary refund to customer card",
        json!({"type": "object", "properties": {"order_id": {"type": "string"}}}),
    );
    refund_tool.status = ToolStatus::Enabled;
    refund_tool.tags = vec!["refund".to_string(), "payments".to_string()];
    refund_tool.cache_ttl_sec = Some(30);
    pay_srv.add_tool(refund_tool);
    registry.register_server(pay_srv);

    let mut wx_srv = ServerRecord::new(
        "weather",
        "Weather Forecast",
        "Meteorology temperature and rainfall forecasts",
        "data",
    );
    wx_srv.status = ToolStatus::Enabled;
    let mut wx_tool = ToolRecord::new(
        "get_forecast",
        "Retrieve 7-day weather forecast by city",
        json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    );
    wx_tool.status = ToolStatus::Enabled;
    wx_srv.add_tool(wx_tool);
    registry.register_server(wx_srv);

    // Confirm pure BM25 scores 0.0 on pure synonyms "reimburse chargeback"
    let pure_bm25 = CodeModeTypeGenerator::bm25_score_corpus(
        "reimburse chargeback",
        &[
            vec![
                (3.5, "issue_refund".to_string()),
                (1.8, "Issue monetary refund to customer card".to_string()),
            ],
            vec![
                (3.5, "get_forecast".to_string()),
                (1.8, "Retrieve 7-day weather forecast by city".to_string()),
            ],
        ],
    );
    assert_eq!(
        pure_bm25[0], 0.0,
        "Pure BM25 has 0 lexical overlap on synonyms"
    );

    let cache = InMemoryConfigCache::new(registry, 60.0);
    let gateway = MCPProxyGateway::new(cache.clone(), None);
    let omni = OmniMCPServer::new(cache.clone(), gateway);

    // Hybrid RRF in recommend_tools must rank payments::issue_refund #1
    let rec_req = RPCRequest::new(
        1,
        "tools/call",
        json!({
            "name": "recommend_tools",
            "arguments": {"goal": "reimburse chargeback", "limit": 3}
        }),
    );
    let rec_resp = omni.handle_request(&rec_req, "agent", None);
    let rec_text = rec_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let rec_json = JsonValue::parse(&rec_text).unwrap();
    let top_tool = rec_json["recommended_pipeline"][0]["tool_name"].as_str();
    assert_eq!(
        top_tool,
        Some("issue_refund"),
        "Hybrid RRF must rank payments::issue_refund #1 on 'reimburse chargeback'; got: {}",
        rec_text
    );

    // Hybrid RRF in discover_server must rank payments #1
    let disc_req = RPCRequest::new(
        2,
        "tools/call",
        json!({
            "name": "discover_server",
            "arguments": {"query": "reimburse chargeback", "limit": 3}
        }),
    );
    let disc_resp = omni.handle_request(&disc_req, "agent", None);
    let disc_text = disc_resp.result.unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let disc_json = JsonValue::parse(&disc_text).unwrap();
    assert_eq!(
        disc_json["servers"][0]["server_name"].as_str(),
        Some("payments")
    );

    // Canonical SemanticResponseCache: key-reordered JSON hits the exact same cache entry
    let sem_cache = SemanticResponseCache::new(60.0);
    let k1 = SemanticResponseCache::make_key(
        "payments",
        "issue_refund",
        &json!({"b": 1, "a": 2, "nested": {"y": true, "x": " ok "}}),
    );
    let k2 = SemanticResponseCache::make_key(
        "payments",
        "issue_refund",
        &json!({"nested": {"x": "ok", "y": true}, "a": 2, "b": 1}),
    );
    assert_eq!(
        k1, k2,
        "Canonical JSON cache keys must be invariant to key order"
    );
    sem_cache.put_with_ttl(k1, json!({"refunded": true}), Some(30));
    let hit = sem_cache.get(&k2).expect("Reordered JSON must hit cache");
    assert_eq!(hit["refunded"].as_bool(), Some(true));
    assert_eq!(sem_cache.stats(), (1, 0));
}

// ============================================================================
// Pillar 6: ChaCha20-Poly1305 AEAD Vault, 401 Auto-Refresh, Argument ABAC & Tenant Quotas
// ============================================================================

#[test]
fn test_v6_5_chacha20_poly1305_oauth_refresh_and_arg_abac_quotas() {
    let dir = temp_test_dir("omni_v65_oauth_abac");
    let store = UpstreamOAuthTokenStore::new(&dir, Some("master-key-v65"));

    // 1. Seal & load full OAuthTokenRecord (ChaCha20-Poly1305 when tls-rustls is enabled)
    let rec = OAuthTokenRecord {
        access_token: "access_v1".to_string(),
        refresh_token: Some("refresh_xyz".to_string()),
        expires_at: Some(9_999_999_999.0),
        token_endpoint: Some("http://127.0.0.1:59988/oauth/token".to_string()),
        client_id: Some("omni-client".to_string()),
        scope: Some("mcp:tools".to_string()),
    };
    store.save_token_record("saas-linear", &rec).unwrap();
    let loaded = store.load_token_record("saas-linear").unwrap();
    assert_eq!(loaded, rec);

    let raw_file =
        fs::read_to_string(std::path::Path::new(&dir).join("saas-linear.oauth.enc")).unwrap();
    #[cfg(feature = "tls-rustls")]
    assert!(
        raw_file.contains("ChaCha20-Poly1305"),
        "v6.5 must seal OAuth tokens with ChaCha20-Poly1305 AEAD"
    );

    // Wrong AAD (renaming file to another server) & tampered ciphertext must be rejected
    fs::write(
        std::path::Path::new(&dir).join("other-server.oauth.enc"),
        &raw_file,
    )
    .unwrap();
    #[cfg(feature = "tls-rustls")]
    assert!(
        store.load_token_record("other-server").is_err(),
        "AEAD AAD mismatch must reject token transplanted to another server name"
    );

    let wrong_key_store = UpstreamOAuthTokenStore::new(&dir, Some("wrong-key"));
    assert!(wrong_key_store.load_token_record("saas-linear").is_err());

    // 2. Transparent backward-compat read of legacy v6.0.0 SHA-256 CTR + HMAC envelope
    let legacy_key = omni_mcp::storage::sha256(b"master-key-v65");
    let nonce = [7u8; 16];
    let legacy_plain = b"legacy_v6_access_token";
    let mut block_in = Vec::new();
    block_in.extend_from_slice(&legacy_key);
    block_in.extend_from_slice(&nonce);
    block_in.extend_from_slice(&0u32.to_le_bytes());
    let ks = omni_mcp::storage::sha256(&block_in);
    let ct: Vec<u8> = legacy_plain
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ ks[i])
        .collect();
    let mut blob = Vec::new();
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    let mac = omni_mcp::storage::audit_log::hmac_sha256_hex(&legacy_key, &blob);
    let hex = |bs: &[u8]| bs.iter().map(|b| format!("{:02x}", b)).collect::<String>();
    let legacy_json = json!({
        "server": "legacy-srv",
        "nonce_hex": hex(&nonce),
        "ciphertext_hex": hex(&ct),
        "hmac_sha256": mac,
    });
    fs::write(
        std::path::Path::new(&dir).join("legacy-srv.oauth.enc"),
        legacy_json.to_string_compact(),
    )
    .unwrap();
    assert_eq!(
        store
            .load_token("legacy-srv")
            .expect("v6.0 legacy token must decrypt transparently"),
        "legacy_v6_access_token"
    );

    // 3. Automatic 401 -> grant_type=refresh_token -> retry in HttpUpstreamSession
    let token_ep = "http://127.0.0.1:59988/oauth/token";
    let mcp_ep = "http://127.0.0.1:59989/mcp";
    let default_store = UpstreamOAuthTokenStore::new(&dir, None);
    default_store
        .save_token_record(
            "auto-refresh-mcp",
            &OAuthTokenRecord {
                access_token: "stale_token_401".to_string(),
                refresh_token: Some("ref_tok_1".to_string()),
                expires_at: Some(9_999_999_999.0),
                token_endpoint: Some(token_ep.to_string()),
                client_id: Some("cid".to_string()),
                scope: None,
            },
        )
        .unwrap();

    let refresh_hits = Arc::new(AtomicUsize::new(0));
    let rh = refresh_hits.clone();
    register_mock_http(token_ep, move |_p, body, _h| {
        let bstr = String::from_utf8_lossy(body);
        assert!(bstr.contains("grant_type=refresh_token"));
        rh.fetch_add(1, Ordering::SeqCst);
        let mut hdrs = BTreeMap::new();
        hdrs.insert("content-type".to_string(), "application/json".to_string());
        (
            200,
            hdrs,
            br#"{"access_token":"fresh_token_200","refresh_token":"ref_tok_2","expires_in":3600}"#
                .to_vec(),
        )
    });

    register_mock_http(mcp_ep, |_p, body, hdrs| {
        let auth = hdrs.get("authorization").cloned().unwrap_or_default();
        let req_json = JsonValue::parse(&String::from_utf8_lossy(body)).unwrap();
        let method = req_json["method"].as_str().unwrap_or("");
        let id = req_json["id"].clone();
        let mut resp_hdrs = BTreeMap::new();
        resp_hdrs.insert("content-type".to_string(), "application/json".to_string());
        if method == "initialize" {
            return (
                200,
                resp_hdrs,
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-11-25","capabilities":{}}})
                    .to_string_compact()
                    .into_bytes(),
            );
        }
        if auth != "Bearer fresh_token_200" {
            return (401, resp_hdrs, b"Unauthorized: token expired".to_vec());
        }
        (
            200,
            resp_hdrs,
            json!({"jsonrpc":"2.0","id":id,"result":{"refreshed":true}})
                .to_string_compact()
                .into_bytes(),
        )
    });

    let sess = HttpUpstreamSession::new(
        "auto-refresh-mcp",
        mcp_ep,
        None,
        BTreeMap::new(),
        SessionMode::Stateless,
        std::path::PathBuf::from(&dir),
        5000,
        true,
    );
    let call_res = sess.call_rpc("tools/call", json!({"name": "ping"}));
    clear_mock_http(token_ep);
    clear_mock_http(mcp_ep);
    assert_eq!(
        call_res.expect("401 auto-refresh retry must succeed")["refreshed"].as_bool(),
        Some(true)
    );
    assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        default_store.load_token("auto-refresh-mcp").unwrap(),
        "fresh_token_200"
    );

    // 4. Argument-level ABAC guardrails & Per-Tenant Hourly Quotas
    let registry = MCPRegistry::new(None);
    let mut fin_srv = ServerRecord::new("treasury", "Treasury", "Wire payouts", "finance");
    fin_srv.status = ToolStatus::Enabled;
    fin_srv.charter_policy.argument_rules = vec![
        ArgumentPolicyRule::new("/amount", "lte", json!(5000)),
        ArgumentPolicyRule::new("/environment", "neq", json!("prod")),
    ];
    fin_srv.charter_policy.tenant_quota = Some(TenantQuotaPolicy {
        max_tool_calls_per_hour: 2,
        max_codemode_fuel_per_hour: 500,
        max_egress_bytes_per_hour: 1024,
    });
    let mut payout = ToolRecord::new(
        "send_payout",
        "Send wire payout",
        json!({"type": "object", "properties": {"amount": {"type": "number"}, "environment": {"type": "string"}}}),
    );
    payout.status = ToolStatus::Enabled;
    fin_srv.add_tool(payout);
    registry.register_server(fin_srv);

    let cache = InMemoryConfigCache::new(registry, 60.0);
    let translator = ProtocolTranslator::new(None, None);
    translator.mesh_client.set_allow_synthetic_mocks(true);
    let gw = MCPProxyGateway::new(cache, Some(translator));

    // Amount > 5000 must fail with -32001 UNAUTHORIZED
    let over_limit = gw.handle_request(
        "treasury",
        &RPCRequest::new(
            1,
            "tools/call",
            json!({"name": "send_payout", "arguments": {"amount": 7500, "environment": "staging"}}),
        ),
        "agent",
        Some("tenant_acme"),
    );
    assert_eq!(
        over_limit.error.as_ref().map(|e| e.code),
        Some(UNAUTHORIZED)
    );

    // environment == "prod" must fail with -32001 UNAUTHORIZED
    let prod_blocked = gw.handle_request(
        "treasury",
        &RPCRequest::new(
            2,
            "tools/call",
            json!({"name": "send_payout", "arguments": {"amount": 100, "environment": "prod"}}),
        ),
        "agent",
        Some("tenant_acme"),
    );
    assert_eq!(
        prod_blocked.error.as_ref().map(|e| e.code),
        Some(UNAUTHORIZED)
    );

    // Two compliant calls succeed; 3rd call breaches max_tool_calls_per_hour=2 -> -32004 RATE_LIMITED with Retry-After
    for i in 3..=4 {
        let ok = gw.handle_request(
            "treasury",
            &RPCRequest::new(i, "tools/call", json!({"name": "send_payout", "arguments": {"amount": 100, "environment": "staging"}})),
            "agent",
            Some("tenant_acme"),
        );
        assert!(
            ok.error.is_none(),
            "Compliant call {} must succeed: {:?}",
            i,
            ok.error
        );
    }
    let quota_hit = gw.handle_request(
        "treasury",
        &RPCRequest::new(
            5,
            "tools/call",
            json!({"name": "send_payout", "arguments": {"amount": 100, "environment": "staging"}}),
        ),
        "agent",
        Some("tenant_acme"),
    );
    assert_eq!(quota_hit.error.as_ref().map(|e| e.code), Some(RATE_LIMITED));
    assert!(
        quota_hit
            .error
            .as_ref()
            .and_then(|e| e.data.as_ref())
            .and_then(|d| d.get("retry_after_sec"))
            .is_some(),
        "Quota breach must include retry_after_sec metadata"
    );

    // Direct fuel quota check on TenantQuotaTracker
    let qt = TenantQuotaTracker::new(TenantQuotaPolicy {
        max_tool_calls_per_hour: 100,
        max_codemode_fuel_per_hour: 1000,
        max_egress_bytes_per_hour: 10_000,
    });
    assert!(qt.check_and_consume("t1", 0, 600, 0, None).is_ok());
    assert!(qt.check_and_consume("t1", 0, 500, 0, None).is_err());

    let _ = fs::remove_dir_all(&dir);
}

// ============================================================================
// Pillar 7: OTLP Background Batch Worker (Periodic + Threshold Flush + Histograms)
// ============================================================================

#[test]
fn test_v6_5_otlp_background_batch_worker() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock OTLP collector");
    let addr = listener.local_addr().unwrap();
    let received_bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let recv_c = received_bodies.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = stop.clone();
    let _ = listener.set_nonblocking(true);

    let collector = thread::spawn(move || {
        while !stop_c.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let mut cl = 0usize;
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            if k.eq_ignore_ascii_case("content-length") {
                                cl = v.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    let mut body = vec![0u8; cl];
                    if cl > 0 {
                        let _ = reader.read_exact(&mut body);
                    }
                    recv_c
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&body).into_owned());
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    );
                    let _ = stream.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let endpoint = format!("http://127.0.0.1:{}/v1/traces", addr.port());
    let worker = OtlpBatchWorker::new(endpoint, 120, 4);
    worker.start();

    for i in 0..5u64 {
        worker.record_span(TraceSpan {
            trace_id: format!("0000000000000000000000000000000{}", i + 1),
            span_id: format!("000000000000000{}", i + 1),
            parent_span_id: None,
            name: format!("payments.op_{}", i),
            service_name: "payments".to_string(),
            tool_name: format!("op_{}", i),
            start_time_us: 1_700_000_000_000_000 + i * 1000,
            duration_us: (i + 1) * 250,
            status: if i == 4 {
                "ERROR".to_string()
            } else {
                "OK".to_string()
            },
            attributes: BTreeMap::new(),
        });
    }

    for _ in 0..40 {
        if worker.status().total_exported >= 5 {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    let status = worker.status();
    assert!(
        status.total_exported >= 5,
        "OTLP batch worker must flush >= 5 spans automatically; status: {:?}",
        status
    );
    assert!(status.p50_us > 0 && status.p95_us >= status.p50_us && status.p99_us >= status.p95_us);
    assert_eq!(status.error_counts.get("ERROR"), Some(&1));

    let combined = received_bodies.lock().unwrap().join("\n");
    assert!(combined.contains("resourceSpans"));
    assert!(combined.contains("omni_latency_us_quantiles"));
    assert!(combined.contains("omni_errors_total.ERROR"));

    worker.shutdown();
    stop.store(true, Ordering::Relaxed);
    let _ = collector.join();
}

#[test]
fn test_v6_5_rest_oauth_pkce_import_wizard_and_migration() {
    let dir = temp_test_dir("omni_v65_rest_wizard");
    let ctx = ProductionGatewayContext::new(Some(OmniConfig {
        data_dir: dir.clone(),
        secrets_dir: std::path::PathBuf::from(&dir).join("secrets"),
        demo_seed: true,
        ..Default::default()
    }));
    let mut admin_hdrs = HashMap::new();
    admin_hdrs.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );

    // 1. OAuth PKCE browser authorize -> callback -> sealed token
    let (st, _, _, body) = ctx.handle_http(
        "GET",
        "/api/v1/oauth/authorize/external-mcp:linear",
        &admin_hdrs,
        &[],
    );
    assert_eq!(st, 200);
    let auth_json = JsonValue::parse(&String::from_utf8(body).unwrap()).unwrap();
    let state = auth_json["state"].as_str().unwrap().to_string();
    assert!(auth_json["authorization_url"]
        .as_str()
        .unwrap()
        .contains("code_challenge_method=S256"));

    let cb_path = format!(
        "/api/v1/oauth/callback?state={}&access_token=pkce_live_tok_999&refresh_token=pkce_ref_999",
        state
    );
    let (cb_st, _, _, cb_body) = ctx.handle_http("GET", &cb_path, &HashMap::new(), &[]);
    assert_eq!(
        cb_st,
        200,
        "OAuth callback failed: {}",
        String::from_utf8_lossy(&cb_body)
    );
    let token_store = UpstreamOAuthTokenStore::new(&ctx.config.secrets_dir, None);
    assert_eq!(
        token_store.load_token("external-mcp:linear").unwrap(),
        "pkce_live_tok_999"
    );

    // 2. Browser mcp.json import wizard (dry_run preview + live import)
    let cfg_payload = json!({
        "dry_run": true,
        "config_json": r#"{"mcpServers":{"gh-demo":{"command":"npx","args":["-y","server-github"],"env":{"GITHUB_TOKEN":"ghp_secret_value"}}}}"#
    });
    let (imp_st, _, _, imp_body) = ctx.handle_http(
        "POST",
        "/api/v1/import-config",
        &admin_hdrs,
        cfg_payload.to_string_compact().as_bytes(),
    );
    assert_eq!(imp_st, 200);
    let imp_json = JsonValue::parse(&String::from_utf8(imp_body).unwrap()).unwrap();
    assert_eq!(imp_json["dry_run"].as_bool(), Some(true));
    assert_eq!(
        imp_json["servers"][0]["converted_secrets"][0]["converted_to"].as_str(),
        Some("{env:GITHUB_TOKEN}")
    );

    // Live import registers server
    let live_payload = json!({
        "dry_run": false,
        "config_json": r#"{"mcpServers":{"gh-demo":{"command":"npx","args":["-y","server-github"],"env":{"GITHUB_TOKEN":"ghp_secret_value"}}}}"#
    });
    let (live_st, _, _, live_body) = ctx.handle_http(
        "POST",
        "/api/v1/import-config",
        &admin_hdrs,
        live_payload.to_string_compact().as_bytes(),
    );
    assert_eq!(live_st, 200);
    let live_json = JsonValue::parse(&String::from_utf8(live_body).unwrap()).unwrap();
    assert_eq!(live_json["imported_count"].as_i64(), Some(1));
    assert!(ctx.registry.get_server("external-mcp:gh-demo").is_some());

    // 3. OTLP status endpoint returns quantiles and storage_backend
    let (otlp_st, _, _, otlp_body) =
        ctx.handle_http("GET", "/api/v1/telemetry/otlp/status", &admin_hdrs, &[]);
    assert_eq!(otlp_st, 200);
    let otlp_json = JsonValue::parse(&String::from_utf8(otlp_body).unwrap()).unwrap();
    assert!(otlp_json.get("latency_quantiles_us").is_some());

    // 4. v6.5 SQLite migration with SHA-256 checksum verification
    #[cfg(feature = "storage-sqlite")]
    {
        let sqlite_target = std::path::Path::new(&dir)
            .join("migrated_v65.sqlite")
            .to_string_lossy()
            .into_owned();
        let rep = omni_mcp::extension::aifx::migrate_to_v6_5(&dir, "sqlite", &sqlite_target, false);
        assert!(rep.ok, "v6.5 SQLite migration failed: {:?}", rep);
        assert!(rep.servers_migrated >= 1);
    }

    let _ = fs::remove_dir_all(&dir);
}

// ============================================================================
// v6.5.0 Hardening & Adversarial Regression Tests (Round 2 Review Fixes)
// ============================================================================

#[test]
fn test_v6_5_hardening_hpack_huffman_dynamic_table_and_h2_chunking() {
    use omni_mcp::data_plane::downstream_grpc::{
        hpack_huffman_decode, hpack_huffman_encode, read_h2_frame, write_h2_frame, HpackDecoder,
    };

    // 1. Endpoint parser must reject empty-host URLs cleanly without panicking
    assert!(GrpcDownstreamClient::parse_endpoint("http://", "svc/Method").is_err());
    assert!(GrpcDownstreamClient::parse_endpoint("grpc://", "svc/Method").is_err());
    assert!(GrpcDownstreamClient::decode_grpc_frame(&[1, 0, 0, 0, 2, b'{', b'}']).is_err());

    // 2. IPv6 + UTF-8 URL template hardening in HttpDownstreamClient
    let (h6, p6, path6) =
        HttpDownstreamClient::parse_url("http://[::1]:8085/v1/items").expect("IPv6 URL");
    assert_eq!(h6, "::1");
    assert_eq!(p6, 8085);
    assert_eq!(path6, "/v1/items");
    let (utf8_url, _) = HttpDownstreamClient::interpolate_url_template(
        "http://127.0.0.1:8080/café/{id}",
        &json!({"id": "42"}),
        true,
    )
    .expect("UTF-8 template");
    assert_eq!(utf8_url, "http://127.0.0.1:8080/café/42");

    // 3. RFC 7541 Huffman encode/decode + Dynamic Table stateful HPACK decoding
    let sample = b"permission denied: invalid-token-123";
    let encoded = hpack_huffman_encode(sample);
    let decoded = hpack_huffman_decode(&encoded).expect("Huffman decode");
    assert_eq!(decoded, sample);

    let mut dec = HpackDecoder::new(4096);
    let mut blk1 = Vec::new();
    blk1.push(0x40);
    let h_name = hpack_huffman_encode(b"grpc-status");
    blk1.push(0x80 | (h_name.len() as u8));
    blk1.extend_from_slice(&h_name);
    let h_val = hpack_huffman_encode(b"7");
    blk1.push(0x80 | (h_val.len() as u8));
    blk1.extend_from_slice(&h_val);
    let mut hdrs = HashMap::new();
    dec.decode_block(&blk1, &mut hdrs);
    assert_eq!(hdrs.get("grpc-status").map(|s| s.as_str()), Some("7"));

    let mut hdrs2 = HashMap::new();
    dec.decode_block(&[0x80 | 62], &mut hdrs2);
    assert_eq!(hdrs2.get("grpc-status").map(|s| s.as_str()), Some("7"));

    // 4. Live HTTP/2 gRPC server testing >16KiB request chunking, >64KiB response + WINDOW_UPDATE,
    //    and Huffman-encoded trailers split across HEADERS + CONTINUATION
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind H2 stress server");
    let addr = listener.local_addr().unwrap();
    let max_req_frame_seen = Arc::new(AtomicUsize::new(0));
    let window_updates_seen = Arc::new(AtomicUsize::new(0));
    let max_frame_c = max_req_frame_seen.clone();
    let wu_c = window_updates_seen.clone();

    let srv = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut preface = [0u8; 24];
        stream.read_exact(&mut preface).unwrap();
        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");

        write_h2_frame(&mut stream, 0x04, 0, 0, &[]).unwrap();
        stream.flush().unwrap();

        let mut req_data = Vec::new();
        loop {
            let (ftype, flags, sid, body) = read_h2_frame(&mut stream).unwrap();
            if ftype == 0x04 && (flags & 0x01) == 0 {
                write_h2_frame(&mut stream, 0x04, 0x01, 0, &[]).unwrap();
                stream.flush().unwrap();
            } else if ftype == 0x00 && sid == 1 {
                max_frame_c.fetch_max(body.len(), Ordering::SeqCst);
                req_data.extend_from_slice(&body);
                if (flags & 0x01) != 0 {
                    break;
                }
            }
        }
        assert!(req_data.len() > 20_000, "expected >20KB request payload");

        // Send response HEADERS (:status 200 static index 8 -> 0x88)
        write_h2_frame(&mut stream, 0x01, 0x04, 1, &[0x88]).unwrap();

        // Send > 70 KiB response DATA in 16 KiB chunks
        let big_payload = format!("{{\"blob\":\"{}\"}}", "x".repeat(70_000));
        let framed_resp = GrpcDownstreamClient::encode_grpc_frame(big_payload.as_bytes());
        for chunk in framed_resp.chunks(16_384) {
            write_h2_frame(&mut stream, 0x00, 0x00, 1, chunk).unwrap();
        }
        // Send trailers split across HEADERS (no END_HEADERS) + CONTINUATION (END_HEADERS) with Huffman grpc-status: 0
        let mut trailer_blk = Vec::new();
        trailer_blk.push(0x00);
        let hn = hpack_huffman_encode(b"grpc-status");
        trailer_blk.push(0x80 | (hn.len() as u8));
        trailer_blk.extend_from_slice(&hn);
        let hv = hpack_huffman_encode(b"0");
        trailer_blk.push(0x80 | (hv.len() as u8));
        trailer_blk.extend_from_slice(&hv);
        let mid = trailer_blk.len() / 2;
        write_h2_frame(&mut stream, 0x01, 0x01, 1, &trailer_blk[..mid]).unwrap();
        write_h2_frame(&mut stream, 0x09, 0x04, 1, &trailer_blk[mid..]).unwrap();
        stream.flush().unwrap();

        let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
        for _ in 0..20 {
            match read_h2_frame(&mut stream) {
                Ok((0x08, _, _, _)) => {
                    wu_c.fetch_add(1, Ordering::SeqCst);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let client = GrpcDownstreamClient::new();
    let large_arg = json!({"payload": "a".repeat(22_000)});
    let resp = client
        .invoke(
            &format!("grpc://127.0.0.1:{}/pkg.Svc/LargeEcho", addr.port()),
            "pkg.Svc/LargeEcho",
            &large_arg,
            None,
        )
        .expect("Large H2 gRPC round-trip must succeed");
    let _ = srv.join();
    assert!(
        max_req_frame_seen.load(Ordering::SeqCst) <= 16_384,
        "Outbound H2 DATA frames must respect 16384-byte DEFAULT_MAX_FRAME_SIZE"
    );
    assert!(
        window_updates_seen.load(Ordering::SeqCst) >= 2,
        "Client must emit WINDOW_UPDATE frames for >64 KiB H2 responses"
    );
    assert!(resp.len() > 70_000);

    // 5. Connect-RPC via ProtocolTranslator must NOT send duplicate Content-Type headers
    #[cfg(any(feature = "transport-http", feature = "transport-grpc"))]
    {
        let ct_listener = TcpListener::bind("127.0.0.1:0").expect("bind Connect CT server");
        let ct_addr = ct_listener.local_addr().unwrap();
        let ct_headers = Arc::new(Mutex::new(Vec::<String>::new()));
        let ct_h_clone = ct_headers.clone();
        let ct_srv = thread::spawn(move || {
            let (mut stream, _) = ct_listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut cl = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    if k.eq_ignore_ascii_case("content-type") {
                        ct_h_clone.lock().unwrap().push(v.trim().to_string());
                    } else if k.eq_ignore_ascii_case("content-length") {
                        cl = v.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; cl];
            let _ = reader.read_exact(&mut body);
            let resp_body = r#"{"ok":true}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        });
        let translator = ProtocolTranslator::new(None, None);
        let mut grpc_tool = ToolRecord::new("Echo", "Connect echo", json!({"type": "object"}));
        grpc_tool.protocol = omni_mcp::models::ProtocolType::Grpc;
        grpc_tool.downstream_endpoint = format!("http://127.0.0.1:{}/pkg.Svc/Echo", ct_addr.port());
        let res = translator
            .execute_tool_call("svc", &grpc_tool, &json!({"msg": "hi"}), None, None)
            .expect("Connect-RPC via ProtocolTranslator must succeed");
        let _ = ct_srv.join();
        assert_eq!(res["ok"].as_bool(), Some(true));
        let seen_cts = ct_headers.lock().unwrap().clone();
        assert_eq!(
            seen_cts,
            vec!["application/json".to_string()],
            "Connect-RPC must send strictly ONE Content-Type header, got: {:?}",
            seen_cts
        );
    }
}

#[cfg(feature = "storage-postgres")]
#[test]
fn test_v6_5_hardening_postgres_scram_downgrade_and_dsn_decode() {
    use omni_mcp::storage::{PgDsn, PgWireConnection};

    // 1. Percent-decoded DSN credentials
    let parsed = PgDsn::parse("postgres://u%3A1:p%40ss%2F2@127.0.0.1:5432/my%20db?sslmode=disable")
        .expect("percent-encoded DSN");
    assert_eq!(parsed.user, "u:1");
    assert_eq!(parsed.password, "p@ss/2");
    assert_eq!(parsed.database, "my db");

    // 2. Reject SCRAM-SHA-256 downgrade where server skips SASLFinal and sends AuthenticationOk
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut lbuf = [0u8; 4];
        s.read_exact(&mut lbuf).unwrap();
        let len = u32::from_be_bytes(lbuf) as usize;
        let mut startup = vec![0u8; len.saturating_sub(4)];
        s.read_exact(&mut startup).unwrap();

        let mut sasl = Vec::new();
        sasl.extend_from_slice(&10u32.to_be_bytes());
        sasl.extend_from_slice(b"SCRAM-SHA-256\0\0");
        s.write_all(b"R").unwrap();
        s.write_all(&((4 + sasl.len()) as u32).to_be_bytes())
            .unwrap();
        s.write_all(&sasl).unwrap();
        s.flush().unwrap();

        let mut hdr = [0u8; 5];
        s.read_exact(&mut hdr).unwrap();
        let plen = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let mut pbody = vec![0u8; plen.saturating_sub(4)];
        s.read_exact(&mut pbody).unwrap();

        // Maliciously skip SASLContinue/SASLFinal and send AuthenticationOk + ReadyForQuery
        s.write_all(b"R").unwrap();
        s.write_all(&8u32.to_be_bytes()).unwrap();
        s.write_all(&0u32.to_be_bytes()).unwrap();
        s.write_all(b"Z").unwrap();
        s.write_all(&5u32.to_be_bytes()).unwrap();
        s.write_all(b"I").unwrap();
        s.flush().unwrap();
    });

    let bad_dsn = PgDsn::parse(&format!(
        "postgres://omni:pw@127.0.0.1:{}/db?sslmode=disable",
        addr.port()
    ))
    .unwrap();
    let err = PgWireConnection::connect(&bad_dsn)
        .err()
        .expect("SCRAM downgrade skipping SASLFinal MUST be rejected");
    assert!(
        err.contains("SCRAM-SHA-256"),
        "Unexpected error for SCRAM downgrade: {}",
        err
    );
    let _ = srv.join();
}

#[test]
fn test_v6_5_hardening_ssrf_smuggling_wasm_recursion_quotas_and_otlp_retry() {
    let dir = temp_test_dir("omni_v65_hardening");
    let ctx = Arc::new(ProductionGatewayContext::new(Some(OmniConfig {
        data_dir: dir.clone(),
        allow_loopback_upstream: false,
        demo_seed: true,
        ..Default::default()
    })));
    let mut admin_hdrs = HashMap::new();
    admin_hdrs.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );

    // 1. /api/stats must report real package version (6.5.0), not stale 2.0.0
    let (st, _, _, body) = ctx.handle_http("GET", "/api/stats", &admin_hdrs, &[]);
    assert_eq!(st, 200);
    let stats_json = JsonValue::parse(&String::from_utf8(body).unwrap()).unwrap();
    assert_eq!(
        stats_json["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    // 2. /api/v1/import-config must reject SSRF metadata IPs & cmd.exe injection
    let ssrf_import = json!({
        "dry_run": false,
        "config_json": r#"{"mcpServers":{"evil":{"url":"http://169.254.169.254/latest/meta-data"}}}"#
    });
    let (st_ssrf, _, _, _) = ctx.handle_http(
        "POST",
        "/api/v1/import-config",
        &admin_hdrs,
        ssrf_import.to_string_compact().as_bytes(),
    );
    assert_eq!(
        st_ssrf, 403,
        "SSRF URL in import-config must be rejected with 403"
    );

    let cmd_import = json!({
        "dry_run": false,
        "config_json": r#"{"mcpServers":{"evil_cmd":{"command":"cmd.exe","args":["/C","whoami"]}}}"#
    });
    let (st_cmd, _, _, _) = ctx.handle_http(
        "POST",
        "/api/v1/import-config",
        &admin_hdrs,
        cmd_import.to_string_compact().as_bytes(),
    );
    assert_eq!(
        st_cmd, 403,
        "cmd.exe in import-config must be rejected with 403"
    );

    // 3. /api/v1/oauth/authorize must reject SSRF token_endpoint
    let (st_oauth_ssrf, _, _, _) = ctx.handle_http(
        "GET",
        "/api/v1/oauth/authorize/srv?token_endpoint=http://169.254.169.254/token",
        &admin_hdrs,
        &[],
    );
    assert_eq!(
        st_oauth_ssrf, 403,
        "SSRF token_endpoint in OAuth authorize must be rejected"
    );

    // 4. Async HTTP listener must enforce request smuggling & malformed Content-Length rejection
    #[cfg(feature = "async-runtime")]
    {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ctx_c = ctx.clone();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = tokio::sync::watch::channel(false);
            let srv = tokio::spawn(async move {
                omni_mcp::server::serve_async_listener(
                    listener,
                    ctx_c,
                    #[cfg(feature = "tls-rustls")]
                    None,
                    rx,
                )
                .await;
            });

            let resp_bad_cl = tokio::task::spawn_blocking(move || {
                let mut tcp = std::net::TcpStream::connect(addr).unwrap();
                tcp.write_all(
                    b"POST /omni/mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: notanumber\r\n\r\n",
                )
                .unwrap();
                let mut s = String::new();
                let _ = tcp.read_to_string(&mut s);
                s
            })
            .await
            .unwrap();
            assert!(
                resp_bad_cl.starts_with("HTTP/1.1 400"),
                "Async listener must reject malformed Content-Length: {}",
                resp_bad_cl
            );

            let resp_smuggle = tokio::task::spawn_blocking(move || {
                let mut tcp = std::net::TcpStream::connect(addr).unwrap();
                tcp.write_all(
                    b"POST /omni/mcp HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{}",
                )
                .unwrap();
                let mut s = String::new();
                let _ = tcp.read_to_string(&mut s);
                s
            })
            .await
            .unwrap();
            assert!(
                resp_smuggle.starts_with("HTTP/1.1 400"),
                "Async listener must reject CL+TE smuggling: {}",
                resp_smuggle
            );

            let _ = tx.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(2), srv).await;
        });
    }

    // 5. Gateway Code Mode fuel & egress quota enforcement (end-to-end)
    let reg = MCPRegistry::new(None);
    let mut s = ServerRecord::new("q_srv", "Q", "Q", "eng");
    s.status = ToolStatus::Enabled;
    s.charter_policy.tenant_quota = Some(TenantQuotaPolicy {
        max_tool_calls_per_hour: 100,
        max_codemode_fuel_per_hour: 50,
        max_egress_bytes_per_hour: 10,
    });
    let mut t = ToolRecord::new("big", "Big", json!({"type": "object"}));
    t.status = ToolStatus::Enabled;
    s.add_tool(t);
    reg.register_server(s);
    let cache = InMemoryConfigCache::new(reg, 60.0);
    let tr = ProtocolTranslator::new(None, None);
    tr.mesh_client.set_allow_synthetic_mocks(true);
    let mut gw = MCPProxyGateway::new(cache, Some(tr));
    gw.quota_tracker = TenantQuotaTracker::new(TenantQuotaPolicy {
        max_tool_calls_per_hour: 100,
        max_codemode_fuel_per_hour: 5,
        max_egress_bytes_per_hour: 10_000,
    });
    let egress_resp = gw.handle_request(
        "q_srv",
        &RPCRequest::new(1, "tools/call", json!({"name": "big", "arguments": {}})),
        "agent",
        Some("tenant_egress"),
    );
    assert_eq!(
        egress_resp.error.as_ref().map(|e| e.code),
        Some(RATE_LIMITED),
        "Hourly egress quota breach must return -32004 RATE_LIMITED"
    );
    let cm_out = gw.execute_codemode(
        "async () => { const nums = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]; return nums.map(|x| x * 2); }",
        None,
        false,
        "agent",
        Some("tenant_fuel"),
        None,
    );
    assert_eq!(
        cm_out.error_code,
        Some(RATE_LIMITED),
        "Code Mode hourly fuel quota breach must return -32004 RATE_LIMITED"
    );

    // 6. Wasm recursion depth guard prevents stack overflow on self-calling Wasm plugin
    #[cfg(feature = "sandbox-wasm")]
    {
        use omni_mcp::data_plane::codemode::{WasmSandboxExecutor, WasmToolCallback};
        let mut wasm: Vec<u8> = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        let push_sec = |out: &mut Vec<u8>, id: u8, payload: &[u8]| {
            out.push(id);
            out.push(payload.len() as u8);
            out.extend_from_slice(payload);
        };
        push_sec(
            &mut wasm,
            1,
            &[
                0x02, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7e, 0x60, 0x01, 0x7f, 0x01, 0x7f,
            ],
        );
        let mut imp = vec![0x01, 0x04, b'o', b'm', b'n', b'i', 0x09];
        imp.extend_from_slice(b"call_tool");
        imp.extend_from_slice(&[0x00, 0x00]);
        push_sec(&mut wasm, 2, &imp);
        push_sec(&mut wasm, 3, &[0x02, 0x01, 0x00]);
        push_sec(&mut wasm, 5, &[0x01, 0x00, 0x01]);
        push_sec(&mut wasm, 6, &[0x01, 0x7f, 0x01, 0x41, 0x80, 0x08, 0x0b]);
        let mut exp = vec![0x03, 0x06];
        exp.extend_from_slice(b"memory");
        exp.extend_from_slice(&[0x02, 0x00, 0x0a]);
        exp.extend_from_slice(b"omni_alloc");
        exp.extend_from_slice(&[0x00, 0x01, 0x08]);
        exp.extend_from_slice(b"omni_run");
        exp.extend_from_slice(&[0x00, 0x02]);
        push_sec(&mut wasm, 7, &exp);
        let b_alloc = vec![
            0x01, 0x01, 0x7f, 0x23, 0x00, 0x22, 0x01, 0x20, 0x00, 0x6a, 0x24, 0x00, 0x20, 0x01,
            0x0b,
        ];
        let b_run = vec![0x00, 0x20, 0x00, 0x20, 0x01, 0x10, 0x00, 0x0b];
        let mut code = vec![0x02, b_alloc.len() as u8];
        code.extend_from_slice(&b_alloc);
        code.push(b_run.len() as u8);
        code.extend_from_slice(&b_run);
        push_sec(&mut wasm, 10, &code);

        let wasm_arc = Arc::new(wasm);
        let exec = Arc::new(WasmSandboxExecutor::new(200_000, 4 * 1024 * 1024));
        let depth_err_seen = Arc::new(AtomicBool::new(false));
        let flag = depth_err_seen.clone();
        fn make_recursive_cb(
            w: Arc<Vec<u8>>,
            e: Arc<WasmSandboxExecutor>,
            flag: Arc<AtomicBool>,
        ) -> WasmToolCallback {
            Arc::new(move |srv, tool, args| {
                let nested_cb = make_recursive_cb(w.clone(), e.clone(), flag.clone());
                let out = e.execute_wasm_abi_v1(
                    &w,
                    &json!({"server": srv, "tool": tool, "arguments": args.clone()}),
                    Some(nested_cb),
                );
                if let Some(err) = out.error {
                    if err.contains("recursion depth limit") {
                        flag.store(true, Ordering::SeqCst);
                    }
                    return Err(err);
                }
                Ok(out.result)
            })
        }
        let cb = make_recursive_cb(wasm_arc.clone(), exec.clone(), flag);
        let _ = exec.execute_wasm_abi_v1(
            &wasm_arc,
            &json!({"server": "self", "tool": "loop", "arguments": {}}),
            Some(cb),
        );
        assert!(
            depth_err_seen.load(Ordering::SeqCst),
            "Recursive Wasm omni::call_tool must trip recursion depth guard cleanly"
        );
    }

    // 7. OTLP batch worker re-queues failed spans on transient 500 and succeeds on retry
    let otlp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let otlp_addr = otlp_listener.local_addr().unwrap();
    let attempt_num = Arc::new(AtomicUsize::new(0));
    let att_c = attempt_num.clone();
    let stop_otlp = Arc::new(AtomicBool::new(false));
    let stop_o_c = stop_otlp.clone();
    let _ = otlp_listener.set_nonblocking(true);
    let otlp_srv = thread::spawn(move || {
        while !stop_o_c.load(Ordering::Relaxed) {
            match otlp_listener.accept() {
                Ok((mut s, _)) => {
                    let _ = s.set_nonblocking(false);
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut l = String::new();
                    let _ = r.read_line(&mut l);
                    let mut cl = 0usize;
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            if k.eq_ignore_ascii_case("content-length") {
                                cl = v.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    let mut b = vec![0u8; cl];
                    if cl > 0 {
                        let _ = r.read_exact(&mut b);
                    }
                    let n = att_c.fetch_add(1, Ordering::SeqCst);
                    if n < 3 {
                        let _ = s.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail");
                    } else {
                        let _ = s.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        );
                    }
                    let _ = s.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let worker = OtlpBatchWorker::new(
        format!("http://127.0.0.1:{}/v1/traces", otlp_addr.port()),
        80,
        1,
    );
    worker.start();
    worker.record_span(TraceSpan {
        trace_id: "00000000000000000000000000000099".to_string(),
        span_id: "0000000000000099".to_string(),
        parent_span_id: None,
        name: "retry.span".to_string(),
        service_name: "retry".to_string(),
        tool_name: "span".to_string(),
        start_time_us: 1_700_000_000_000_000,
        duration_us: 100,
        status: "OK".to_string(),
        attributes: BTreeMap::new(),
    });
    for _ in 0..50 {
        if worker.status().total_exported >= 1 {
            break;
        }
        thread::sleep(Duration::from_millis(40));
    }
    let st_otlp = worker.status();
    worker.shutdown();
    stop_otlp.store(true, Ordering::Relaxed);
    let _ = otlp_srv.join();
    assert!(
        st_otlp.total_failed >= 1 && st_otlp.total_exported >= 1,
        "OTLP worker must re-queue failed batch and succeed on retry; status: {:?}",
        st_otlp
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_v6_5_demo_seed_enables_synthetic_mocks_without_affecting_non_demo() {
    // 1. Demo-seeded gateway enables synthetic Muttley mocks so paused `accounting.refund` resumes cleanly
    let demo_dir = temp_test_dir("omni_v65_demo_resume");
    let demo_ctx = ProductionGatewayContext::new(Some(OmniConfig {
        data_dir: demo_dir.clone(),
        demo_seed: true,
        ..Default::default()
    }));

    let mut agent_headers = HashMap::new();
    agent_headers.insert(
        "authorization".to_string(),
        "Bearer agent_dev_token".to_string(),
    );
    let eval_body = json!({
        "code": "async () => { const res = await codemode.accounting.refund({ ride_id: \"r_demo_1\", reason: \"demo_dispute\" }); return res; }",
        "require_mutation_approval": true
    })
    .to_string_compact();
    let (status_eval, _, _, eval_resp_bytes) = demo_ctx.handle_http(
        "POST",
        "/api/codemode/eval",
        &agent_headers,
        eval_body.as_bytes(),
    );
    assert_eq!(status_eval, 200);
    let eval_json = JsonValue::parse(&String::from_utf8_lossy(&eval_resp_bytes)).unwrap();
    assert_eq!(
        eval_json.get("status").and_then(|v| v.as_str()),
        Some("APPROVAL_REQUIRED")
    );
    let exec_id = eval_json
        .get("execution_id")
        .and_then(|v| v.as_str())
        .expect("execution_id must be present")
        .to_string();

    let mut admin_headers = HashMap::new();
    admin_headers.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );
    let (status_list, _, _, list_bytes) =
        demo_ctx.handle_http("GET", "/api/codemode/suspended", &admin_headers, &[]);
    assert_eq!(status_list, 200);
    let list_json = JsonValue::parse(&String::from_utf8_lossy(&list_bytes)).unwrap();
    let frames = list_json
        .get("suspended_frames")
        .and_then(|v| v.as_array())
        .expect("suspended_frames array must be present");
    let target_frame = frames
        .iter()
        .find(|f| f.get("execution_id").and_then(|v| v.as_str()) == Some(exec_id.as_str()))
        .expect("suspended frame must appear in approval inbox");
    let arg_hash = target_frame
        .get("pending_arguments_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let resume_body = json!({
        "execution_id": exec_id,
        "arguments_digest": arg_hash
    })
    .to_string_compact();
    let (status_resume, _, _, resume_bytes) = demo_ctx.handle_http(
        "POST",
        "/api/codemode/resume",
        &admin_headers,
        resume_body.as_bytes(),
    );
    assert_eq!(status_resume, 200);
    let resume_json = JsonValue::parse(&String::from_utf8_lossy(&resume_bytes)).unwrap();
    assert!(
        resume_json.get("error").is_none() || resume_json.get("error").unwrap().is_null(),
        "Demo resume must succeed without error; got: {:?}",
        resume_json
    );
    assert_eq!(
        resume_json
            .get("result")
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_str()),
        Some("REFUNDED"),
        "Demo resume must return synthetic refund payload; got: {:?}",
        resume_json
    );

    // 2. Non-demo production context does NOT enable synthetic mocks unless compiled with `testing-mocks`
    #[cfg(not(feature = "testing-mocks"))]
    {
        let prod_dir = temp_test_dir("omni_v65_nondemo_prod");
        let prod_ctx = ProductionGatewayContext::new(Some(OmniConfig {
            data_dir: prod_dir.clone(),
            demo_seed: false,
            ..Default::default()
        }));
        let err = prod_ctx
            .gateway
            .translator
            .mesh_client
            .execute_downstream(
                "accounting",
                "refund",
                omni_mcp::models::ProtocolType::Http,
                b"{}",
                None,
                1000,
            )
            .expect_err("non-demo production context must refuse synthetic mocks");
        assert!(
            err.contains("disabled in production builds"),
            "unexpected non-demo error message: {}",
            err
        );
        let _ = fs::remove_dir_all(&prod_dir);
    }

    let _ = fs::remove_dir_all(&demo_dir);
}

// ============================================================================
// Helpers
// ============================================================================

#[cfg(feature = "sandbox-wasm")]
fn encode_base64_for_test(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(all(feature = "async-runtime", feature = "tls-rustls"))]
fn generate_test_ed25519_cert() -> (String, String, Vec<u8>) {
    use ring::signature::{Ed25519KeyPair, KeyPair};
    fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if body.len() < 128 {
            out.push(body.len() as u8);
        } else if body.len() < 256 {
            out.push(0x81);
            out.push(body.len() as u8);
        } else {
            out.push(0x82);
            out.push((body.len() >> 8) as u8);
            out.push((body.len() & 0xff) as u8);
        }
        out.extend_from_slice(body);
        out
    }
    fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(bytes.len() + 1);
        body.push(0x00);
        body.extend_from_slice(bytes);
        der_tlv(0x03, &body)
    }
    fn pem(label: &str, der: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut b64 = String::new();
        for chunk in der.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
            let n = (b0 << 16) | (b1 << 8) | b2;
            b64.push(TABLE[((n >> 18) & 63) as usize] as char);
            b64.push(TABLE[((n >> 12) & 63) as usize] as char);
            b64.push(if chunk.len() > 1 {
                TABLE[((n >> 6) & 63) as usize] as char
            } else {
                '='
            });
            b64.push(if chunk.len() > 2 {
                TABLE[(n & 63) as usize] as char
            } else {
                '='
            });
        }
        format!(
            "-----BEGIN {}-----\n{}\n-----END {}-----\n",
            label, b64, label
        )
    }

    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let alg_id = der_tlv(0x30, &[0x06, 0x03, 0x2b, 0x65, 0x70]);
    let cn_attr = der_tlv(
        0x30,
        &[
            &[0x06, 0x03, 0x55, 0x04, 0x03][..],
            &der_tlv(0x0c, b"localhost"),
        ]
        .concat(),
    );
    let name = der_tlv(0x30, &der_tlv(0x31, &cn_attr));
    let validity = der_tlv(
        0x30,
        &[
            der_tlv(0x17, b"200101000000Z"),
            der_tlv(0x17, b"350101000000Z"),
        ]
        .concat(),
    );
    let spki = der_tlv(
        0x30,
        &[alg_id.clone(), der_bit_string(kp.public_key().as_ref())].concat(),
    );
    let san_seq = der_tlv(0x30, &der_tlv(0x82, b"localhost"));
    let san_ext = der_tlv(
        0x30,
        &[
            &[0x06, 0x03, 0x55, 0x1d, 0x11][..],
            &der_tlv(0x04, &san_seq),
        ]
        .concat(),
    );
    let exts = der_tlv(0xa3, &der_tlv(0x30, &san_ext));
    let tbs = der_tlv(
        0x30,
        &[
            der_tlv(0xa0, &der_tlv(0x02, &[0x02])),
            der_tlv(0x02, &[0x01]),
            alg_id.clone(),
            name.clone(),
            validity,
            name,
            spki,
            exts,
        ]
        .concat(),
    );
    let sig = kp.sign(&tbs);
    let cert_der = der_tlv(0x30, &[tbs, alg_id, der_bit_string(sig.as_ref())].concat());
    (
        pem("CERTIFICATE", &cert_der),
        pem("PRIVATE KEY", pkcs8.as_ref()),
        cert_der,
    )
}
