//! Authentication, RBAC, Origin/CORS protection, HS256 & OIDC/JWKS JWT verification, and CIMD metadata validation.

use super::http_codec::get_header;
use super::ProductionGatewayContext;
use crate::json::JsonValue;
#[cfg(feature = "tls-rustls")]
use crate::logging::now_epoch_secs;
use std::collections::HashMap;
#[cfg(feature = "tls-rustls")]
use std::fs;
#[cfg(feature = "tls-rustls")]
use std::sync::{OnceLock, RwLock};
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AuthRole {
    Agent = 1,
    Operator = 2,
    Approver = 3,
    Admin = 4,
}

impl AuthRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthRole::Agent => "agent",
            AuthRole::Operator => "operator",
            AuthRole::Approver => "approver",
            AuthRole::Admin => "admin",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "agent" => Some(AuthRole::Agent),
            "operator" => Some(AuthRole::Operator),
            "approver" => Some(AuthRole::Approver),
            "admin" => Some(AuthRole::Admin),
            _ => None,
        }
    }

    pub fn can_satisfy(&self, required: AuthRole) -> bool {
        matches!(
            (self, required),
            (AuthRole::Admin, _)
                | (AuthRole::Approver, AuthRole::Approver)
                | (AuthRole::Approver, AuthRole::Agent)
                | (AuthRole::Operator, AuthRole::Operator)
                | (AuthRole::Operator, AuthRole::Agent)
                | (AuthRole::Agent, AuthRole::Agent)
        )
    }
}

#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub id: String,
    pub role: AuthRole,
}

pub(crate) fn is_local_origin(origin: &str) -> bool {
    let rest = if let Some(r) = origin.strip_prefix("http://") {
        r
    } else if let Some(r) = origin.strip_prefix("https://") {
        r
    } else {
        return false;
    };
    let host_part = rest.split('/').next().unwrap_or(rest);
    let host_part = host_part.split('?').next().unwrap_or(host_part);
    let host_part = host_part.split('#').next().unwrap_or(host_part);
    let hostname = if host_part.starts_with('[') {
        if let Some(end_bracket) = host_part.find(']') {
            &host_part[..=end_bracket]
        } else {
            return false;
        }
    } else {
        host_part.split(':').next().unwrap_or(host_part)
    };
    hostname.eq_ignore_ascii_case("localhost") || hostname == "127.0.0.1" || hostname == "[::1]"
}

pub(crate) fn is_origin_allowed(origin: &str, cors_config: &str, host: Option<&str>) -> bool {
    if origin == "null" || origin.trim().is_empty() {
        return false;
    }

    let is_local_orig = is_local_origin(origin);

    if let Some(h) = host {
        let h_lower = h.to_lowercase();
        let h_name = h_lower.split(':').next().unwrap_or(&h_lower);
        let host_is_local = h_name == "localhost" || h_name == "127.0.0.1" || h_name == "[::1]";

        if host_is_local {
            if is_local_orig {
                return true;
            }
            if cors_config != "*" && !cors_config.trim().is_empty() {
                let allowed: Vec<&str> = cors_config.split(',').map(|s| s.trim()).collect();
                if allowed.contains(&origin) {
                    return true;
                }
            }
            return false;
        }
    }

    if cors_config != "*" && !cors_config.trim().is_empty() {
        let allowed: Vec<&str> = cors_config.split(',').map(|s| s.trim()).collect();
        if allowed.contains(&origin) {
            return true;
        }
    }

    if is_local_orig {
        return true;
    }

    cors_config == "*"
}

impl ProductionGatewayContext {
    pub fn authenticate(&self, headers: &HashMap<String, String>) -> Result<AuthPrincipal, u16> {
        let auth_hdr = get_header(headers, "authorization").unwrap_or("");
        let token = auth_hdr
            .strip_prefix("Bearer ")
            .or_else(|| auth_hdr.strip_prefix("bearer "))
            .map(|s| s.trim())
            .unwrap_or("");

        if token.is_empty() || token == "anonymous" {
            return Err(401);
        }

        // 0. If token is a 3-part JWT, check OIDC JWKS (RS256/ES256/EdDSA) or HS256 (OMNI_JWT_SECRET)
        if token.matches('.').count() == 2 {
            if let Ok(jwks_uri) = std::env::var("OMNI_OIDC_JWKS_URI") {
                if !jwks_uri.is_empty() {
                    let expected_iss = std::env::var("OMNI_OIDC_ISSUER")
                        .or_else(|_| std::env::var("OMNI_JWT_ISSUER"))
                        .ok();
                    let expected_aud = std::env::var("OMNI_OIDC_AUDIENCE")
                        .or_else(|_| std::env::var("OMNI_JWT_AUDIENCE"))
                        .ok();
                    return validate_jwt_oidc_jwks(
                        token,
                        &jwks_uri,
                        expected_iss.as_deref(),
                        expected_aud.as_deref(),
                    );
                }
            }
            if let Ok(jwt_secret) = std::env::var("OMNI_JWT_SECRET") {
                if !jwt_secret.is_empty() {
                    let expected_iss = std::env::var("OMNI_JWT_ISSUER").ok();
                    let expected_aud = std::env::var("OMNI_JWT_AUDIENCE").ok();
                    return validate_jwt_hs256(
                        token,
                        jwt_secret.as_bytes(),
                        expected_iss.as_deref(),
                        expected_aud.as_deref(),
                    );
                }
            }
        }

        // 1. If auth_secret is configured, strictly validate against it using constant-time comparison
        if !self.config.auth_secret.is_empty() {
            let mut matched = None;
            for entry in self.config.auth_secret.split(',') {
                let entry = entry.trim();
                if entry.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = entry.split(':').collect();
                if parts.len() >= 2 {
                    if let Some(role) = AuthRole::from_str(parts[0]) {
                        let secret = parts[1];
                        if crate::models::constant_time_eq(token.as_bytes(), secret.as_bytes()) {
                            let id = if parts.len() >= 3 {
                                parts[2].to_string()
                            } else {
                                format!("{}_user", parts[0])
                            };
                            matched = Some(AuthPrincipal { id, role });
                            break;
                        }
                    } else if crate::models::constant_time_eq(token.as_bytes(), entry.as_bytes()) {
                        matched = Some(AuthPrincipal {
                            id: "admin".to_string(),
                            role: AuthRole::Admin,
                        });
                        break;
                    }
                } else if crate::models::constant_time_eq(token.as_bytes(), entry.as_bytes()) {
                    matched = Some(AuthPrincipal {
                        id: "admin".to_string(),
                        role: AuthRole::Admin,
                    });
                    break;
                }
            }

            if let Some(p) = matched {
                return Ok(p);
            } else {
                return Err(401);
            }
        }

        // 2. Dev/Test mode (only on loopback and non-production, with explicit dev token allowlist Ã¢â‚¬â€ never substring inference)
        let is_loopback = matches!(
            self.config.host.as_str(),
            "127.0.0.1" | "localhost" | "::1" | "[::1]"
        );
        let is_prod = std::env::var("OMNI_ENV")
            .map(|v| v.eq_ignore_ascii_case("production"))
            .unwrap_or(false);
        if !is_loopback || is_prod {
            return Err(401);
        }

        let role = match token {
            "admin_dev_token" => AuthRole::Admin,
            "approver_dev_token" => AuthRole::Approver,
            "operator_dev_token"
            | "test_operator_token"
            | "operator_key"
            | "operator_secret_token" => AuthRole::Operator,
            "agent_dev_token" | "agent_token" => AuthRole::Agent,
            _ => return Err(401),
        };

        Ok(AuthPrincipal {
            id: token.to_string(),
            role,
        })
    }

    pub fn is_authorized(&self, headers: &HashMap<String, String>) -> bool {
        self.authenticate(headers).is_ok()
    }

    pub(crate) fn forbidden_response(
        &self,
        endpoint: &str,
        required: AuthRole,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            403,
            "application/json".to_string(),
            vec![(
                "WWW-Authenticate".to_string(),
                "Bearer error=\"insufficient_scope\"".to_string(),
            )],
            json!({
                "error": format!("Forbidden: Insufficient role permissions for {}. Required role: {:?}", endpoint, required)
            })
            .to_string_compact()
            .into_bytes(),
        )
    }

    pub(crate) fn unauthorized_response(
        &self,
        endpoint: &str,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            401,
            "application/json".to_string(),
            vec![(
                "WWW-Authenticate".to_string(),
                "Bearer resource_metadata=\"/.well-known/oauth-protected-resource\"".to_string(),
            )],
            json!({
                "error": format!("Unauthorized: Valid Bearer token required for {}", endpoint)
            })
            .to_string_compact()
            .into_bytes(),
        )
    }
}

fn base64url_decode(input: &str) -> Result<Vec<u8>, u16> {
    let mut out = Vec::with_capacity((input.len() * 3) / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for b in input.bytes() {
        if b == b'=' {
            break;
        }
        let val = match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err(401),
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

pub fn validate_jwt_hs256(
    token: &str,
    secret: &[u8],
    expected_iss: Option<&str>,
    expected_aud: Option<&str>,
) -> Result<AuthPrincipal, u16> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(401);
    }
    let header_bytes = base64url_decode(parts[0])?;
    let payload_bytes = base64url_decode(parts[1])?;
    let sig_bytes = base64url_decode(parts[2])?;

    let header_str = std::str::from_utf8(&header_bytes).map_err(|_| 401u16)?;
    let header_json = JsonValue::parse(header_str).map_err(|_| 401u16)?;
    let alg = header_json
        .get("alg")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if alg != "HS256" {
        return Err(401);
    }

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected_sig = crate::storage::audit_log::hmac_sha256(secret, signing_input.as_bytes());
    if !crate::models::constant_time_eq(&sig_bytes, &expected_sig) {
        return Err(401);
    }

    let payload_str = std::str::from_utf8(&payload_bytes).map_err(|_| 401u16)?;
    let claims = JsonValue::parse(payload_str).map_err(|_| 401u16)?;
    let now = crate::logging::now_epoch_secs();

    if let Some(exp) = claims.get("exp").and_then(|x| x.as_f64()) {
        if now >= exp {
            return Err(401);
        }
    }
    if let Some(nbf) = claims.get("nbf").and_then(|x| x.as_f64()) {
        if now < nbf {
            return Err(401);
        }
    }
    if let Some(req_iss) = expected_iss {
        let iss = claims.get("iss").and_then(|x| x.as_str()).unwrap_or("");
        if iss != req_iss {
            return Err(401);
        }
    }
    if let Some(req_aud) = expected_aud {
        let aud_ok = match claims.get("aud") {
            Some(v) if v.as_str() == Some(req_aud) => true,
            Some(v) => v
                .as_array()
                .map(|arr| arr.iter().any(|x| x.as_str() == Some(req_aud)))
                .unwrap_or(false),
            None => false,
        };
        if !aud_ok {
            return Err(401);
        }
    }

    let sub = claims
        .get("sub")
        .and_then(|x| x.as_str())
        .unwrap_or("jwt_user")
        .to_string();
    let role_str = claims
        .get("role")
        .and_then(|x| x.as_str())
        .or_else(|| claims.get("scope").and_then(|x| x.as_str()))
        .unwrap_or("agent");
    let role = if role_str.contains("admin") {
        AuthRole::Admin
    } else if role_str.contains("approver") {
        AuthRole::Approver
    } else if role_str.contains("operator") {
        AuthRole::Operator
    } else {
        AuthRole::Agent
    };

    Ok(AuthPrincipal { id: sub, role })
}

/// Cached JWKS entry: `(fetched_at_epoch_secs, parsed_jwks_json)`.
#[cfg(feature = "tls-rustls")]
static JWKS_CACHE: OnceLock<RwLock<HashMap<String, (f64, JsonValue)>>> = OnceLock::new();
#[cfg(feature = "tls-rustls")]
const JWKS_CACHE_TTL_SECS: f64 = 300.0;

#[cfg(feature = "tls-rustls")]
fn fetch_jwks_cached(jwks_uri: &str, force_refresh: bool) -> Result<JsonValue, u16> {
    if jwks_uri.starts_with('{') {
        return JsonValue::parse(jwks_uri).map_err(|_| 401u16);
    }
    let cache = JWKS_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    let now = now_epoch_secs();
    if !force_refresh {
        if let Ok(guard) = cache.read() {
            if let Some((fetched_at, doc)) = guard.get(jwks_uri) {
                if (now - *fetched_at) < JWKS_CACHE_TTL_SECS {
                    return Ok(doc.clone());
                }
            }
        }
    }
    let jwks_raw = if let Some(path) = jwks_uri.strip_prefix("file://") {
        fs::read_to_string(path).map_err(|_| 401u16)?
    } else {
        let client = crate::data_plane::HttpDownstreamClient::new();
        client.get_raw(jwks_uri, None).map_err(|_| 401u16)?
    };
    let doc = JsonValue::parse(&jwks_raw).map_err(|_| 401u16)?;
    if let Ok(mut guard) = cache.write() {
        guard.insert(jwks_uri.to_string(), (now, doc.clone()));
    }
    Ok(doc)
}

#[cfg(feature = "tls-rustls")]
fn pad_be_32(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() > 32 {
        return None;
    }
    let mut out = [0u8; 32];
    let offset = 32 - bytes.len();
    out[offset..].copy_from_slice(bytes);
    Some(out)
}

#[cfg(feature = "tls-rustls")]
fn verify_jwk_signature(
    alg: &str,
    jwk: &JsonValue,
    signing_input: &[u8],
    sig_bytes: &[u8],
) -> bool {
    match alg {
        "RS256" => {
            if jwk.get("kty").and_then(|v| v.as_str()) != Some("RSA") {
                return false;
            }
            let n = match jwk.get("n").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) if !b.is_empty() => b,
                _ => return false,
            };
            let e = match jwk.get("e").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) if !b.is_empty() => b,
                _ => return false,
            };
            let key = ring::signature::RsaPublicKeyComponents { n: &n, e: &e };
            key.verify(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                signing_input,
                sig_bytes,
            )
            .is_ok()
        }
        "ES256" => {
            if jwk.get("kty").and_then(|v| v.as_str()) != Some("EC")
                || jwk.get("crv").and_then(|v| v.as_str()) != Some("P-256")
            {
                return false;
            }
            let x = match jwk.get("x").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) => match pad_be_32(&b) {
                    Some(arr) => arr,
                    None => return false,
                },
                _ => return false,
            };
            let y = match jwk.get("y").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) => match pad_be_32(&b) {
                    Some(arr) => arr,
                    None => return false,
                },
                _ => return false,
            };
            let mut uncompressed = [0u8; 65];
            uncompressed[0] = 0x04;
            uncompressed[1..33].copy_from_slice(&x);
            uncompressed[33..65].copy_from_slice(&y);
            ring::signature::UnparsedPublicKey::new(
                &ring::signature::ECDSA_P256_SHA256_FIXED,
                &uncompressed,
            )
            .verify(signing_input, sig_bytes)
            .is_ok()
        }
        "EdDSA" => {
            if jwk.get("kty").and_then(|v| v.as_str()) != Some("OKP")
                || jwk.get("crv").and_then(|v| v.as_str()) != Some("Ed25519")
            {
                return false;
            }
            let x = match jwk.get("x").and_then(|v| v.as_str()).map(base64url_decode) {
                Some(Ok(b)) if b.len() == 32 => b,
                _ => return false,
            };
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &x)
                .verify(signing_input, sig_bytes)
                .is_ok()
        }
        _ => false,
    }
}

#[cfg(feature = "tls-rustls")]
fn extract_role_from_claims(claims: &JsonValue) -> AuthRole {
    let mut tokens: Vec<String> = Vec::new();
    if let Some(role_s) = claims.get("role").and_then(|x| x.as_str()) {
        tokens.extend(role_s.split_whitespace().map(|s| s.to_lowercase()));
    }
    if let Some(roles_arr) = claims.get("roles").and_then(|x| x.as_array()) {
        for r in roles_arr {
            if let Some(s) = r.as_str() {
                tokens.push(s.to_lowercase());
            }
        }
    } else if let Some(roles_s) = claims.get("roles").and_then(|x| x.as_str()) {
        tokens.extend(roles_s.split_whitespace().map(|s| s.to_lowercase()));
    }
    if let Some(scope_s) = claims.get("scope").and_then(|x| x.as_str()) {
        tokens.extend(scope_s.split_whitespace().map(|s| s.to_lowercase()));
    }
    if let Some(scp_arr) = claims.get("scp").and_then(|x| x.as_array()) {
        for r in scp_arr {
            if let Some(s) = r.as_str() {
                tokens.push(s.to_lowercase());
            }
        }
    } else if let Some(scp_s) = claims.get("scp").and_then(|x| x.as_str()) {
        tokens.extend(scp_s.split_whitespace().map(|s| s.to_lowercase()));
    }

    let has = |needle: &str| {
        tokens
            .iter()
            .any(|t| t == needle || t.ends_with(&format!(":{}", needle)))
    };
    if has("admin") {
        AuthRole::Admin
    } else if has("approver") {
        AuthRole::Approver
    } else if has("operator") {
        AuthRole::Operator
    } else {
        AuthRole::Agent
    }
}

/// Validates a JWT against a remote/local OIDC JWKS document (`RS256` / `ES256` / `EdDSA`),
/// enforcing cryptographic signature verification (`ring::signature`), `kid` matching,
/// `exp`/`nbf`, `iss`, and RFC 8707 `aud` resource indicator.
pub fn validate_jwt_oidc_jwks(
    token: &str,
    jwks_uri: &str,
    expected_iss: Option<&str>,
    expected_aud: Option<&str>,
) -> Result<AuthPrincipal, u16> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(401);
    }
    let header_bytes = base64url_decode(parts[0])?;
    let header_json =
        JsonValue::parse(&String::from_utf8_lossy(&header_bytes)).map_err(|_| 401u16)?;
    let alg = header_json
        .get("alg")
        .and_then(|a| a.as_str())
        .unwrap_or("");
    if !matches!(alg, "RS256" | "ES256" | "EdDSA") {
        return Err(401);
    }
    let kid = header_json.get("kid").and_then(|k| k.as_str());
    let sig_bytes = base64url_decode(parts[2])?;
    if sig_bytes.is_empty() {
        return Err(401);
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);

    #[cfg(not(feature = "tls-rustls"))]
    {
        let _ = (
            jwks_uri,
            expected_iss,
            expected_aud,
            kid,
            signing_input,
            parts,
        );
        Err(401)
    }

    #[cfg(feature = "tls-rustls")]
    {
        let mut verified = false;
        for attempt in 0..2 {
            let jwks = fetch_jwks_cached(jwks_uri, attempt == 1)?;
            let keys = jwks.get("keys").and_then(|k| k.as_array()).ok_or(401u16)?;
            for k in keys {
                let k_kid = k.get("kid").and_then(|x| x.as_str());
                let k_alg = k.get("alg").and_then(|x| x.as_str());
                let kid_ok = match (kid, k_kid) {
                    (Some(a), Some(b)) => a == b,
                    (None, _) => true,
                    _ => false,
                };
                let alg_ok = k_alg.map(|a| a == alg).unwrap_or(true);
                if kid_ok
                    && alg_ok
                    && verify_jwk_signature(alg, k, signing_input.as_bytes(), &sig_bytes)
                {
                    verified = true;
                    break;
                }
            }
            if verified || jwks_uri.starts_with('{') {
                break;
            }
        }
        if !verified {
            return Err(401);
        }

        let claims_bytes = base64url_decode(parts[1])?;
        let claims =
            JsonValue::parse(&String::from_utf8_lossy(&claims_bytes)).map_err(|_| 401u16)?;
        let now = now_epoch_secs();
        if let Some(exp) = claims.get("exp").and_then(|x| x.as_f64()) {
            if now > exp {
                return Err(401);
            }
        }
        if let Some(nbf) = claims.get("nbf").and_then(|x| x.as_f64()) {
            if now < nbf {
                return Err(401);
            }
        }
        if let Some(iss_req) = expected_iss {
            if claims.get("iss").and_then(|x| x.as_str()) != Some(iss_req) {
                return Err(401);
            }
        }
        if let Some(aud_req) = expected_aud {
            let aud_ok = match claims.get("aud") {
                Some(JsonValue::Str(s)) => s == aud_req,
                Some(JsonValue::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(aud_req)),
                _ => false,
            };
            if !aud_ok {
                return Err(401);
            }
        }

        let sub = claims
            .get("sub")
            .and_then(|x| x.as_str())
            .unwrap_or("oidc_user")
            .to_string();
        let role = extract_role_from_claims(&claims);
        Ok(AuthPrincipal { id: sub, role })
    }
}

/// Validates a Client ID Metadata Document (CIMD) per MCP OAuth 2.1 draft.
pub fn validate_cimd_metadata(doc: &JsonValue, expected_client_id: &str) -> Result<(), String> {
    let client_id = doc
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "CIMD document missing 'client_id'".to_string())?;
    if client_id != expected_client_id {
        return Err(format!(
            "CIMD 'client_id' mismatch: expected '{}', got '{}'",
            expected_client_id, client_id
        ));
    }
    let redirect_uris = doc
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "CIMD document missing 'redirect_uris' array".to_string())?;
    if redirect_uris.is_empty() {
        return Err("CIMD 'redirect_uris' must not be empty".to_string());
    }
    for u in redirect_uris {
        let s = u
            .as_str()
            .ok_or_else(|| "CIMD 'redirect_uris' must contain strings".to_string())?;
        if !s.contains("://") {
            return Err(format!("Invalid CIMD redirect_uri '{}'", s));
        }
    }
    Ok(())
}

/// Fetches and validates a Client ID Metadata Document (CIMD) from an HTTPS/HTTP client_id URL.
pub fn fetch_cimd_metadata(client_id_url: &str) -> Result<JsonValue, String> {
    if !client_id_url.starts_with("https://") && !client_id_url.starts_with("http://") {
        return Err("CIMD client_id must be an http(s) URL".to_string());
    }
    let client = crate::data_plane::HttpDownstreamClient::new();
    let raw = client.get_raw(client_id_url, None)?;
    let doc = JsonValue::parse(&raw)?;
    validate_cimd_metadata(&doc, client_id_url)?;
    Ok(doc)
}
