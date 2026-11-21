//! HTTP/1.1 framing helpers, URL decoding, JSON response encoding, static dashboard delivery, and inbound rustls TLS config builders.

use super::ProductionGatewayContext;
use crate::json::JsonValue;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
#[cfg(feature = "tls-rustls")]
use std::sync::Arc;
/// Pure zero-dependency URL percent-decode helper function.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = bytes[i + 1];
            let h2 = bytes[i + 2];
            let d1 = (h1 as char).to_digit(16);
            let d2 = (h2 as char).to_digit(16);
            if let (Some(v1), Some(v2)) = (d1, d2) {
                out.push(((v1 << 4) | v2) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn get_header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers.get(name).map(|s| s.as_str()).or_else(|| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    })
}

impl ProductionGatewayContext {
    pub(crate) fn json_response(
        &self,
        status: u16,
        data: JsonValue,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            status,
            "application/json".to_string(),
            Vec::new(),
            data.to_string_compact().into_bytes(),
        )
    }

    pub(crate) fn json_response_with_headers(
        &self,
        status: u16,
        data: JsonValue,
        headers: Vec<(String, String)>,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        (
            status,
            "application/json".to_string(),
            headers,
            data.to_string_compact().into_bytes(),
        )
    }

    pub(crate) fn serve_static_or_index(
        &self,
        path: &str,
    ) -> (u16, String, Vec<(String, String)>, Vec<u8>) {
        let ui_dist = Path::new("ui").join("dist");
        if ui_dist.exists() {
            let rel = path.trim_start_matches('/');
            if !rel.is_empty() && !rel.contains("..") {
                let candidate = ui_dist.join(rel);
                if candidate.is_file() {
                    if let Ok(bytes) = fs::read(&candidate) {
                        let ctype = if rel.ends_with(".js") {
                            "application/javascript"
                        } else if rel.ends_with(".css") {
                            "text/css"
                        } else if rel.ends_with(".svg") {
                            "image/svg+xml"
                        } else if rel.ends_with(".png") {
                            "image/png"
                        } else if rel.ends_with(".html") {
                            "text/html; charset=utf-8"
                        } else {
                            "application/octet-stream"
                        };
                        return (200, ctype.to_string(), Vec::new(), bytes);
                    }
                }
            }
            let index_path = ui_dist.join("index.html");
            if index_path.is_file() {
                if let Ok(bytes) = fs::read(&index_path) {
                    return (
                        200,
                        "text/html; charset=utf-8".to_string(),
                        Vec::new(),
                        bytes,
                    );
                }
            }
        }
        (
            200,
            "text/html; charset=utf-8".to_string(),
            Vec::new(),
            EMBEDDED_DASHBOARD_HTML.as_bytes().to_vec(),
        )
    }
}

pub(crate) fn send_raw_error_response<W: Write>(
    writer: &mut W,
    status_code: u16,
    status_text: &str,
    error_msg: &str,
) {
    let body = format!("{{\"error\":\"{}\"}}", error_msg);
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_code,
        status_text,
        body.len(),
        body
    );
    let _ = writer.write_all(resp.as_bytes());
    let _ = writer.flush();
}

pub(crate) fn read_line_capped<R: BufRead>(
    reader: &mut R,
    buf: &mut String,
    max_bytes: usize,
) -> std::io::Result<usize> {
    buf.clear();
    let mut total_read = 0;
    while total_read < max_bytes {
        let available = match reader.fill_buf() {
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            break;
        }
        let (consume_len, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => {
                let to_take = available.len().min(max_bytes - total_read);
                (to_take, false)
            }
        };
        let s = match std::str::from_utf8(&available[..consume_len]) {
            Ok(valid) => valid,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid utf-8",
                ))
            }
        };
        buf.push_str(s);
        reader.consume(consume_len);
        total_read += consume_len;
        if found_newline {
            return Ok(total_read);
        }
        if total_read >= max_bytes {
            return Err(std::io::Error::other("Line length exceeded"));
        }
    }
    Ok(total_read)
}

#[cfg(feature = "tls-rustls")]
fn decode_pem_blocks(pem: &str, begin_marker: &str, end_marker: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut b64 = String::new();
    for line in pem.lines() {
        let trimmed = line.trim();
        if trimmed.contains(begin_marker) {
            in_block = true;
            b64.clear();
        } else if trimmed.contains(end_marker) {
            if in_block {
                if let Some(der) = base64_decode_standard(&b64) {
                    out.push(der);
                }
            }
            in_block = false;
            b64.clear();
        } else if in_block {
            b64.push_str(trimmed);
        }
    }
    out
}

#[cfg(feature = "tls-rustls")]
fn base64_decode_standard(input: &str) -> Option<Vec<u8>> {
    let cleaned: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut bytes = Vec::with_capacity(cleaned.len() * 3 / 4);
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
            _ => return None,
        };
        val = (val << 6) | d;
        valb += 6;
        if valb >= 0 {
            bytes.push(((val >> valb) & 0xFF) as u8);
            valb -= 8;
        }
    }
    Some(bytes)
}

#[cfg(feature = "tls-rustls")]
pub fn build_inbound_tls_config_from_pem(
    cert_pem: &str,
    key_pem: &str,
    client_ca_pem: Option<&str>,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        decode_pem_blocks(cert_pem, "BEGIN CERTIFICATE", "END CERTIFICATE")
            .into_iter()
            .map(rustls::pki_types::CertificateDer::from)
            .collect();
    if certs.is_empty() {
        return Err("No certificates found in OMNI_TLS_CERT".to_string());
    }

    let key_der = decode_pem_blocks(key_pem, "BEGIN PRIVATE KEY", "END PRIVATE KEY")
        .into_iter()
        .next()
        .map(rustls::pki_types::PrivatePkcs8KeyDer::from)
        .map(rustls::pki_types::PrivateKeyDer::Pkcs8)
        .or_else(|| {
            decode_pem_blocks(key_pem, "BEGIN RSA PRIVATE KEY", "END RSA PRIVATE KEY")
                .into_iter()
                .next()
                .map(rustls::pki_types::PrivatePkcs1KeyDer::from)
                .map(rustls::pki_types::PrivateKeyDer::Pkcs1)
        })
        .or_else(|| {
            decode_pem_blocks(key_pem, "BEGIN EC PRIVATE KEY", "END EC PRIVATE KEY")
                .into_iter()
                .next()
                .map(rustls::pki_types::PrivateSec1KeyDer::from)
                .map(rustls::pki_types::PrivateKeyDer::Sec1)
        })
        .ok_or_else(|| "No supported private key found in OMNI_TLS_KEY".to_string())?;

    let builder = rustls::ServerConfig::builder();
    let mut cfg = if let Some(ca_pem) = client_ca_pem {
        let ca_certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            decode_pem_blocks(ca_pem, "BEGIN CERTIFICATE", "END CERTIFICATE")
                .into_iter()
                .map(rustls::pki_types::CertificateDer::from)
                .collect();
        if ca_certs.is_empty() {
            return Err("No CA certificates found in OMNI_TLS_CLIENT_CA".to_string());
        }
        let mut roots = rustls::RootCertStore::empty();
        for c in ca_certs {
            roots
                .add(c)
                .map_err(|e| format!("Invalid client CA certificate: {}", e))?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| format!("Failed to build mTLS client verifier: {}", e))?;
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key_der)
            .map_err(|e| format!("Invalid inbound mTLS certificate/key: {}", e))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key_der)
            .map_err(|e| format!("Invalid inbound TLS certificate/key: {}", e))?
    };
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

#[cfg(feature = "tls-rustls")]
pub fn build_inbound_tls_config() -> Result<Option<Arc<rustls::ServerConfig>>, String> {
    let cert_path = match std::env::var("OMNI_TLS_CERT") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => return Ok(None),
    };
    let key_path = std::env::var("OMNI_TLS_KEY")
        .map_err(|_| "OMNI_TLS_KEY must be set when OMNI_TLS_CERT is configured".to_string())?;

    let cert_pem = fs::read_to_string(&cert_path)
        .map_err(|e| format!("Failed to read OMNI_TLS_CERT '{}': {}", cert_path, e))?;
    let key_pem = fs::read_to_string(&key_path)
        .map_err(|e| format!("Failed to read OMNI_TLS_KEY '{}': {}", key_path, e))?;
    let client_ca_pem = match std::env::var("OMNI_TLS_CLIENT_CA") {
        Ok(ca_path) if !ca_path.trim().is_empty() => Some(
            fs::read_to_string(&ca_path)
                .map_err(|e| format!("Failed to read OMNI_TLS_CLIENT_CA '{}': {}", ca_path, e))?,
        ),
        _ => None,
    };

    build_inbound_tls_config_from_pem(&cert_pem, &key_pem, client_ca_pem.as_deref()).map(Some)
}

const EMBEDDED_DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
    <title>Omni MCP - Rust Enterprise Platform</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; background: #0b0b0f; color: #f3f4f6; margin: 0; padding: 24px; }
        header { border-bottom: 1px solid #222; padding-bottom: 16px; margin-bottom: 24px; display: flex; justify-content: space-between; align-items: center; }
        h1 { margin: 0; font-size: 22px; font-weight: 700; }
        .badge { background: #1b382b; color: #00e575; border-radius: 4px; padding: 4px 10px; font-size: 13px; font-weight: 600; }
        .card { background: #13141e; border: 1px solid #212333; border-radius: 8px; padding: 18px; margin-bottom: 16px; }
        .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 16px; }
        code { color: #00e575; }
    </style>
</head>
<body>
    <header>
        <div>
            <h1>Omni MCP Platform</h1>
            <p style="color: #888; margin: 4px 0 0 0;">High-Speed Enterprise MCP Gateway & Runtime Registry</p>
        </div>
        <div class="badge">Ã¢â€”Â Rust Runtime Online</div>
    </header>
    <div class="card">
        <h3>Platform Overview</h3>
        <p>Production Rust server active. Endpoints available:</p>
        <ul>
            <li>Dynamic Virtual MCP Servers: <code>/&lt;service-name&gt;/mcp</code></li>
            <li>Omni MCP Incremental Discovery Proxy: <code>/omni/mcp</code></li>
            <li>Health &amp; Readiness: <code>/healthz</code>, <code>/readyz</code>, <code>/metrics</code></li>
            <li>Control Plane APIs: <code>/api/servers</code>, <code>/api/stats</code>, <code>/api/diffs</code></li>
        </ul>
    </div>
</body>
</html>"#;
