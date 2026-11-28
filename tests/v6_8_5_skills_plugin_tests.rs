#![forbid(unsafe_code)]
//! Integration tests for OmniMCP v6.8.5:
//! Universal CLI/Coding-Agent Skills Suite, Enterprise Plugin Distribution,
//! Tool Group Stdio Isolation, Enterprise MCP Auth Enforcement, and OIDC Bind Validation.

use omni_mcp::config::OmniConfig;
use omni_mcp::control_plane::{MCPRegistry, SkillAccessScope, SkillEngine};
use omni_mcp::extension::plugin::{
    builtin_skill_records, export_enterprise_bundle, init_plugin, install_skills,
    seed_builtin_skills_into_registry, verify_plugin_bundle, EnterpriseExportOptions,
    PluginInitOptions, SkillInstallOptions, BUILTIN_SKILLS,
};
use omni_mcp::json::JsonValue;
use omni_mcp::models::{
    parse_skill_frontmatter, ProtocolType, ServerRecord, ToolGroupEntry, ToolGroupRecord,
    ToolRecord, ToolStatus,
};
use omni_mcp::server::ProductionGatewayContext;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn make_temp_dir(prefix: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "omni_v685_{}_{}_{}",
        prefix,
        std::process::id(),
        omni_mcp::models::generate_ulid()
    ));
    fs::create_dir_all(&p).expect("create temp dir");
    p
}

#[test]
fn test_1_builtin_skills_validate_deterministically_and_frontmatter_hardening() {
    assert_eq!(BUILTIN_SKILLS.len(), 4);
    let first = builtin_skill_records(None).expect("first builtin_skill_records");
    let second = builtin_skill_records(None).expect("second builtin_skill_records");
    assert_eq!(first.len(), 4);

    let slugs: Vec<&str> = first.iter().map(|s| s.slug.as_str()).collect();
    assert!(slugs.contains(&"omni-mcp"));
    assert!(slugs.contains(&"omni-codemode"));
    assert!(slugs.contains(&"omni-cli"));
    assert!(slugs.contains(&"omni-enterprise"));

    for (a, b) in first.iter().zip(second.iter()) {
        assert!(!a.manifest_sha256.is_empty());
        assert_eq!(a.manifest_sha256, b.manifest_sha256);
        assert_eq!(a.version, b.version);
    }

    // Multi-line folded (`>`) scalar + indented nested `name:` / `description:` must NOT overwrite top-level
    let folded_md = r#"---
name: top-level-skill
metadata:
  name: nested-should-be-ignored
  description: nested-desc-ignored
description: >
  Line one of description
  and line two folded cleanly.
---
# Body
"#;
    let (name, desc) = parse_skill_frontmatter(folded_md).expect("folded YAML frontmatter");
    assert_eq!(name, "top-level-skill");
    assert_eq!(desc, "Line one of description and line two folded cleanly.");

    // Multi-line literal (`|`) scalar
    let literal_md =
        "---\nname: literal-skill\ndescription: |\n  First line\n  Second line\n---\n# Body\n";
    let (lname, ldesc) = parse_skill_frontmatter(literal_md).expect("literal YAML frontmatter");
    assert_eq!(lname, "literal-skill");
    assert_eq!(ldesc, "First line\nSecond line");
}

#[test]
fn test_2_cli_dispatch_and_offline_zero_side_effects() {
    let bin = env!("CARGO_BIN_EXE_omni-gateway");
    let isolated_data = make_temp_dir("offline_data_probe");
    // Remove directory so we can prove offline commands do NOT create it
    fs::remove_dir_all(&isolated_data).expect("remove probe dir");
    let install_dir = make_temp_dir("skills_install_all");

    // 1. `omni-gateway skills install --agent all --dir <tmp>` runs offline with zero data_dir creation
    let out = Command::new(bin)
        .args([
            "skills",
            "install",
            "--agent",
            "all",
            "--scope",
            "project",
            "--dir",
            install_dir.to_str().unwrap(),
        ])
        .env("OMNI_DATA_DIR", &isolated_data)
        .output()
        .expect("run omni-gateway skills install");
    assert!(
        out.status.success(),
        "skills install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !isolated_data.exists(),
        "Offline `skills install` must NOT create OMNI_DATA_DIR"
    );
    assert!(install_dir
        .join(".claude/skills/omni-mcp/SKILL.md")
        .exists());
    assert!(install_dir
        .join(".agent/skills/omni-codemode/SKILL.md")
        .exists());
    assert!(install_dir.join(".codex/skills/omni-cli/SKILL.md").exists());
    assert!(install_dir
        .join(".agents/skills/omni-enterprise/SKILL.md")
        .exists());

    // 2. `omni-gateway plugin agents` runs offline with zero data_dir creation
    let agents_out = Command::new(bin)
        .args(["plugin", "agents"])
        .env("OMNI_DATA_DIR", &isolated_data)
        .output()
        .expect("run omni-gateway plugin agents");
    assert!(agents_out.status.success());
    assert!(
        !isolated_data.exists(),
        "Offline `plugin agents` must NOT create OMNI_DATA_DIR"
    );
    let agents_json =
        JsonValue::parse(&String::from_utf8_lossy(&agents_out.stdout)).expect("valid agents JSON");
    assert!(agents_json.as_array().is_some_and(|a| a.len() >= 10));

    // 3. `omni-gateway skills list` dispatches to CLI (does not hang starting HTTP server)
    let list_data = make_temp_dir("skills_list_data");
    let list_out = Command::new(bin)
        .args(["skills", "list"])
        .env("OMNI_DATA_DIR", &list_data)
        .output()
        .expect("run omni-gateway skills list");
    assert!(
        list_out.status.success(),
        "skills list failed: {}",
        String::from_utf8_lossy(&list_out.stderr)
    );
    let list_json =
        JsonValue::parse(&String::from_utf8_lossy(&list_out.stdout)).expect("valid skills JSON");
    assert!(list_json.is_array());

    let _ = fs::remove_dir_all(&install_dir);
    let _ = fs::remove_dir_all(&list_data);
}

#[test]
fn test_3_skills_bootstrap_and_codemode_playbook_syntax() {
    let tmp = make_temp_dir("bootstrap_ctx");
    let mut cfg = OmniConfig::from_env();
    cfg.data_dir = tmp.to_string_lossy().into_owned();
    cfg.demo_seed = false;
    cfg.seed_builtin_skills = false;

    let ctx = ProductionGatewayContext::new(Some(cfg));

    // Register a sample service and Tool Group to check auto-synthesized playbook syntax
    let mut srv = ServerRecord::new("billing_srv", "Billing Service", "Billing APIs", "finance");
    srv.protocol = ProtocolType::Http;
    srv.status = ToolStatus::Enabled;
    let mut t = ToolRecord::new(
        "create_invoice",
        "Create an invoice",
        omni_mcp::json!({"type": "object", "properties": {"amount": {"type": "number"}}}),
    );
    t.status = ToolStatus::Enabled;
    srv.add_tool(t);
    ctx.registry.register_server(srv.clone());

    let grp = ToolGroupRecord {
        slug: "billing".to_string(),
        display_name: "Billing Group".to_string(),
        description: "Scoped billing tools".to_string(),
        entries: vec![ToolGroupEntry {
            server_name: "billing_srv".to_string(),
            tool_name: Some("create_invoice".to_string()),
        }],
        created_at: 0.0,
    };
    ctx.registry.save_tool_group(grp.clone());
    ctx.cache.refresh_now();

    // Auto-synthesized group & service playbooks must emit `codemode.<srv>.<tool>({})` (NOT `omni.`)
    let engine = SkillEngine::new(ctx.registry.clone());
    let grp_pb = engine.synthesize_group_playbook(&grp, &[srv.clone()]);
    let grp_examples = grp_pb
        .files
        .iter()
        .find(|f| f.path == "references/examples.ts")
        .expect("group examples.ts");
    assert!(
        grp_examples
            .content
            .contains("await codemode.billing_srv.create_invoice({})"),
        "Group playbook must use codemode.<srv>.<tool>: {}",
        grp_examples.content
    );
    assert!(!grp_examples.content.contains("await omni."));

    let srv_pb = engine.synthesize_service_playbook(&srv);
    let srv_examples = srv_pb
        .files
        .iter()
        .find(|f| f.path == "references/examples.ts")
        .expect("service examples.ts");
    assert!(
        srv_examples
            .content
            .contains("await codemode.billing_srv.create_invoice({})"),
        "Service playbook must use codemode.<srv>.<tool>: {}",
        srv_examples.content
    );
    assert!(!srv_examples.content.contains("await omni."));

    // Default registry should NOT have the 4 built-in skills until bootstrapped
    let before = engine.list_skills(&SkillAccessScope::default());
    assert!(!before.iter().any(|s| s.slug == "omni-mcp"));

    // Bootstrap built-in skills and verify MCP `skills/list` and `skills/get` serve them
    let seeded = seed_builtin_skills_into_registry(&ctx.registry, None).expect("bootstrap skills");
    assert_eq!(seeded.len(), 4);

    let list_req = omni_mcp::models::RPCRequest {
        jsonrpc: "2.0".to_string(),
        id: JsonValue::from(1),
        method: "skills/list".to_string(),
        params: JsonValue::object(),
    };
    let list_resp = ctx.omni_mcp.handle_request(&list_req, "agent", None);
    let skills_arr = list_resp
        .result
        .as_ref()
        .and_then(|r| r.get("skills"))
        .and_then(|s| s.as_array())
        .expect("skills/list result array");
    assert!(skills_arr
        .iter()
        .any(|s| s["slug"].as_str() == Some("omni-mcp")));
    assert!(skills_arr
        .iter()
        .any(|s| s["slug"].as_str() == Some("omni-enterprise")));

    let get_req = omni_mcp::models::RPCRequest {
        jsonrpc: "2.0".to_string(),
        id: JsonValue::from(2),
        method: "skills/get".to_string(),
        params: omni_mcp::json!({"slug": "omni-codemode"}),
    };
    let get_resp = ctx.omni_mcp.handle_request(&get_req, "agent", None);
    assert!(get_resp.error.is_none());
    assert_eq!(
        get_resp.result.as_ref().and_then(|r| r["slug"].as_str()),
        Some("omni-codemode")
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_4_enterprise_plugin_init_export_guardrails_and_tamper_verification() {
    let out_dir = make_temp_dir("ent_plugin_init");

    // 1. Enterprise init rejects insecure http:// unless --allow-insecure-http
    let bad_http = init_plugin(&PluginInitOptions {
        mode: "enterprise".to_string(),
        agent: "claude".to_string(),
        gateway_url: Some("http://mcp.corp.example.com".to_string()),
        group_slug: Some("billing".to_string()),
        auth_env: "OMNI_AUTH_TOKEN".to_string(),
        transport: None,
        out_dir: out_dir.clone(),
        allow_insecure_http: false,
        dry_run: true,
    });
    assert!(bad_http.is_err(), "Enterprise mode must reject http://");

    // 2. Enterprise init rejects plaintext secret in --auth-env
    let bad_secret = init_plugin(&PluginInitOptions {
        mode: "enterprise".to_string(),
        agent: "claude".to_string(),
        gateway_url: Some("https://mcp.corp.example.com".to_string()),
        group_slug: Some("billing".to_string()),
        auth_env: "sk-1234567890abcdef".to_string(),
        transport: None,
        out_dir: out_dir.clone(),
        allow_insecure_http: false,
        dry_run: true,
    });
    assert!(
        bad_secret.is_err(),
        "Enterprise mode must reject plaintext secrets in --auth-env"
    );

    // 3. Valid enterprise init writes enterprise-policy.json + MANIFEST.sha256 + /groups/billing/mcp
    let report = init_plugin(&PluginInitOptions {
        mode: "enterprise".to_string(),
        agent: "claude".to_string(),
        gateway_url: Some("https://mcp.corp.example.com".to_string()),
        group_slug: Some("billing".to_string()),
        auth_env: "OMNI_AUTH_TOKEN".to_string(),
        transport: None,
        out_dir: out_dir.clone(),
        allow_insecure_http: false,
        dry_run: false,
    })
    .expect("valid enterprise init");
    assert!(report.manifest_sha256.is_some());
    assert!(out_dir.join("enterprise-policy.json").exists());
    assert!(out_dir.join("MANIFEST.sha256").exists());
    let mcp_cfg = fs::read_to_string(out_dir.join(".mcp.json")).expect("read .mcp.json");
    assert!(mcp_cfg.contains("https://mcp.corp.example.com/groups/billing/mcp"));

    // 4. Verify passes on intact bundle
    let verify_ok = verify_plugin_bundle(&out_dir).expect("verify intact bundle");
    assert!(
        verify_ok.valid,
        "Expected valid bundle: {:?}",
        verify_ok.errors
    );
    assert_eq!(verify_ok.validated_skills.len(), 4);
    assert_eq!(
        verify_ok.verified_checksum_file.as_deref(),
        Some("MANIFEST.sha256")
    );

    // 5. Tampering a file causes verify to fail
    fs::write(out_dir.join("AGENTS.md"), "# tampered\n").expect("tamper AGENTS.md");
    let verify_tampered = verify_plugin_bundle(&out_dir).expect("verify tampered bundle");
    assert!(!verify_tampered.valid);
    assert!(verify_tampered
        .errors
        .iter()
        .any(|e| e.contains("SHA-256 mismatch")));

    // 6. Enterprise export with registry skills + secret leak detection
    let export_dir = make_temp_dir("ent_export");
    let reg = MCPRegistry::new(None);
    let mut custom_skill = builtin_skill_records(None).unwrap().remove(0);
    custom_skill.slug = "custom-registry-playbook".to_string();
    reg.save_skill(custom_skill.clone()).unwrap();

    let exp_report = export_enterprise_bundle(
        &EnterpriseExportOptions {
            gateway_url: "https://mcp.corp.example.com".to_string(),
            group_slug: Some("billing".to_string()),
            auth_env: "OMNI_AUTH_TOKEN".to_string(),
            out_dir: export_dir.clone(),
            allow_insecure_http: false,
            include_registry_skills: true,
            dry_run: false,
        },
        &[custom_skill],
    )
    .expect("export enterprise bundle");
    assert!(exp_report
        .skills_exported
        .contains(&"custom-registry-playbook".to_string()));
    assert!(export_dir.join("SHA256SUMS").exists());
    let exp_verify = verify_plugin_bundle(&export_dir).expect("verify export bundle");
    assert!(
        exp_verify.valid,
        "Export bundle invalid: {:?}",
        exp_verify.errors
    );

    // Inject a plaintext secret leak and confirm verify catches it
    fs::write(
        export_dir.join("leak.txt"),
        "Authorization: Bearer super_secret_literal_token_99999\n",
    )
    .expect("write leak file");
    let leak_verify = verify_plugin_bundle(&export_dir).expect("verify leaked bundle");
    assert!(!leak_verify.valid);
    assert!(leak_verify
        .errors
        .iter()
        .any(|e| e.contains("Plaintext secret leak")));

    // Dry-run install_skills leaves directory untouched
    let dry_dir = make_temp_dir("dry_install");
    let dry_rep = install_skills(&SkillInstallOptions {
        agent: "claude".to_string(),
        scope: "project".to_string(),
        dir: Some(dry_dir.clone()),
        only: vec!["omni-mcp".to_string()],
        dry_run: true,
        force: false,
    })
    .expect("dry-run install");
    assert!(dry_rep.dry_run);
    assert!(!dry_dir.join(".claude").exists());

    let _ = fs::remove_dir_all(&out_dir);
    let _ = fs::remove_dir_all(&export_dir);
    let _ = fs::remove_dir_all(&dry_dir);
}

#[test]
fn test_5_plugin_manifest_api_group_discovery_mcp_auth_and_oidc_bind() {
    // 1. validate_bind("0.0.0.0") succeeds with OIDC JWKS or JWT secret even without auth_secret
    let mut cfg_oidc = OmniConfig::from_env();
    cfg_oidc.auth_secret.clear();
    cfg_oidc.jwt_secret = None;
    cfg_oidc.oidc_jwks_uri = Some("https://idp.corp.example.com/.well-known/jwks.json".to_string());
    assert!(cfg_oidc.validate_bind("0.0.0.0").is_ok());

    let mut cfg_jwt = OmniConfig::from_env();
    cfg_jwt.auth_secret.clear();
    cfg_jwt.oidc_jwks_uri = None;
    cfg_jwt.jwt_secret = Some("hs256-super-secret".to_string());
    assert!(cfg_jwt.validate_bind("0.0.0.0").is_ok());

    let mut cfg_none = OmniConfig::from_env();
    cfg_none.auth_secret.clear();
    cfg_none.jwt_secret = None;
    cfg_none.oidc_jwks_uri = None;
    assert!(cfg_none.validate_bind("0.0.0.0").is_err());

    // 2. HTTP routes with OMNI_REQUIRE_MCP_AUTH=true
    let tmp = make_temp_dir("http_auth_ctx");
    let mut cfg = OmniConfig::from_env();
    cfg.data_dir = tmp.to_string_lossy().into_owned();
    cfg.demo_seed = false;
    cfg.auth_secret =
        "agent:agent_secret_123:agent_one,admin:admin_secret_456:admin_one".to_string();
    cfg.require_mcp_auth = true;
    let ctx = ProductionGatewayContext::new(Some(cfg));

    let grp = ToolGroupRecord {
        slug: "billing".to_string(),
        display_name: "Billing".to_string(),
        description: "Billing tools".to_string(),
        entries: vec![],
        created_at: 0.0,
    };
    ctx.registry.save_tool_group(grp);

    let no_auth = HashMap::new();
    let mut agent_auth = HashMap::new();
    agent_auth.insert(
        "authorization".to_string(),
        "Bearer agent_secret_123".to_string(),
    );

    // GET /api/v1/plugin/manifest enforces AuthRole::Agent (401 without token, 200 with Agent token)
    let (st_unauth, _, _, _) = ctx.handle_http(
        "GET",
        "/api/v1/plugin/manifest?agent=codex&mode=enterprise&group=billing",
        &no_auth,
        b"",
    );
    assert_eq!(st_unauth, 401);

    let (st_ok, ct_ok, _, body_ok) = ctx.handle_http(
        "GET",
        "/api/v1/plugin/manifest?agent=codex&mode=enterprise&group=billing",
        &agent_auth,
        b"",
    );
    assert_eq!(st_ok, 200);
    assert!(ct_ok.contains("application/json"));
    let manifest = JsonValue::parse(&String::from_utf8_lossy(&body_ok)).expect("manifest JSON");
    assert_eq!(manifest["agent"].as_str(), Some("codex"));
    assert_eq!(manifest["mode"].as_str(), Some("enterprise"));
    assert!(manifest["endpoint"]
        .as_str()
        .unwrap_or("")
        .ends_with("/groups/billing/mcp"));

    // GET /groups/billing/mcp returns JSON discovery metadata (NOT HTML)
    let (g_st, g_ct, _, g_body) = ctx.handle_http("GET", "/groups/billing/mcp", &no_auth, b"");
    assert_eq!(g_st, 200);
    assert!(g_ct.contains("application/json"));
    let g_json = JsonValue::parse(&String::from_utf8_lossy(&g_body)).expect("group discovery JSON");
    assert_eq!(g_json["group"].as_str(), Some("billing"));
    assert!(g_json["capabilities"]["extensions"].is_object());

    // POST /omni/mcp and POST /groups/billing/mcp enforce 401 + RFC 9728 WWW-Authenticate when require_mcp_auth=true
    let rpc_body = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
    let (m_st, _, m_hdrs, _) = ctx.handle_http("POST", "/omni/mcp", &no_auth, rpc_body);
    assert_eq!(m_st, 401);
    assert!(m_hdrs.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("www-authenticate")
            && v.contains("/.well-known/oauth-protected-resource")
    }));

    let (gm_st, _, gm_hdrs, _) = ctx.handle_http("POST", "/groups/billing/mcp", &no_auth, rpc_body);
    assert_eq!(gm_st, 401);
    assert!(gm_hdrs.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("www-authenticate")
            && v.contains("/.well-known/oauth-protected-resource")
    }));

    // With valid Agent token, POST /groups/billing/mcp succeeds with 200
    let (gm_ok, _, _, _) = ctx.handle_http("POST", "/groups/billing/mcp", &agent_auth, rpc_body);
    assert_eq!(gm_ok, 200);

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_6_stdio_group_isolation_and_mutual_exclusion() {
    let bin = env!("CARGO_BIN_EXE_omni-gateway");

    // 1. Simultaneous --server and --group must exit with code 2
    let conflict = Command::new(bin)
        .args(["--stdio", "--server", "srv_a", "--group", "grp_a"])
        .output()
        .expect("run conflicting stdio args");
    assert_eq!(conflict.status.code(), Some(2));

    // 2. Seed a WAL data directory with two services (`alpha` and `beta`) and a Tool Group `alpha-only`
    let tmp = make_temp_dir("stdio_group_iso");
    {
        let mut cfg = OmniConfig::from_env();
        cfg.data_dir = tmp.to_string_lossy().into_owned();
        cfg.demo_seed = false;
        let ctx = ProductionGatewayContext::new(Some(cfg));

        let mut alpha = ServerRecord::new("alpha", "Alpha", "Alpha service", "team-a");
        alpha.protocol = ProtocolType::Http;
        alpha.status = ToolStatus::Enabled;
        let mut a_tool = ToolRecord::new(
            "ping_alpha",
            "Ping alpha",
            omni_mcp::json!({"type": "object"}),
        );
        a_tool.status = ToolStatus::Enabled;
        alpha.add_tool(a_tool);
        ctx.registry.register_server(alpha);

        let mut beta = ServerRecord::new("beta", "Beta", "Beta service", "team-b");
        beta.protocol = ProtocolType::Http;
        beta.status = ToolStatus::Enabled;
        let mut b_tool = ToolRecord::new(
            "secret_beta",
            "Secret beta",
            omni_mcp::json!({"type": "object"}),
        );
        b_tool.status = ToolStatus::Enabled;
        beta.add_tool(b_tool);
        ctx.registry.register_server(beta);

        ctx.registry.save_tool_group(ToolGroupRecord {
            slug: "alpha-only".to_string(),
            display_name: "Alpha Only".to_string(),
            description: "Strict isolation to alpha".to_string(),
            entries: vec![ToolGroupEntry {
                server_name: "alpha".to_string(),
                tool_name: None,
            }],
            created_at: 0.0,
        });
    }

    // 3. Spawn `omni-gateway --stdio --group alpha-only` and verify tools/list + tools/call isolation
    let mut child = Command::new(bin)
        .args(["--stdio", "--group", "alpha-only"])
        .env("OMNI_DATA_DIR", &tmp)
        .env("OMNI_MCP_DEMO_SEED", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stdio group server");

    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{}}}}"#
        )
        .unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"beta_secret_beta","arguments":{{}}}}}}"#
        )
        .unwrap();
    }

    let output = child.wait_with_output().expect("wait stdio child");
    assert!(
        output.status.success(),
        "stdio child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "Expected 2 JSON-RPC responses, got: {}",
        stdout_str
    );

    let resp1 = JsonValue::parse(lines[0]).expect("parse tools/list resp");
    let tools = resp1["result"]["tools"].as_array().expect("tools array");
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(tool_names.contains(&"alpha_ping_alpha"));
    assert!(
        !tool_names.contains(&"beta_secret_beta"),
        "beta tool leaked into alpha-only group: {:?}",
        tool_names
    );

    let resp2 = JsonValue::parse(lines[1]).expect("parse tools/call resp");
    assert_eq!(
        resp2["error"]["code"].as_i64(),
        Some(-32601),
        "Calling out-of-group tool must fail with -32601: {}",
        lines[1]
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_7_security_edge_cases_and_regression_guards() {
    use omni_mcp::extension::plugin::{build_plugin_manifest_json, detect_plaintext_secret};
    use omni_mcp::models::ExternalResourceRecord;

    // 1. Secret detector must scan all occurrences (not stop at first benign `sk-` in `task-id`)
    //    AND must not false-positive on hyphenated English words like `disk-snapshot`.
    assert!(
        detect_plaintext_secret("enable disk-snapshot and task-orchestration").is_none(),
        "Hyphenated words must not false-positive as sk- secrets"
    );
    assert!(
        detect_plaintext_secret("task-id=42\nOPENAI_API_KEY=sk-live1234567890abcdef").is_some(),
        "Real sk- secret after benign task-id must be detected"
    );

    // 2. Group slug validation blocks path traversal and TOML injection
    assert!(build_plugin_manifest_json(
        "codex",
        "enterprise",
        "https://mcp.corp.example.com",
        Some("../admin"),
        "OMNI_AUTH_TOKEN",
        None,
        false,
    )
    .is_err());
    assert!(build_plugin_manifest_json(
        "codex",
        "enterprise",
        "https://mcp.corp.example.com",
        Some("billing\"]\n[evil"),
        "OMNI_AUTH_TOKEN",
        None,
        false,
    )
    .is_err());

    // 3. Gemini agent init generates full Gemini CLI extension schema (name, version, contextFileName, mcpServers)
    let gemini_dir = make_temp_dir("gemini_ent_init");
    init_plugin(&PluginInitOptions {
        mode: "enterprise".to_string(),
        agent: "gemini".to_string(),
        gateway_url: Some("https://mcp.corp.example.com".to_string()),
        group_slug: Some("billing".to_string()),
        auth_env: "OMNI_AUTH_TOKEN".to_string(),
        transport: None,
        out_dir: gemini_dir.clone(),
        allow_insecure_http: false,
        dry_run: false,
    })
    .expect("gemini enterprise init");
    let gemini_raw = fs::read_to_string(gemini_dir.join("gemini-extension.json"))
        .expect("read gemini-extension.json");
    let gemini_json = JsonValue::parse(&gemini_raw).expect("parse gemini-extension.json");
    assert_eq!(gemini_json["name"].as_str(), Some("omni-mcp-enterprise"));
    assert_eq!(
        gemini_json["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(gemini_json["contextFileName"].as_str(), Some("AGENTS.md"));
    assert!(gemini_json["mcpServers"].is_object());

    // 4. Deleting MANIFEST.sha256 from an enterprise bundle must fail verify_plugin_bundle
    fs::remove_file(gemini_dir.join("MANIFEST.sha256")).expect("delete MANIFEST.sha256");
    let missing_cs = verify_plugin_bundle(&gemini_dir).expect("verify missing checksum");
    assert!(
        !missing_cs.valid,
        "Enterprise bundle without MANIFEST.sha256 must fail verification"
    );
    assert!(missing_cs
        .errors
        .iter()
        .any(|e| e.contains("MANIFEST.sha256 or SHA256SUMS")));

    // 5. Codex init must not point .claude-plugin/plugin.json mcpServers at a .toml file
    let codex_dir = make_temp_dir("codex_std_init");
    init_plugin(&PluginInitOptions {
        mode: "standard".to_string(),
        agent: "codex".to_string(),
        gateway_url: None,
        group_slug: None,
        auth_env: "OMNI_AUTH_TOKEN".to_string(),
        transport: None,
        out_dir: codex_dir.clone(),
        allow_insecure_http: false,
        dry_run: false,
    })
    .expect("codex standard init");
    let claude_json = JsonValue::parse(
        &fs::read_to_string(codex_dir.join(".claude-plugin/plugin.json"))
            .expect("read plugin.json"),
    )
    .expect("parse plugin.json");
    assert!(
        claude_json.get("mcpServers").is_none(),
        "Claude manifest must not reference .codex/config.toml"
    );

    // 6. Tool Group resources/list deduplicates resources when multiple tools belong to the same server
    let tmp = make_temp_dir("grp_res_dedup");
    let mut cfg = OmniConfig::from_env();
    cfg.data_dir = tmp.to_string_lossy().into_owned();
    cfg.demo_seed = false;
    let ctx = ProductionGatewayContext::new(Some(cfg));
    let mut srv = ServerRecord::new("docs_srv", "Docs", "Docs service", "eng");
    srv.protocol = ProtocolType::Http;
    srv.status = ToolStatus::Enabled;
    srv.external_resources.push(ExternalResourceRecord {
        uri: "guide://v1".to_string(),
        name: "Guide".to_string(),
        description: Some("Engineering guide".to_string()),
        mime_type: Some("text/markdown".to_string()),
    });
    ctx.registry.register_server(srv);
    let grp = ToolGroupRecord {
        slug: "docs-multi".to_string(),
        display_name: "Docs Multi".to_string(),
        description: "Two tools from same server".to_string(),
        entries: vec![
            ToolGroupEntry {
                server_name: "docs_srv".to_string(),
                tool_name: Some("read_doc".to_string()),
            },
            ToolGroupEntry {
                server_name: "docs_srv".to_string(),
                tool_name: Some("search_docs".to_string()),
            },
        ],
        created_at: 0.0,
    };
    ctx.registry.save_tool_group(grp.clone());
    ctx.cache.refresh_now();

    let res_req =
        omni_mcp::json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list", "params": {}});
    let res_resp = ctx
        .handle_group_rpc(&grp, &res_req, "agent", None, None)
        .expect("resources/list response");
    let resources = res_resp["result"]["resources"]
        .as_array()
        .expect("resources array");
    assert_eq!(
        resources.len(),
        1,
        "Duplicate server entries in a Tool Group must not duplicate resources/list"
    );

    let _ = fs::remove_dir_all(&gemini_dir);
    let _ = fs::remove_dir_all(&codex_dir);
    let _ = fs::remove_dir_all(&tmp);
}
