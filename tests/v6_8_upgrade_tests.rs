#![forbid(unsafe_code)]
//! Comprehensive Integration Tests for OmniMCP v6.8.0 Major Upgrade (Milestones 1–5).

use omni_mcp::config::{get_config, set_config, OmniConfig};
use omni_mcp::control_plane::{MCPRegistry, SkillAccessScope, SkillEngine};
use omni_mcp::data_plane::codemode::{
    CodeModeError, CodeModeExecutor, CodeModeTypeGenerator, HostCallbacks, SandboxLimits,
};
use omni_mcp::data_plane::{
    HttpDownstreamClient, InMemoryConfigCache, MCPProxyGateway, TaskDispatchOutcome, TaskManager,
};
use omni_mcp::extension::OmniMCPServer;
use omni_mcp::json::JsonValue;
use omni_mcp::models::{
    build_server_capabilities, is_required_capability_supported, CapabilityScope,
    ClientCapabilitiesMeta, ElicitationPolicyRule, McpTaskRecord, McpTaskStatus,
    MutationCompensationSpec, OAuthFlowRecord, ProtocolType, RPCRequest, ServerRecord,
    SkillFileEntry, SkillRecord, ToolGroupEntry, ToolGroupRecord, ToolRecord, ToolStatus,
    EXT_MCP_FILES, EXT_MCP_SKILLS, EXT_MCP_TASKS, MISSING_REQUIRED_CLIENT_CAPABILITY, UNAUTHORIZED,
};
use omni_mcp::server::ProductionGatewayContext;
use omni_mcp::storage::{
    sha256_hex, AzureBlobConfig, AzureFileBlobBackend, FileBlobBackend, FileBlobStore,
    GcsBlobConfig, GcsFileBlobBackend, HttpBlobConfig, HttpFileBlobBackend, LocalFileBlobBackend,
    PostgresFileBlobBackend, S3BlobConfig, S3FileBlobBackend, WalRegistryStore,
};
use omni_mcp::upstream::UpstreamOAuthTokenStore;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static GLOBAL_CFG_LOCK: Mutex<()> = Mutex::new(());

fn temp_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "omni_v68_{}_{}",
        prefix,
        omni_mcp::models::generate_ulid()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn make_test_config(prefix: &str) -> OmniConfig {
    let dir = temp_dir(prefix);
    let mut cfg = OmniConfig::from_env();
    cfg.data_dir = dir.to_string_lossy().into_owned();
    cfg.uploads_dir = dir.join("uploads");
    cfg.secrets_dir = dir.join("secrets");
    cfg.demo_seed = false;
    cfg.auth_secret = String::new();
    cfg.storage_backend = "wal".to_string();
    cfg.enable_skills_extension = true;
    cfg.enable_tasks_extension = true;
    cfg.enable_file_uploads_extension = false;
    cfg
}

// ============================================================================
// Milestone 1: Unified 2026-07-28 Capabilities & server/discover Parity
// ============================================================================

#[test]
fn test_v6_8_capability_negotiation_and_server_discover_all_surfaces() {
    let _cfg_guard = GLOBAL_CFG_LOCK.lock().unwrap();
    let mut cfg = make_test_config("m1_caps");
    cfg.enable_file_uploads_extension = false;
    set_config(cfg.clone());

    // 1. ClientCapabilitiesMeta backward compatibility
    let empty_elicit = JsonValue::parse(r#"{"capabilities":{"elicitation":{}}}"#).unwrap();
    let meta1 = ClientCapabilitiesMeta::from_params(&empty_elicit);
    assert!(meta1.elicitation_form);
    assert!(
        !meta1.elicitation_url,
        "Empty elicitation:{{}} must NOT enable url-mode elicitation"
    );

    let url_elicit =
        JsonValue::parse(r#"{"capabilities":{"elicitation":{"form":{},"url":{}}}}"#).unwrap();
    let meta2 = ClientCapabilitiesMeta::from_params(&url_elicit);
    assert!(meta2.elicitation_form);
    assert!(meta2.elicitation_url);

    // 2. Feature-gating of io.modelcontextprotocol/files
    let caps_default = build_server_capabilities(&cfg, CapabilityScope::Global);
    let exts_default = caps_default.get("extensions").expect("extensions present");
    assert!(exts_default.get(EXT_MCP_TASKS).is_some());
    assert!(exts_default.get(EXT_MCP_SKILLS).is_some());
    assert!(
        exts_default.get(EXT_MCP_FILES).is_none(),
        "io.modelcontextprotocol/files must be disabled by default"
    );
    assert!(is_required_capability_supported(EXT_MCP_SKILLS, &cfg));
    assert!(is_required_capability_supported(EXT_MCP_TASKS, &cfg));
    assert!(!is_required_capability_supported(EXT_MCP_FILES, &cfg));

    // 3. Verify server/discover parity across /omni/mcp, /groups/<slug>/mcp, and /<service>/mcp
    let ctx = ProductionGatewayContext::new(Some(cfg.clone()));
    let mut srv = ServerRecord::new("orders-svc", "Orders Service", "Orders API", "commerce");
    let mut t = ToolRecord::new(
        "get_order",
        "Fetch order by ID",
        JsonValue::parse(r#"{"type":"object","properties":{"id":{"type":"string"}}}"#).unwrap(),
    );
    t.status = ToolStatus::Enabled;
    srv.add_tool(t);
    ctx.registry.register_server(srv);
    ctx.registry.save_tool_group(ToolGroupRecord {
        slug: "commerce-grp".to_string(),
        display_name: "Commerce Group".to_string(),
        description: "Commerce tools".to_string(),
        entries: vec![ToolGroupEntry {
            server_name: "orders-svc".to_string(),
            tool_name: None,
        }],
        created_at: 1700000000.0,
    });
    ctx.cache.refresh_now();

    let discover_body = br#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{}}"#;
    let hdrs = HashMap::new();

    for route in ["/omni/mcp", "/groups/commerce-grp/mcp", "/orders-svc/mcp"] {
        let (status, _, _, body) = ctx.handle_http("POST", route, &hdrs, discover_body);
        assert_eq!(status, 200, "server/discover failed on {}", route);
        let parsed = JsonValue::parse(&String::from_utf8_lossy(&body)).unwrap();
        let res = parsed.get("result").expect("result object");
        assert_eq!(
            res.get("resultType").and_then(|v| v.as_str()),
            Some("complete")
        );
        let exts = res
            .get("capabilities")
            .and_then(|c| c.get("extensions"))
            .expect("extensions advertised");
        assert!(
            exts.get(EXT_MCP_TASKS).is_some(),
            "tasks missing on {}",
            route
        );
        assert!(
            exts.get(EXT_MCP_SKILLS).is_some(),
            "skills missing on {}",
            route
        );
        assert!(
            exts.get(EXT_MCP_FILES).is_none(),
            "files must be gated off by default on {}",
            route
        );
    }

    // 4. Verify clientCapabilities.required rejects disabled io.modelcontextprotocol/files
    let init_req = RPCRequest::new(
        2,
        "initialize",
        JsonValue::parse(
            r#"{"protocolVersion":"2026-07-28","clientCapabilities":{"required":["io.modelcontextprotocol/files"]}}"#,
        )
        .unwrap(),
    );
    let init_resp = ctx.omni_mcp.handle_request(&init_req, "agent", None);
    assert_eq!(
        init_resp.error.as_ref().map(|e| e.code),
        Some(MISSING_REQUIRED_CLIENT_CAPABILITY)
    );

    // 5. Enable file uploads and verify advertisement + negotiation succeeds
    cfg.enable_file_uploads_extension = true;
    set_config(cfg.clone());
    let caps_enabled = build_server_capabilities(&cfg, CapabilityScope::Global);
    assert!(caps_enabled
        .get("extensions")
        .and_then(|e| e.get(EXT_MCP_FILES))
        .is_some());
    let init_ok = ctx.omni_mcp.handle_request(&init_req, "agent", None);
    assert!(init_ok.error.is_none());

    // Restore default config
    cfg.enable_file_uploads_extension = false;
    set_config(cfg);
}

// ============================================================================
// Milestone 2: Skills over MCP (SEP-2640) Limits, Determinism & Group Isolation
// ============================================================================

#[test]
fn test_v6_8_skills_limits_deterministic_manifest_and_group_isolation() {
    let dir = temp_dir("m2_skills");
    let db_path = dir.join("skills.db").to_string_lossy().into_owned();

    // 1. Validation: reject path traversal
    let mut bad_path = SkillRecord {
        slug: "bad-path".to_string(),
        name: String::new(),
        description: String::new(),
        version: String::new(),
        group_slug: None,
        server_scope: None,
        auto_generated: false,
        files: vec![
            SkillFileEntry::new(
                "SKILL.md",
                "text/markdown",
                "---\nname: Bad\ndescription: Traversal\n---\n# Bad",
            ),
            SkillFileEntry::new("../etc/passwd", "text/plain", "root:x:0:0"),
        ],
        manifest_sha256: String::new(),
        updated_at: 0.0,
    };
    assert!(bad_path.validate().is_err());

    // 2. Validation: reject >512 files
    let mut too_many_files = vec![SkillFileEntry::new(
        "SKILL.md",
        "text/markdown",
        "---\nname: Huge\ndescription: Too many files\n---\n",
    )];
    for i in 0..512 {
        too_many_files.push(SkillFileEntry::new(
            format!("refs/f{}.txt", i),
            "text/plain",
            "x",
        ));
    }
    let mut over_count = SkillRecord {
        slug: "over-count".to_string(),
        name: String::new(),
        description: String::new(),
        version: String::new(),
        group_slug: None,
        server_scope: None,
        auto_generated: false,
        files: too_many_files,
        manifest_sha256: String::new(),
        updated_at: 0.0,
    };
    assert!(over_count.validate().is_err());

    // 3. Validation: reject missing frontmatter
    let mut no_fm = SkillRecord {
        slug: "no-fm".to_string(),
        name: String::new(),
        description: String::new(),
        version: String::new(),
        group_slug: None,
        server_scope: None,
        auto_generated: false,
        files: vec![SkillFileEntry::new(
            "SKILL.md",
            "text/markdown",
            "# No frontmatter",
        )],
        manifest_sha256: String::new(),
        updated_at: 0.0,
    };
    assert!(no_fm.validate().is_err());

    // 4. Deterministic manifest_sha256 across WAL restart + group isolation
    let expected_sha = {
        let store = Arc::new(WalRegistryStore::new(&db_path));
        let registry = MCPRegistry::new(Some(store.clone()));
        let saved_a = registry
            .save_skill(SkillRecord {
                slug: "alpha-ops".to_string(),
                name: String::new(),
                description: String::new(),
                version: String::new(),
                group_slug: Some("group-a".to_string()),
                server_scope: None,
                auto_generated: false,
                files: vec![
                    SkillFileEntry::new(
                        "SKILL.md",
                        "text/markdown",
                        "---\nname: Alpha Ops\ndescription: Alpha runbook\n---\n# Alpha",
                    ),
                    SkillFileEntry::new(
                        "references/guide.md",
                        "text/markdown",
                        "Step 1: Verify health",
                    ),
                ],
                manifest_sha256: String::new(),
                updated_at: 1700000000.0,
            })
            .expect("Valid skill A");

        let _saved_b = registry
            .save_skill(SkillRecord {
                slug: "beta-secret".to_string(),
                name: String::new(),
                description: String::new(),
                version: String::new(),
                group_slug: Some("group-b".to_string()),
                server_scope: None,
                auto_generated: false,
                files: vec![SkillFileEntry::new(
                    "SKILL.md",
                    "text/markdown",
                    "---\nname: Beta Ops\ndescription: Beta runbook\n---\n# Beta",
                )],
                manifest_sha256: String::new(),
                updated_at: 1700000000.0,
            })
            .expect("Valid skill B");

        store.checkpoint_snapshot();
        saved_a.manifest_sha256
    };

    // Reopen from disk and assert identical manifest_sha256 and strict group isolation
    let reopened_store = Arc::new(WalRegistryStore::new(&db_path));
    let reopened_reg = MCPRegistry::new(Some(reopened_store));
    let engine = SkillEngine::new(reopened_reg.clone());
    let scope_a = SkillAccessScope {
        group_slug: Some("group-a".to_string()),
        server_scope: None,
    };
    let listed_a = engine.list_skills(&scope_a);
    assert!(listed_a.iter().any(|s| s.slug == "alpha-ops"));
    assert!(
        !listed_a.iter().any(|s| s.slug == "beta-secret"),
        "Group A must NOT see Group B skills"
    );
    let loaded_a = engine.get_skill("alpha-ops", &scope_a).expect("alpha-ops");
    assert_eq!(loaded_a.manifest_sha256, expected_sha);
    assert!(engine.get_skill("beta-secret", &scope_a).is_none());

    // resources/read & resources/directory/read
    let (uri, mime, text) = engine
        .read_skill_resource("skill://alpha-ops/references/guide.md", &scope_a)
        .expect("read skill resource");
    assert_eq!(uri, "skill://alpha-ops/references/guide.md");
    assert_eq!(mime, "text/markdown");
    assert_eq!(text, "Step 1: Verify health");
    assert!(engine
        .read_skill_resource("skill://beta-secret/SKILL.md", &scope_a)
        .is_err());

    let dir_entries = engine
        .read_skill_directory("skill://alpha-ops", &scope_a)
        .expect("directory read");
    assert!(dir_entries
        .iter()
        .any(|e| e.get("name").and_then(|v| v.as_str()) == Some("SKILL.md")));
    assert!(dir_entries
        .iter()
        .any(
            |e| e.get("name").and_then(|v| v.as_str()) == Some("references")
                && e.get("type").and_then(|v| v.as_str()) == Some("directory")
        ));
}

// ============================================================================
// Milestone 3: Postgres HA Tasks, Group Isolation, Cancel -> Saga & Channel Split
// ============================================================================

#[test]
fn test_v6_8_tasks_ha_group_isolation_cancel_and_notify_channel_split() {
    let _cfg_guard = GLOBAL_CFG_LOCK.lock().unwrap();
    let cfg = make_test_config("m3_tasks");
    set_config(cfg.clone());
    let store = Arc::new(WalRegistryStore::new(":memory:"));
    let registry = MCPRegistry::new(Some(store));
    let cache = InMemoryConfigCache::new(registry.clone(), 5.0);
    let gateway = MCPProxyGateway::new(cache.clone(), None);
    let task_mgr = TaskManager::new(registry.clone(), "pod-a", 16, 40, 3600);
    let skills = SkillEngine::new(registry.clone());
    let file_store = FileBlobStore::from_config(&cfg, registry.clone());
    let omni = OmniMCPServer::with_runtime(
        cache.clone(),
        gateway.clone(),
        skills,
        task_mgr.clone(),
        file_store,
    );

    // 1. Grace-window promotion -> resultType: "task" + cooperative cancellation via omni_task_changed
    let started = Arc::new(AtomicBool::new(false));
    let started_clone = started.clone();
    let outcome = task_mgr
        .run_with_grace(
            "agent-1",
            Some("group-a".to_string()),
            None,
            false,
            Some(30),
            move |cancel_flag| {
                started_clone.store(true, Ordering::SeqCst);
                for _ in 0..200 {
                    if cancel_flag.load(Ordering::SeqCst) {
                        return Err((-32603, "Cancelled cooperatively".to_string()));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(JsonValue::Str("finished".to_string()))
            },
        )
        .expect("task submitted");

    let task_rec = match outcome {
        TaskDispatchOutcome::PromotedAsync(t) => t,
        TaskDispatchOutcome::CompletedSync(_) => panic!("Expected async promotion"),
    };
    assert_eq!(task_rec.group_slug.as_deref(), Some("group-a"));

    // 2. Cross-group isolation: group-b MUST be denied on tasks/get & tasks/cancel
    let get_req = RPCRequest::new(
        1,
        "tasks/get",
        JsonValue::parse(&format!(r#"{{"taskId":"{}"}}"#, task_rec.task_id)).unwrap(),
    );
    let denied_get = omni.handle_request_scoped(&get_req, "agent-2", None, Some("group-b"), None);
    assert_eq!(
        denied_get.error.as_ref().map(|e| e.code),
        Some(UNAUTHORIZED),
        "Group B must NOT inspect Group A's task"
    );

    let allowed_get = omni.handle_request_scoped(&get_req, "agent-1", None, Some("group-a"), None);
    assert!(allowed_get.error.is_none());

    // 3. Simulate remote `omni_task_changed` notification: must wake cancel flag WITHOUT reloading catalog
    let reloads_before = registry.reload_count();
    task_mgr.handle_remote_task_notification(&format!("cancel:{}", task_rec.task_id));
    assert_eq!(
        registry.reload_count(),
        reloads_before,
        "omni_task_changed must NOT trigger MCPRegistry reload"
    );

    // Wait briefly for worker to observe cancellation
    thread::sleep(Duration::from_millis(80));

    // 4. Spec-compliant tasks/cancel returns `{}` ack
    let cancel_req = RPCRequest::new(
        2,
        "tasks/cancel",
        JsonValue::parse(&format!(r#"{{"taskId":"{}"}}"#, task_rec.task_id)).unwrap(),
    );
    let cancel_resp =
        omni.handle_request_scoped(&cancel_req, "agent-1", None, Some("group-a"), None);
    assert!(cancel_resp.error.is_none());
    assert_eq!(
        cancel_resp.result.unwrap().to_string_compact(),
        "{}",
        "GA tasks/cancel must return empty {{}} ack"
    );

    // 5. Cooperative cancellation inside Code Mode VM triggers CodeModeError::Cancelled
    let vm_cancel = Arc::new(AtomicBool::new(true));
    let mut invoke_cb = |_: &str, _: &str, _: Vec<JsonValue>| -> Result<JsonValue, CodeModeError> {
        Ok(JsonValue::Null)
    };
    let mut batch_cb =
        |_: Vec<(String, String, Vec<JsonValue>)>| -> Result<Vec<JsonValue>, CodeModeError> {
            Ok(Vec::new())
        };
    let search_cb = |_: &str| JsonValue::Array(Vec::new());
    let desc_cb = |_: &str, _: Option<&str>| String::new();
    let mut host = HostCallbacks {
        invoke_tool: &mut invoke_cb,
        invoke_parallel_batch: &mut batch_cb,
        search_catalog: &search_cb,
        describe_types: &desc_cb,
        cancel_flag: Some(vm_cancel),
    };
    let out = CodeModeExecutor::new(SandboxLimits::default()).execute(
        "let x = 1; let y = 2; return x + y;",
        &mut host,
        "00-00000000000000000000000000000001-0000000000000001-01",
    );
    assert_eq!(out.status, "ERROR");
    assert!(out.error.unwrap_or_default().contains("Cancelled"));

    // 6. Spec-compliant tasks/update with `inputResponses` returns `{}` ack
    let mut input_task = McpTaskRecord::new(
        "task_input_1",
        "pod-a",
        "agent-1",
        Some("group-a".to_string()),
        None,
        3600,
    );
    input_task.status = McpTaskStatus::InputRequired;
    task_mgr.record_task(&input_task);
    let update_req = RPCRequest::new(
        3,
        "tasks/update",
        JsonValue::parse(
            r#"{"taskId":"task_input_1","inputResponses":[{"action":"accept","content":{"approved":true,"justification":"change ticket CHG-42"}}]}"#,
        )
        .unwrap(),
    );
    let update_resp =
        omni.handle_request_scoped(&update_req, "approver-1", None, Some("group-a"), None);
    assert!(update_resp.error.is_none());
    assert_eq!(
        update_resp.result.unwrap().to_string_compact(),
        "{}",
        "GA tasks/update must return empty {{}} ack"
    );
}

// ============================================================================
// Milestone 4: Governance Elicitation Secret Guard & Cross-Pod HA OAuth
// ============================================================================

#[test]
fn test_v6_8_elicitation_form_secret_guard_and_cross_pod_url_oauth() {
    // 1. Strict Secret-Safety & Primitive-Schema Validator
    for secret_key in [
        "api_key",
        "access_token",
        "client_secret",
        "user_password",
        "db_pass",
        "auth_header",
        "aws_credential",
    ] {
        let bad_schema = JsonValue::parse(&format!(
            r#"{{"type":"object","properties":{{"{}":{{"type":"string"}}}}}}"#,
            secret_key
        ))
        .unwrap();
        assert!(
            ElicitationPolicyRule::validate_elicitation_schema(&bad_schema).is_err(),
            "Secret key '{}' must be rejected in form elicitation",
            secret_key
        );
    }

    let non_primitive =
        JsonValue::parse(r#"{"type":"object","properties":{"nested":{"type":"object"}}}"#).unwrap();
    assert!(ElicitationPolicyRule::validate_elicitation_schema(&non_primitive).is_err());

    let valid_schema = JsonValue::parse(
        r#"{"type":"object","properties":{"justification":{"type":"string"},"confirm":{"type":"boolean"}}}"#,
    )
    .unwrap();
    assert!(ElicitationPolicyRule::validate_elicitation_schema(&valid_schema).is_ok());

    // 2. Cross-Pod HA OAuth: Pod A initiates PKCE flow -> Pod B completes callback -> Pod A reads token
    let shared_dir = temp_dir("m4_oauth");
    let shared_db = shared_dir.join("shared.db").to_string_lossy().into_owned();
    let shared_store = Arc::new(WalRegistryStore::new(&shared_db));
    let shared_reg = MCPRegistry::new(Some(shared_store));

    // Pod A persists PKCE flow to shared omni_oauth_flows
    let (verifier, _challenge) = UpstreamOAuthTokenStore::generate_pkce_pair();
    shared_reg.save_oauth_flow(&OAuthFlowRecord {
        state: "st_cross_pod_1".to_string(),
        server_name: "external-mcp:linear".to_string(),
        elicitation_id: Some("elicit_1".to_string()),
        code_verifier: verifier,
        token_url: "https://api.linear.app/oauth/token".to_string(),
        client_id: "omni-client".to_string(),
        redirect_uri: "https://gateway.example.com/api/v1/oauth/callback".to_string(),
        created_at: 1700000000.0,
        expires_at: 2900000000.0,
    });

    // Pod B (separate secrets_dir!) consumes the flow and seals AEAD token into shared omni_oauth_tokens
    let flow_on_pod_b = shared_reg
        .take_oauth_flow("st_cross_pod_1")
        .expect("Pod B must see Pod A's PKCE flow");
    assert_eq!(flow_on_pod_b.server_name, "external-mcp:linear");
    assert!(
        shared_reg.take_oauth_flow("st_cross_pod_1").is_none(),
        "PKCE flow state must be single-use"
    );

    let pod_b_store =
        UpstreamOAuthTokenStore::new(shared_dir.join("pod_b_secrets"), Some("shared-cluster-key"))
            .with_registry(shared_reg.clone());
    pod_b_store
        .save_token("external-mcp:linear", "lin_oauth_tok_cross_pod_999")
        .expect("Pod B must seal AEAD token");

    // Pod A (with empty local secrets_dir!) reads the token immediately from shared omni_oauth_tokens
    let pod_a_store =
        UpstreamOAuthTokenStore::new(shared_dir.join("pod_a_secrets"), Some("shared-cluster-key"))
            .with_registry(shared_reg.clone());
    let loaded_tok = pod_a_store
        .load_token("external-mcp:linear")
        .expect("Pod A must read Pod B's sealed AEAD token");
    assert_eq!(loaded_tok, "lin_oauth_tok_cross_pod_999");

    // 3. Verify Code Mode mutation + compensation spec compiles cleanly
    let mut comp_map = BTreeMap::new();
    comp_map.insert("id".to_string(), "output.id".to_string());
    let _spec = MutationCompensationSpec {
        forward_tool: "create".to_string(),
        compensating_server: None,
        compensating_tool: "delete".to_string(),
        arg_mapping: comp_map,
    };
}

// ============================================================================
// Milestone 5: Multi-Cloud FileBlobBackend, Streaming Upload, Downstream & Wasm ABI v2
// ============================================================================

type MockBlobMap = Arc<Mutex<HashMap<String, Vec<u8>>>>;

fn spawn_mock_blob_server() -> (String, MockBlobMap) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock blob server");
    let addr = listener.local_addr().unwrap();
    let store: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
    let store_bg = store.clone();

    thread::spawn(move || {
        for stream_res in listener.incoming().take(32) {
            let Ok(mut stream) = stream_res else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let mut header_end = None;
            let mut content_len = 0usize;
            while header_end.is_none() {
                match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = Some(pos);
                            let head = String::from_utf8_lossy(&buf[..pos]);
                            for line in head.lines() {
                                if let Some((k, v)) = line.split_once(':') {
                                    if k.trim().eq_ignore_ascii_case("content-length") {
                                        content_len = v.trim().parse().unwrap_or(0);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let Some(h_end) = header_end else { continue };
            while buf.len() < h_end + 4 + content_len {
                match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            let head = String::from_utf8_lossy(&buf[..h_end]).into_owned();
            let first_line = head.lines().next().unwrap_or("");
            let mut parts = first_line.split_whitespace();
            let method = parts.next().unwrap_or("GET");
            let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/");
            let body = buf[h_end + 4..].to_vec();

            let (status, resp_body) = match method {
                "PUT" | "POST" => {
                    store_bg.lock().unwrap().insert(path.to_string(), body);
                    ("200 OK", b"{\"ok\":true}".to_vec())
                }
                "GET" => {
                    if let Some(data) = store_bg.lock().unwrap().get(path).cloned() {
                        ("200 OK", data)
                    } else {
                        ("404 Not Found", b"not found".to_vec())
                    }
                }
                "DELETE" => {
                    store_bg.lock().unwrap().remove(path);
                    ("204 No Content", Vec::new())
                }
                _ => ("405 Method Not Allowed", Vec::new()),
            };
            let resp = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status,
                resp_body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(&resp_body);
            let _ = stream.flush();
        }
    });

    (format!("http://127.0.0.1:{}", addr.port()), store)
}

#[test]
fn test_v6_8_dual_listener_streaming_uploads_and_wasm_abi_v2() {
    let _cfg_guard = GLOBAL_CFG_LOCK.lock().unwrap();
    let dir = temp_dir("m5_files");
    let mut cfg = make_test_config("m5_files_ctx");
    cfg.enable_file_uploads_extension = true;
    cfg.file_upload_max_bytes = 64 * 1024 * 1024;
    set_config(cfg.clone());

    let wal = Arc::new(WalRegistryStore::new(":memory:"));
    let reg = MCPRegistry::new(Some(wal));

    // 1. Exercise all 6 FileBlobBackend implementations
    let payload = b"OmniMCP v6.8.0 universal multi-cloud file blob payload!";
    let expected_sha = sha256_hex(payload);

    // 1a. Local backend
    let local_be = LocalFileBlobBackend::new(dir.join("local_blobs"));
    local_be
        .put_bytes("k1.blob", payload, "application/octet-stream")
        .unwrap();
    assert_eq!(local_be.get_bytes("k1.blob").unwrap(), payload);
    assert_eq!(local_be.get_range("k1.blob", 8, 6).unwrap(), b"v6.8.0");
    local_be.delete("k1.blob").unwrap();

    // 1b. Postgres/DB chunked backend
    let pg_be = PostgresFileBlobBackend::new(reg.clone());
    pg_be
        .put_bytes("k2.blob", payload, "application/octet-stream")
        .unwrap();
    assert_eq!(pg_be.get_bytes("k2.blob").unwrap(), payload);
    pg_be.delete("k2.blob").unwrap();

    // 1c. S3 (SigV4), GCS, Azure (SharedKey HMAC-SHA256), HTTP/WebDAV against live mock HTTP server
    let (mock_url, _mock_store) = spawn_mock_blob_server();

    let s3_be = S3FileBlobBackend::new(S3BlobConfig {
        endpoint: mock_url.clone(),
        bucket: "omni-bucket".to_string(),
        region: "us-east-1".to_string(),
        access_key_id: "AKIAEXAMPLE".to_string(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        path_style: true,
    });
    let sig_hdr = s3_be.sign_sigv4(
        "PUT",
        "127.0.0.1",
        "/omni-bucket/k3.blob",
        &expected_sha,
        "20261125T120000Z",
    );
    assert!(sig_hdr
        .starts_with("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/20261125/us-east-1/s3/aws4_request"));
    s3_be
        .put_bytes("k3.blob", payload, "application/octet-stream")
        .unwrap();
    assert_eq!(s3_be.get_bytes("k3.blob").unwrap(), payload);
    s3_be.delete("k3.blob").unwrap();

    let gcs_be = GcsFileBlobBackend::new(GcsBlobConfig {
        endpoint: mock_url.clone(),
        bucket: "omni-gcs".to_string(),
        bearer_token: "ya29.mock-token".to_string(),
        sa_key_file: None,
    });
    gcs_be
        .put_bytes("k4.blob", payload, "application/octet-stream")
        .unwrap();
    assert_eq!(gcs_be.get_bytes("k4.blob").unwrap(), payload);

    let azure_be = AzureFileBlobBackend::new(AzureBlobConfig {
        endpoint: mock_url.clone(),
        account: "omniacct".to_string(),
        container: "blobs".to_string(),
        shared_key_base64: Some("c2VjcmV0LWtleS1mb3ItYXp1cmU=".to_string()),
        sas_token: None,
        bearer_token: None,
    });
    let az_hdr = azure_be
        .sign_shared_key(
            "PUT",
            Some(payload.len()),
            "application/octet-stream",
            "Wed, 25 Nov 2026 12:00:00 GMT",
            Some("BlockBlob"),
            "k5.blob",
        )
        .unwrap();
    assert!(az_hdr.starts_with("SharedKey omniacct:"));
    azure_be
        .put_bytes("k5.blob", payload, "application/octet-stream")
        .unwrap();
    assert_eq!(azure_be.get_bytes("k5.blob").unwrap(), payload);

    let http_be = HttpFileBlobBackend::new(HttpBlobConfig {
        base_url: format!("{}/dav", mock_url),
        auth_header: Some("Bearer dav-secret".to_string()),
    });
    http_be
        .put_bytes("k6.blob", payload, "application/octet-stream")
        .unwrap();
    assert_eq!(http_be.get_bytes("k6.blob").unwrap(), payload);

    // 2. Upload 12 MiB file via gateway route (bypassing 10 MiB JSON cap) while JSON-RPC rejects >10 MiB
    let ctx = ProductionGatewayContext::new(Some(cfg.clone()));
    let big_upload_id = "file_12mb_test";
    let upload_token = FileBlobStore::sign_token("upload", big_upload_id);
    let twelve_mb = vec![0x42u8; 12 * 1024 * 1024];
    let (up_status, _, _, up_body) = ctx.handle_http(
        "PUT",
        &format!(
            "/api/v1/files/upload/{}?token={}&filename=report.bin",
            big_upload_id, upload_token
        ),
        &HashMap::new(),
        &twelve_mb,
    );
    assert_eq!(up_status, 200, "12 MiB streaming upload must succeed");
    let up_json = JsonValue::parse(&String::from_utf8_lossy(&up_body)).unwrap();
    assert_eq!(
        up_json.get("size_bytes").and_then(|v| v.as_u64()),
        Some((12 * 1024 * 1024) as u64)
    );

    // JSON-RPC > 10 MiB must still be rejected with 413
    let (rpc_status, _, _, _) = ctx.handle_http("POST", "/omni/mcp", &HashMap::new(), &twelve_mb);
    assert_eq!(rpc_status, 413, "JSON-RPC must remain capped at 10 MiB");

    // 3. HttpDownstreamClient::post_stream_file() streaming 64 KiB chunks to mock endpoint
    let small_rec = ctx
        .file_store
        .put_file(
            Some("file_stream_test".to_string()),
            "data.csv",
            "text/csv",
            "agent",
            &vec![b'A'; 200_000],
        )
        .unwrap();
    let http_client = HttpDownstreamClient::new();
    let stream_resp = http_client
        .post_stream_file(
            &format!("{}/ingest", mock_url),
            &ctx.file_store,
            &small_rec.file_id,
            Some("file"),
            None,
            Some(10),
        )
        .expect("post_stream_file must succeed");
    assert!(String::from_utf8_lossy(&stream_resp).contains("ok"));

    // 4. CodeModeTypeGenerator emits `OmniFileHandle` for `x-mcp-file` properties
    let mut doc_srv = ServerRecord::new("docs-svc", "Docs", "PDF parser", "docs");
    doc_srv.status = ToolStatus::Enabled;
    let mut parse_tool = ToolRecord::new(
        "parse_pdf",
        "Parse uploaded PDF",
        JsonValue::parse(
            r#"{"type":"object","properties":{"document":{"type":"string","x-mcp-file":true}},"required":["document"]}"#,
        )
        .unwrap(),
    );
    parse_tool.status = ToolStatus::Enabled;
    doc_srv.add_tool(parse_tool);
    let ts = CodeModeTypeGenerator::generate_all_declarations(&[doc_srv]);
    assert!(ts.contains("interface OmniFileHandle"));
    assert!(ts.contains("document: OmniFileHandle;"));

    // 5. Wasm Guest ABI v2: live assembled Wasm binary invoking omni::file_stat & omni::file_read_chunk
    #[cfg(feature = "sandbox-wasm")]
    {
        use omni_mcp::data_plane::codemode::{WasmAttachedFile, WasmSandboxExecutor};

        fn push_uleb(buf: &mut Vec<u8>, mut val: u32) {
            loop {
                let mut byte = (val & 0x7f) as u8;
                val >>= 7;
                if val != 0 {
                    byte |= 0x80;
                }
                buf.push(byte);
                if val == 0 {
                    break;
                }
            }
        }
        fn push_sleb(buf: &mut Vec<u8>, mut val: i64) {
            loop {
                let byte = (val & 0x7f) as u8;
                val >>= 7;
                let done = (val == 0 && (byte & 0x40) == 0) || (val == -1 && (byte & 0x40) != 0);
                buf.push(if done { byte } else { byte | 0x80 });
                if done {
                    break;
                }
            }
        }
        fn push_section(wasm: &mut Vec<u8>, id: u8, payload: &[u8]) {
            wasm.push(id);
            push_uleb(wasm, payload.len() as u32);
            wasm.extend_from_slice(payload);
        }
        fn push_str(buf: &mut Vec<u8>, s: &str) {
            push_uleb(buf, s.len() as u32);
            buf.extend_from_slice(s.as_bytes());
        }

        // Build valid Wasm module:
        // - imports omni::file_stat(i32,i32)->i64 (func 0) and omni::file_read_chunk(i32,i32,i64,i32,i32)->i32 (func 1)
        // - exports memory (2 pages), omni_alloc(i32)->i32 (func 2), omni_run(i32,i32)->i64 (func 3)
        // - data segment places "wasm_f1" at offset 0
        // - omni_run:
        //     1. calls file_stat(0, 7) and checks result > 0 (traps if <= 0)
        //     2. calls file_read_chunk(0, 7, 0, 64, 9_999_999) OOB and checks result == -1
        //     3. calls file_read_chunk(0, 7, 0, 64, 2048) into linear memory
        //     4. calls file_stat(0, 7) a second time (allows mid-flight cancel_flag test)
        //     5. returns ((2048 << 32) | bytes_read)
        let mut wasm = Vec::from(&b"\0asm\x01\0\0\0"[..]);
        // Type section (1)
        let mut types = Vec::new();
        push_uleb(&mut types, 3);
        types.extend_from_slice(&[0x60, 2, 0x7f, 0x7f, 1, 0x7e]); // type 0: (i32,i32)->i64
        types.extend_from_slice(&[0x60, 5, 0x7f, 0x7f, 0x7e, 0x7f, 0x7f, 1, 0x7f]); // type 1: (i32,i32,i64,i32,i32)->i32
        types.extend_from_slice(&[0x60, 1, 0x7f, 1, 0x7f]); // type 2: (i32)->i32
        push_section(&mut wasm, 1, &types);

        // Import section (2)
        let mut imports = Vec::new();
        push_uleb(&mut imports, 2);
        push_str(&mut imports, "omni");
        push_str(&mut imports, "file_stat");
        imports.extend_from_slice(&[0x00, 0]);
        push_str(&mut imports, "omni");
        push_str(&mut imports, "file_read_chunk");
        imports.extend_from_slice(&[0x00, 1]);
        push_section(&mut wasm, 2, &imports);

        // Function section (3)
        let mut funcs = Vec::new();
        push_uleb(&mut funcs, 2);
        funcs.push(2); // func 2: omni_alloc
        funcs.push(0); // func 3: omni_run
        push_section(&mut wasm, 3, &funcs);

        // Memory section (5): 1 memory, min=2 pages
        let mem = [1u8, 0x00, 2];
        push_section(&mut wasm, 5, &mem);

        // Export section (7)
        let mut exports = Vec::new();
        push_uleb(&mut exports, 3);
        push_str(&mut exports, "memory");
        exports.extend_from_slice(&[0x02, 0]);
        push_str(&mut exports, "omni_alloc");
        exports.extend_from_slice(&[0x00, 2]);
        push_str(&mut exports, "omni_run");
        exports.extend_from_slice(&[0x00, 3]);
        push_section(&mut wasm, 7, &exports);

        // Code section (10)
        let mut alloc_body = Vec::new();
        alloc_body.push(0); // 0 locals
        alloc_body.push(0x41);
        push_sleb(&mut alloc_body, 1024);
        alloc_body.push(0x0b);

        let mut run_body = Vec::new();
        run_body.extend_from_slice(&[1, 1, 0x7f]); // 1 local of type i32 (local 2 = bytes_read)
                                                   // 1. file_stat(0, 7) <= 0 => unreachable
        run_body.extend_from_slice(&[0x41, 0, 0x41, 7, 0x10, 0]); // call file_stat
        run_body.extend_from_slice(&[0x42, 0, 0x57]); // i64.const 0; i64.le_s
        run_body.extend_from_slice(&[0x04, 0x40, 0x00, 0x0b]); // if unreachable end
                                                               // 2. file_read_chunk(0, 7, 0, 64, 9_999_999) != -1 => unreachable
        run_body.extend_from_slice(&[0x41, 0, 0x41, 7, 0x42, 0, 0x41]);
        push_sleb(&mut run_body, 64);
        run_body.push(0x41);
        push_sleb(&mut run_body, 9_999_999);
        run_body.extend_from_slice(&[0x10, 1]); // call file_read_chunk
        run_body.push(0x41);
        push_sleb(&mut run_body, -1);
        run_body.push(0x47); // i32.ne
        run_body.extend_from_slice(&[0x04, 0x40, 0x00, 0x0b]); // if unreachable end
                                                               // 3. bytes_read = file_read_chunk(0, 7, 0, 64, 2048)
        run_body.extend_from_slice(&[0x41, 0, 0x41, 7, 0x42, 0, 0x41]);
        push_sleb(&mut run_body, 64);
        run_body.push(0x41);
        push_sleb(&mut run_body, 2048);
        run_body.extend_from_slice(&[0x10, 1, 0x21, 2]); // call file_read_chunk; local.set 2
                                                         // 4. Second file_stat(0, 7) (triggers mid-flight cancel_flag check)
        run_body.extend_from_slice(&[0x41, 0, 0x41, 7, 0x10, 0, 0x1a]); // call file_stat; drop
                                                                        // 5. Return (2048i64 << 32) | (local.get 2 as u64)
        run_body.push(0x42);
        push_sleb(&mut run_body, 2048i64 << 32);
        run_body.extend_from_slice(&[0x20, 2, 0xad, 0x84, 0x0b]); // local.get 2; i64.extend_i32_u; i64.or; end

        let mut code = Vec::new();
        push_uleb(&mut code, 2);
        push_uleb(&mut code, alloc_body.len() as u32);
        code.extend_from_slice(&alloc_body);
        push_uleb(&mut code, run_body.len() as u32);
        code.extend_from_slice(&run_body);
        push_section(&mut wasm, 10, &code);

        // Data section (11): active segment at mem 0 offset 0 = "wasm_f1"
        let mut data = Vec::new();
        push_uleb(&mut data, 1);
        data.extend_from_slice(&[0x00, 0x41, 0x00, 0x0b]); // mem 0, i32.const 0, end
        push_str(&mut data, "wasm_f1");
        push_section(&mut wasm, 11, &data);

        let store_for_wasm = ctx.file_store.clone();
        let file_json = br#"{"hello":"wasm-v2-verified"}"#;
        let f_rec = store_for_wasm
            .put_file(
                Some("wasm_f1".to_string()),
                "hello.json",
                "application/json",
                "agent",
                file_json,
            )
            .unwrap();
        let store_ok = store_for_wasm.clone();
        let attached_ok = WasmAttachedFile {
            file_id: f_rec.file_id.clone(),
            size_bytes: f_rec.size_bytes,
            reader: Arc::new(move |fid, offset, len| store_ok.read_range(fid, offset, len)),
        };
        let wasm_exec = WasmSandboxExecutor::default();
        let ok_out = wasm_exec.execute_wasm_abi_v2(
            &wasm,
            &JsonValue::object(),
            None,
            vec![attached_ok],
            None,
        );
        assert_eq!(
            ok_out.status, "OK",
            "Live Wasm ABI v2 guest must succeed: {:?}",
            ok_out.error
        );
        assert_eq!(
            ok_out.result.get("hello").and_then(|v| v.as_str()),
            Some("wasm-v2-verified"),
            "Wasm guest must read back exact bytes via omni::file_read_chunk"
        );

        // Mid-flight cooperative cancellation inside Wasm host callback MUST report "Cancelled", NOT FUEL_EXHAUSTED
        let mid_cancel = Arc::new(AtomicBool::new(false));
        let mid_cancel_cb = mid_cancel.clone();
        let store_cancel = store_for_wasm.clone();
        let attached_cancel = WasmAttachedFile {
            file_id: f_rec.file_id.clone(),
            size_bytes: f_rec.size_bytes,
            reader: Arc::new(move |fid, offset, len| {
                mid_cancel_cb.store(true, Ordering::SeqCst);
                store_cancel.read_range(fid, offset, len)
            }),
        };
        let cancel_out = wasm_exec.execute_wasm_abi_v2(
            &wasm,
            &JsonValue::object(),
            None,
            vec![attached_cancel],
            Some(mid_cancel),
        );
        assert_eq!(
            cancel_out.status, "ERROR",
            "Mid-flight Wasm cancellation must NOT be classified as FUEL_EXHAUSTED"
        );
        assert!(cancel_out.error.unwrap_or_default().contains("Cancelled"));
    }

    // Restore default config
    let mut reset = get_config();
    reset.enable_file_uploads_extension = false;
    set_config(reset);
}

#[test]
fn test_v6_8_deep_fixes_chunked_listeners_url_elicitation_legacy_isolation_and_panic_safety() {
    let _cfg_guard = GLOBAL_CFG_LOCK.lock().unwrap();
    let mut cfg = make_test_config("m5_deep_fixes");
    cfg.enable_file_uploads_extension = true;
    cfg.file_upload_max_bytes = 64 * 1024 * 1024;
    cfg.public_base_url = "https://gw.omni.internal".to_string();
    set_config(cfg.clone());

    let ctx = Arc::new(ProductionGatewayContext::new(Some(cfg.clone())));

    // 1. Live TCP sync listener test: 12 MiB Transfer-Encoding: chunked upload succeeds,
    //    non-upload chunked -> 501, Transfer-Encoding: gzip -> 501, >10 MiB JSON-RPC -> 413.
    let send_raw_sync = |ctx_ref: &ProductionGatewayContext, raw_req: &[u8]| -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind sync test");
        let addr = listener.local_addr().unwrap();
        let req_owned = raw_req.to_vec();
        let client_handle = thread::spawn(move || {
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            let _ = client.set_read_timeout(Some(Duration::from_secs(10)));
            client.write_all(&req_owned).unwrap();
            client.flush().unwrap();
            let mut resp = Vec::new();
            let _ = client.read_to_end(&mut resp);
            String::from_utf8_lossy(&resp).into_owned()
        });
        let (server_stream, _) = listener.accept().unwrap();
        omni_mcp::server::handle_stream(server_stream, ctx_ref);
        client_handle.join().unwrap()
    };

    // 1a. 12 MiB chunked upload over real TCP socket (sync listener)
    let upload_id = "chunked_12mb_sync";
    let token = FileBlobStore::sign_token("upload", upload_id);
    let mut chunked_req = format!(
        "PUT /api/v1/files/upload/{}?token={}&filename=big.bin HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
        upload_id, token
    )
    .into_bytes();
    let chunk_data = vec![b'Z'; 4 * 1024 * 1024];
    for _ in 0..3 {
        chunked_req.extend_from_slice(format!("{:x}\r\n", chunk_data.len()).as_bytes());
        chunked_req.extend_from_slice(&chunk_data);
        chunked_req.extend_from_slice(b"\r\n");
    }
    chunked_req.extend_from_slice(b"0\r\n\r\n");
    let sync_up_resp = send_raw_sync(&ctx, &chunked_req);
    assert!(
        sync_up_resp.starts_with("HTTP/1.1 200 OK"),
        "Sync listener 12 MiB chunked upload must succeed, got: {}",
        &sync_up_resp[..sync_up_resp.len().min(200)]
    );

    // 1b. Non-upload chunked -> 501
    let bad_chunked = b"POST /omni/mcp HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
    let resp_501 = send_raw_sync(&ctx, bad_chunked);
    assert!(resp_501.starts_with("HTTP/1.1 501"));

    // 1c. Unsupported Transfer-Encoding: gzip on upload route -> 501
    let gzip_req = format!(
        "PUT /api/v1/files/upload/{}?token={} HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: gzip\r\nConnection: close\r\n\r\n",
        upload_id, token
    );
    let resp_gzip = send_raw_sync(&ctx, gzip_req.as_bytes());
    assert!(resp_gzip.starts_with("HTTP/1.1 501"));

    // 1d. Live async listener test (when async-runtime feature enabled)
    #[cfg(feature = "async-runtime")]
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx_async = ctx.clone();
        let chunked_clone = chunked_req.clone();
        let async_resp = rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (_tx, rx) = tokio::sync::watch::channel(false);
            let server_task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                omni_mcp::server::handle_async_stream(stream, ctx_async, rx).await;
            });
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client.write_all(&chunked_clone).await.unwrap();
            client.flush().await.unwrap();
            let mut buf = Vec::new();
            let _ = client.read_to_end(&mut buf).await;
            let _ = server_task.await;
            String::from_utf8_lossy(&buf).into_owned()
        });
        assert!(
            async_resp.starts_with("HTTP/1.1 200 OK"),
            "Async listener 12 MiB chunked upload must succeed, got: {}",
            &async_resp[..async_resp.len().min(200)]
        );
    }

    // 2. URL-mode elicitation when external OAuth server has no token & client declared elicitation.url
    let mut ext_srv = ServerRecord::new("external-mcp:jira", "Jira", "Jira API", "eng");
    ext_srv.upstream_config = Some(omni_mcp::models::UpstreamTransportConfig::StreamableHttp {
        url: "https://mcp.jira.example.com".to_string(),
        bearer_ref: None,
        header_refs: BTreeMap::new(),
        session_mode: omni_mcp::models::SessionMode::Stateless,
    });
    let mut jtool = ToolRecord::new("list_issues", "List Jira issues", JsonValue::object());
    jtool.status = ToolStatus::Enabled;
    ext_srv.add_tool(jtool);
    ctx.registry.register_server(ext_srv);
    ctx.cache.refresh_now();

    let url_elicit_req = RPCRequest::new(
        10,
        "tools/call",
        JsonValue::parse(
            r#"{
                "name": "invoke_tool",
                "arguments": {"server_name": "external-mcp:jira", "tool_name": "list_issues", "arguments": {}},
                "capabilities": {"elicitation": {"url": {}}}
            }"#,
        )
        .unwrap(),
    );
    let url_resp = ctx
        .omni_mcp
        .handle_request(&url_elicit_req, "agent-1", None);
    assert!(url_resp.error.is_none());
    let url_res = url_resp.result.expect("url elicitation result");
    assert_eq!(
        url_res.get("resultType").and_then(|v| v.as_str()),
        Some("input_required")
    );
    let req0 = url_res
        .get("inputRequests")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .expect("inputRequests[0]");
    assert_eq!(
        req0.get("method").and_then(|v| v.as_str()),
        Some("elicitation/create")
    );
    assert_eq!(
        req0.get("params")
            .and_then(|p| p.get("mode"))
            .and_then(|v| v.as_str()),
        Some("url")
    );
    let url_str = req0
        .get("params")
        .and_then(|p| p.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        url_str.starts_with("https://gw.omni.internal/api/v1/oauth/start?"),
        "URL-mode elicitation must use OMNI_PUBLIC_BASE_URL, got: {}",
        url_str
    );

    // 3. Cross-group isolation on legacy SuspendedExecutionFrame (tasks/cancel & legacy tasks/update)
    ctx.registry.save_tool_group(ToolGroupRecord {
        slug: "grp-readonly".to_string(),
        display_name: "Readonly".to_string(),
        description: "RO".to_string(),
        entries: vec![],
        created_at: 1700000000.0,
    });
    let legacy_frame = omni_mcp::models::SuspendedExecutionFrame {
        execution_id: "exec_legacy_secret".to_string(),
        script_hash: "hash1".to_string(),
        script_source: "return 1;".to_string(),
        instruction_ptr: 0,
        target_var: None,
        serialized_scope: JsonValue::object(),
        pending_server: "external-mcp:jira".to_string(),
        pending_tool: "delete_issue".to_string(),
        pending_arguments: JsonValue::object(),
        pending_reason: "Destructive".to_string(),
        policy_context_hash: "pch".to_string(),
        caller_actor: "agent".to_string(),
        logs: Vec::new(),
        completed_mutations: Vec::new(),
        created_at: 1700000000.0,
        pending_arguments_hash: "pah".to_string(),
        expires_at: 2900000000.0,
        approver_identity: None,
        is_consumed: false,
        frame_hmac: String::new(),
    };
    ctx.registry.save_suspended_frame(legacy_frame);

    let cancel_legacy = RPCRequest::new(
        11,
        "tasks/cancel",
        JsonValue::parse(r#"{"taskId":"exec_legacy_secret"}"#).unwrap(),
    );
    let denied_cancel = ctx.omni_mcp.handle_request_scoped(
        &cancel_legacy,
        "agent",
        None,
        Some("grp-readonly"),
        None,
    );
    assert_eq!(
        denied_cancel.error.as_ref().map(|e| e.code),
        Some(UNAUTHORIZED),
        "Group 'grp-readonly' must NOT cancel another group's legacy suspended frame"
    );

    let resume_legacy = RPCRequest::new(
        12,
        "tasks/update",
        JsonValue::parse(r#"{"taskId":"exec_legacy_secret","action":"resume"}"#).unwrap(),
    );
    let denied_resume = ctx.omni_mcp.handle_request_scoped(
        &resume_legacy,
        "agent",
        None,
        Some("grp-readonly"),
        None,
    );
    assert_eq!(
        denied_resume.error.as_ref().map(|e| e.code),
        Some(UNAUTHORIZED),
        "Group 'grp-readonly' must NOT resume another group's legacy suspended frame"
    );

    // 4. TaskManager panic safety: panicking worker must release semaphore & decrement active to 0
    let panic_mgr = TaskManager::new(ctx.registry.clone(), "pod-panic", 1, 5, 3600);
    let _ = panic_mgr.run_with_grace("agent", None, None, true, None, |_flag| {
        panic!("simulated worker panic");
    });
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        panic_mgr.metrics.active.load(Ordering::Relaxed),
        0,
        "Panicking task worker must decrement active count"
    );
    // Because max_concurrent_tasks == 1, a second task MUST succeed (proving semaphore was released)
    let second = panic_mgr.run_with_grace("agent", None, None, false, Some(200), |_flag| {
        Ok(JsonValue::Str("recovered".to_string()))
    });
    assert!(
        second.is_ok(),
        "Semaphore slot must be released after worker panic"
    );

    // Restore default config
    let mut reset = get_config();
    reset.enable_file_uploads_extension = false;
    set_config(reset);
}

#[test]
fn test_v6_8_audit_gaps_closed() {
    let _cfg_guard = GLOBAL_CFG_LOCK.lock().unwrap();
    use omni_mcp::data_plane::translators::with_active_cancel_flag;
    use omni_mcp::models::{
        FileBlobRecord, McpTaskRecord, McpTaskStatus, OAuthFlowRecord, OAuthTokenRecord,
        PendingUpstreamCallback, SkillFileEntry, SkillRecord, ToolGroupEntry,
    };
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;

    let tmp_dir = temp_dir("gaps_closed");
    let mut cfg = make_test_config("gaps_closed");
    cfg.data_dir = tmp_dir.to_string_lossy().into_owned();
    cfg.uploads_dir = tmp_dir.join("uploads");
    cfg.enable_file_uploads_extension = true;
    cfg.enable_tasks_extension = true;
    cfg.enable_skills_extension = true;
    cfg.task_sync_grace_ms = 60;
    cfg.public_base_url = "https://gateway.test".to_string();
    set_config(cfg.clone());

    let ctx = ProductionGatewayContext::new(Some(cfg.clone()));

    // Gap 1: Live ProtocolTranslator WasmPlugin executes ABI v2 with attached_files & cancel_flag
    #[cfg(feature = "sandbox-wasm")]
    {
        let blob = ctx
            .file_store
            .put_file(
                Some("wasm_f1".to_string()),
                "hello.txt",
                "text/plain",
                "agent",
                b"hello-abi-v2",
            )
            .unwrap();

        // Minimal valid Wasm module importing omni::file_stat(i32,i32)->i64, exporting memory, omni_alloc, omni_run
        fn push_sec(wasm: &mut Vec<u8>, id: u8, payload: &[u8]) {
            wasm.push(id);
            wasm.push(payload.len() as u8);
            wasm.extend_from_slice(payload);
        }
        let mut wasm = Vec::from(&b"\0asm\x01\0\0\0"[..]);
        // Type section: 0: (i32,i32)->i64, 1: (i32)->i32
        push_sec(
            &mut wasm,
            1,
            &[2, 0x60, 2, 0x7f, 0x7f, 1, 0x7e, 0x60, 1, 0x7f, 1, 0x7f],
        );
        // Import section: omni::file_stat (func 0, type 0)
        push_sec(
            &mut wasm,
            2,
            &[
                1, 4, b'o', b'm', b'n', b'i', 9, b'f', b'i', b'l', b'e', b'_', b's', b't', b'a',
                b't', 0, 0,
            ],
        );
        // Function section: func 1 (type 1 = omni_alloc), func 2 (type 0 = omni_run)
        push_sec(&mut wasm, 3, &[2, 1, 0]);
        // Memory section: 1 page
        push_sec(&mut wasm, 5, &[1, 0, 1]);
        // Export section: memory, omni_alloc, omni_run
        let mut exp = Vec::new();
        exp.push(3);
        exp.extend_from_slice(&[6, b'm', b'e', b'm', b'o', b'r', b'y', 2, 0]);
        exp.extend_from_slice(&[
            10, b'o', b'm', b'n', b'i', b'_', b'a', b'l', b'l', b'o', b'c', 0, 1,
        ]);
        exp.extend_from_slice(&[8, b'o', b'm', b'n', b'i', b'_', b'r', b'u', b'n', 0, 2]);
        push_sec(&mut wasm, 7, &exp);
        // Code section:
        let alloc_body = [0u8, 0x41, 0x80, 0x08, 0x0b];
        let run_body = [
            0u8, 0x41, 0x00, // i32.const 0 ("wasm_f1")
            0x41, 0x07, // i32.const 7
            0x10, 0x00, // call file_stat
            0x42, 0x00, // i64.const 0
            0x57, // i64.le_s
            0x04, 0x40, 0x00, 0x0b, // if unreachable end
            0x42, 0x80, 0x80, 0x80, 0x80, 0x90, 0x01, // i64.const (1024 << 32)
            0x42, 0x02, // i64.const 2
            0x84, // i64.or
            0x0b,
        ];
        let mut code_sec = Vec::new();
        code_sec.push(2);
        code_sec.push(alloc_body.len() as u8);
        code_sec.extend_from_slice(&alloc_body);
        code_sec.push(run_body.len() as u8);
        code_sec.extend_from_slice(&run_body);
        push_sec(&mut wasm, 10, &code_sec);
        // Data section: seg 0 at 0 = "wasm_f1", seg 1 at 1024 = "{}"
        let mut data_sec = Vec::new();
        data_sec.push(2);
        data_sec.extend_from_slice(&[
            0, 0x41, 0, 0x0b, 7, b'w', b'a', b's', b'm', b'_', b'f', b'1',
        ]);
        data_sec.extend_from_slice(&[0, 0x41, 0x80, 0x08, 0x0b, 2, b'{', b'}']);
        push_sec(&mut wasm, 11, &data_sec);

        fn b64_enc(bytes: &[u8]) -> String {
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for c in bytes.chunks(3) {
                let b0 = c[0] as u32;
                let b1 = if c.len() > 1 { c[1] as u32 } else { 0 };
                let b2 = if c.len() > 2 { c[2] as u32 } else { 0 };
                let n = (b0 << 16) | (b1 << 8) | b2;
                out.push(T[((n >> 18) & 63) as usize] as char);
                out.push(T[((n >> 12) & 63) as usize] as char);
                out.push(if c.len() > 1 {
                    T[((n >> 6) & 63) as usize] as char
                } else {
                    '='
                });
                out.push(if c.len() > 2 {
                    T[(n & 63) as usize] as char
                } else {
                    '='
                });
            }
            out
        }

        let mut wasm_tool = ToolRecord::new(
            "inspect_blob",
            "Inspects attached blob via ABI v2",
            JsonValue::parse(
                r#"{"type":"object","properties":{"doc":{"type":"object","x-mcp-file":true}}}"#,
            )
            .unwrap(),
        );
        wasm_tool.protocol = ProtocolType::WasmPlugin;
        wasm_tool.status = ToolStatus::Enabled;
        wasm_tool.wasm_module_base64 = Some(b64_enc(&wasm));

        let ok_res = ctx.gateway.translator.execute_tool_call(
            "wasm-svc",
            &wasm_tool,
            &JsonValue::parse(&format!(r#"{{"doc":{{"fileId":"{}"}}}}"#, blob.file_id)).unwrap(),
            None,
            None,
        );
        assert!(
            ok_res.is_ok(),
            "Live WasmPlugin ABI v2 must resolve attached file: {:?}",
            ok_res
        );

        // With pre-cancelled flag via with_active_cancel_flag, must abort
        let cancelled_flag = Arc::new(AtomicBool::new(true));
        let cancel_res = with_active_cancel_flag(cancelled_flag, || {
            ctx.gateway.translator.execute_tool_call(
                "wasm-svc",
                &wasm_tool,
                &JsonValue::parse(r#"{"doc":{"fileId":"wasm_f1"}}"#).unwrap(),
                None,
                None,
            )
        });
        assert!(
            cancel_res.is_err(),
            "Pre-cancelled flag must abort Wasm ABI v2"
        );
    }

    // Gap 2: Live HTTP downstream streaming via post_stream_file in ProtocolTranslator
    #[cfg(feature = "transport-http")]
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv_handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut acc = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    acc.extend_from_slice(&buf[..n]);
                    let s = String::from_utf8_lossy(&acc);
                    if s.contains("stream-body-bytes") && s.ends_with("--\r\n") {
                        break;
                    }
                }
                let req_str = String::from_utf8_lossy(&acc).to_string();
                assert!(
                    req_str.contains("X-Omni-File-Id: stream_f1")
                        && req_str.contains("multipart/form-data"),
                    "HTTP downstream must receive streamed multipart file, got: {}",
                    req_str
                );
                let body = r#"{"streamed":true}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let _ = ctx
            .file_store
            .put_file(
                Some("stream_f1".to_string()),
                "payload.txt",
                "text/plain",
                "agent",
                b"stream-body-bytes",
            )
            .unwrap();

        let mut http_srv = ServerRecord::new("stream-svc", "Stream Svc", "HTTP streaming", "docs");
        http_srv.status = ToolStatus::Enabled;
        let mut upload_tool = ToolRecord::new(
            "upload_doc",
            "Upload document",
            JsonValue::parse(
                r#"{"type":"object","properties":{"doc":{"type":"object","x-mcp-file":true}},"required":["doc"]}"#,
            )
            .unwrap(),
        );
        upload_tool.status = ToolStatus::Enabled;
        upload_tool.protocol = ProtocolType::Http;
        upload_tool.downstream_endpoint = format!("http://{}/upload", addr);
        http_srv.add_tool(upload_tool);
        ctx.registry.register_server(http_srv);
        ctx.cache.refresh_now();

        // Gap 5: /<service>/mcp parity (missing required x-mcp-file -> upload_required MRTR)
        let mrtr_resp = ctx.gateway.handle_request(
            "stream-svc",
            &RPCRequest::new(
                JsonValue::from(1),
                "tools/call",
                JsonValue::parse(r#"{"name":"upload_doc","arguments":{}}"#).unwrap(),
            ),
            "agent",
            None,
        );
        assert_eq!(
            mrtr_resp
                .result
                .as_ref()
                .and_then(|r| r.get("resultType"))
                .and_then(|v| v.as_str()),
            Some("upload_required")
        );

        // /<service>/mcp parity with fileId -> streams via post_stream_file
        let stream_call_resp = ctx.gateway.handle_request(
            "stream-svc",
            &RPCRequest::new(
                JsonValue::from(2),
                "tools/call",
                JsonValue::parse(
                    r#"{"name":"upload_doc","arguments":{"doc":{"fileId":"stream_f1"}}}"#,
                )
                .unwrap(),
            ),
            "agent",
            None,
        );
        assert!(
            stream_call_resp.error.is_none(),
            "Streamed /<service>/mcp call must succeed: {:?}",
            stream_call_resp.error
        );
        let _ = srv_handle.join();

        // Gap 5: /groups/<slug>/mcp forwards clientCapabilities, _meta, and task
        ctx.registry.save_tool_group(ToolGroupRecord {
            slug: "grp-parity".to_string(),
            display_name: "Parity Group".to_string(),
            description: "Group parity".to_string(),
            entries: vec![ToolGroupEntry {
                server_name: "stream-svc".to_string(),
                tool_name: Some("upload_doc".to_string()),
            }],
            created_at: 0.0,
        });
        let mut mut_srv = ServerRecord::new("mut-svc", "Mut Svc", "Mutating", "ops");
        mut_srv.status = ToolStatus::Enabled;
        let mut mut_tool = ToolRecord::new(
            "do_mut",
            "Mutate",
            JsonValue::parse(r#"{"type":"object"}"#).unwrap(),
        );
        mut_tool.status = ToolStatus::Enabled;
        mut_tool.is_mutation = true;
        mut_srv.add_tool(mut_tool);
        ctx.registry.register_server(mut_srv);
        ctx.registry.save_tool_group(ToolGroupRecord {
            slug: "grp-mut".to_string(),
            display_name: "Mut Group".to_string(),
            description: "Mut group".to_string(),
            entries: vec![ToolGroupEntry {
                server_name: "mut-svc".to_string(),
                tool_name: Some("do_mut".to_string()),
            }],
            created_at: 0.0,
        });
        ctx.cache.refresh_now();

        let grp_req = JsonValue::parse(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mut-svc_do_mut","arguments":{},"clientCapabilities":{"elicitation":{"form":{}}},"_meta":{"require_mutation_approval":true}}}"#,
        )
        .unwrap();
        let mut headers = HashMap::new();
        headers.insert(
            "authorization".to_string(),
            "Bearer admin_dev_token".to_string(),
        );
        let (grp_status, _, _, grp_body) = ctx.handle_http(
            "POST",
            "/groups/grp-mut/mcp",
            &headers,
            grp_req.to_string_compact().as_bytes(),
        );
        assert_eq!(grp_status, 200);
        let grp_json = JsonValue::parse(&String::from_utf8_lossy(&grp_body)).unwrap();
        assert_eq!(
            grp_json
                .get("result")
                .and_then(|r| r.get("resultType"))
                .and_then(|v| v.as_str()),
            Some("input_required"),
            "Group MCP call must preserve clientCapabilities & _meta for elicitation.form MRTR, got: {}",
            grp_json
        );
    }

    // Gap 3: POST /api/v1/tasks/:id/respond updates input_required task
    let mut elicit_task = McpTaskRecord::new(
        "task_elicit_rest_1".to_string(),
        "pod-1".to_string(),
        "agent",
        None,
        Some("mut-svc".to_string()),
        300,
    );
    elicit_task.status = McpTaskStatus::InputRequired;
    ctx.task_manager.record_task(&elicit_task);
    let mut admin_hdrs = HashMap::new();
    admin_hdrs.insert(
        "authorization".to_string(),
        "Bearer admin_dev_token".to_string(),
    );
    let (resp_status, _, _, resp_body) = ctx.handle_http(
        "POST",
        "/api/v1/tasks/task_elicit_rest_1/respond",
        &admin_hdrs,
        br#"{"action":"accept","content":{"approved":true,"justification":"ok"}}"#,
    );
    assert_eq!(
        resp_status,
        200,
        "POST /api/v1/tasks/:id/respond must succeed: {}",
        String::from_utf8_lossy(&resp_body)
    );

    // Gap 4: Grace-window promotion in OmniMCPServer codemode when client advertises tasks
    let fast_resp = ctx.omni_mcp.handle_request(
        &RPCRequest::new(
            JsonValue::from(10),
            "tools/call",
            JsonValue::parse(
                r#"{"name":"codemode","clientCapabilities":{"extensions":{"io.modelcontextprotocol/tasks":{}}},"arguments":{"code":"async () => 42"}}"#,
            )
            .unwrap(),
        ),
        "agent",
        None,
    );
    assert_eq!(
        fast_resp
            .result
            .as_ref()
            .and_then(|r| r.get("resultType"))
            .and_then(|v| v.as_str()),
        Some("complete"),
        "Fast codemode with tasks capability must return synchronously within grace window"
    );

    // Slow-path grace-window promotion: exceeding task_sync_grace_ms promotes to resultType: "task"
    let mut slow_cfg = get_config();
    let prev_grace = slow_cfg.task_sync_grace_ms;
    slow_cfg.task_sync_grace_ms = 5;
    set_config(slow_cfg);
    let slow_resp = ctx.omni_mcp.handle_request(
        &RPCRequest::new(
            JsonValue::from(11),
            "tools/call",
            JsonValue::parse(
                r#"{"name":"codemode","clientCapabilities":{"extensions":{"io.modelcontextprotocol/tasks":{}}},"arguments":{"code":"async () => { let s = 0; let i = 0; while (i < 60000) { s += i; i += 1; } return s; }","max_operations":200000}}"#,
            )
            .unwrap(),
        ),
        "agent",
        None,
    );
    let mut restore_grace_cfg = get_config();
    restore_grace_cfg.task_sync_grace_ms = prev_grace;
    set_config(restore_grace_cfg);
    assert_eq!(
        slow_resp
            .result
            .as_ref()
            .and_then(|r| r.get("resultType"))
            .and_then(|v| v.as_str()),
        Some("task"),
        "Slow codemode exceeding grace window must promote to resultType: 'task', got: {:?}",
        slow_resp
    );

    // Gap 6: migrate_to_v6_8 copies all 10 collections from WAL -> SQLite
    #[cfg(feature = "storage-sqlite")]
    {
        let mig_dir = temp_dir("mig_v6_8");
        let wal_path = mig_dir.join("omni_registry.db");
        let wal = omni_mcp::storage::WalRegistryStore::new(wal_path.to_string_lossy().to_string());
        let mut s = ServerRecord::new("mig-srv", "Mig", "desc", "ops");
        s.status = ToolStatus::Enabled;
        wal.save_server(&s);
        wal.save_skill(&SkillRecord {
            slug: "mig-skill".to_string(),
            name: "Mig Skill".to_string(),
            description: "desc".to_string(),
            version: "1.0.0".to_string(),
            group_slug: None,
            server_scope: None,
            auto_generated: false,
            files: vec![SkillFileEntry {
                path: "SKILL.md".to_string(),
                mime_type: "text/markdown".to_string(),
                content: "# Mig".to_string(),
                size_bytes: 5,
                sha256: "abc".to_string(),
            }],
            manifest_sha256: "sha_mig".to_string(),
            updated_at: 1.0,
        });
        wal.save_mcp_task(&McpTaskRecord::new(
            "mig_task_1".to_string(),
            "pod-1".to_string(),
            "agent",
            None,
            None,
            60,
        ));
        wal.save_oauth_flow(&OAuthFlowRecord {
            state: "mig_state".to_string(),
            server_name: "mig-srv".to_string(),
            elicitation_id: None,
            code_verifier: "v".to_string(),
            token_url: "https://example.com/token".to_string(),
            client_id: "cid".to_string(),
            redirect_uri: "https://example.com/cb".to_string(),
            created_at: 1.0,
            expires_at: 100.0,
        });
        wal.save_oauth_token(&OAuthTokenRecord {
            server_name: "mig-srv".to_string(),
            sealed_envelope_hex: "deadbeef".to_string(),
            updated_at: 1.0,
        });
        wal.save_upstream_callback(&PendingUpstreamCallback {
            callback_id: "cb_mig_1".to_string(),
            server_name: "mig-srv".to_string(),
            upstream_request_id: JsonValue::from(1),
            method: "sampling/createMessage".to_string(),
            params: JsonValue::object(),
            mode: "HITL".to_string(),
            status: "pending".to_string(),
            created_at: 1.0,
        });
        wal.save_file_blob(&FileBlobRecord {
            file_id: "blob_mig_1".to_string(),
            filename: "f.txt".to_string(),
            mime_type: "text/plain".to_string(),
            size_bytes: 3,
            sha256: "deadbeef".to_string(),
            backend: "local".to_string(),
            storage_key: "blob_mig_1.blob".to_string(),
            caller_actor: "agent".to_string(),
            created_at: 1.0,
            expires_at: 9999999999.0,
        });
        wal.close();

        let sqlite_dst = mig_dir.join("omni_registry.sqlite");
        let report = omni_mcp::extension::aifx::migrate_to_v6_8(
            mig_dir.to_str().unwrap(),
            "sqlite",
            sqlite_dst.to_str().unwrap(),
            false,
        );
        assert!(
            report.ok,
            "migrate_to_v6_8 must succeed: {}",
            report.summary
        );
        let sq = omni_mcp::storage::SqliteRegistryBackend::try_open(sqlite_dst.to_str().unwrap())
            .unwrap();
        use omni_mcp::storage::RegistryStorageBackend;
        assert_eq!(sq.load_all_skills().len(), 1);
        assert_eq!(sq.load_all_mcp_tasks().len(), 1);
        assert_eq!(sq.load_all_oauth_flows().len(), 1);
        assert_eq!(sq.load_all_oauth_tokens().len(), 1);
        assert_eq!(sq.load_all_upstream_callbacks().len(), 1);
        assert_eq!(sq.load_all_file_blobs().len(), 1);
        let _ = std::fs::remove_dir_all(&mig_dir);
    }

    // Gap 7: FileBlobStore::purge_expired removes expired blobs
    let mut expired_blob = ctx
        .file_store
        .put_file(
            Some("expired_f1".to_string()),
            "old.txt",
            "text/plain",
            "agent",
            b"old",
        )
        .unwrap();
    expired_blob.expires_at = 1.0;
    ctx.registry.save_file_blob(&expired_blob);
    let purged = ctx.file_store.purge_expired();
    assert!(purged >= 1, "purge_expired must reap expired blobs");
    assert!(ctx.file_store.get_metadata("expired_f1").is_none());

    // Restore default config
    let mut reset = get_config();
    reset.enable_file_uploads_extension = false;
    set_config(reset);
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
