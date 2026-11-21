//! Real PostgreSQL Wire Protocol v3.0 Cluster Storage Backend (`storage-postgres`, v6.5.0).
//!
//! Pure safe Rust (`#![forbid(unsafe_code)]`) implementation of the PostgreSQL Frontend/Backend
//! Protocol v3.0 over TCP (with optional `rustls` SSLRequest upgrade when `tls-rustls` is enabled).
//!
//! Features:
//! - Full DSN parsing (`postgres://user:pass@host:port/db?sslmode=disable|prefer|require`) with percent-decoding
//! - Authentication: Trust (`AuthenticationOk`), Cleartext, and SASL `SCRAM-SHA-256` (mutual verification)
//! - Extended Parameterized Queries (`Parse`/`Bind`/`Execute`/`Sync`) preventing SQL injection
//! - Idempotent `SCHEMA_SQL` initialization (`omni_servers`, `omni_diffs`, `omni_tool_groups`,
//!   `omni_suspended_frames`, `omni_audit_log`)
//! - Distributed Atomic CAS (`DELETE FROM omni_suspended_frames WHERE execution_id = $1 RETURNING payload`)
//! - `LISTEN omni_registry_changed` / `NOTIFY` background cluster cache invalidation listener
//! - Cryptographic `AuditSink::verify_integrity()` streaming `omni_audit_log ORDER BY idx ASC`
//!   and recomputing every SHA-256 `prev_hash -> current_hash` chain link.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{ConfigDiff, ServerRecord, SuspendedExecutionFrame, ToolGroupRecord};
use crate::storage::audit_log::{hmac_sha256, sha256, AuditSink, MerkleAuditRecord, GENESIS_HASH};
use crate::storage::wal_store::{RegistryStorageBackend, WalRegistryStore};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

/// Parsed PostgreSQL connection parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgDsn {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub sslmode: String,
}

impl PgDsn {
    pub fn parse(dsn: &str) -> Result<Self, String> {
        let rest = dsn
            .strip_prefix("postgres://")
            .or_else(|| dsn.strip_prefix("postgresql://"))
            .ok_or_else(|| format!("Invalid Postgres DSN: {}", dsn))?;

        let (before_query, query) = match rest.split_once('?') {
            Some((a, b)) => (a, b),
            None => (rest, ""),
        };

        let mut sslmode = "prefer".to_string();
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k.eq_ignore_ascii_case("sslmode") {
                    sslmode = v.to_lowercase();
                }
            }
        }

        let (auth_part, host_db_part) = match before_query.rsplit_once('@') {
            Some((a, b)) => (Some(a), b),
            None => (None, before_query),
        };

        let (user, password) = match auth_part {
            Some(auth) => match auth.split_once(':') {
                Some((u, p)) => (pg_percent_decode(u), pg_percent_decode(p)),
                None => (pg_percent_decode(auth), String::new()),
            },
            None => ("postgres".to_string(), String::new()),
        };

        let (host_port, db_raw) = match host_db_part.split_once('/') {
            Some((hp, db)) => (hp, db),
            None => (host_db_part, "postgres"),
        };

        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || host_port.starts_with('[') => {
                let port_num = p
                    .parse::<u16>()
                    .map_err(|_| format!("Invalid Postgres port '{}'", p))?;
                (h.trim_matches(&['[', ']'][..]).to_string(), port_num)
            }
            _ => (
                if host_port.is_empty() {
                    "127.0.0.1".to_string()
                } else {
                    host_port.trim_matches(&['[', ']'][..]).to_string()
                },
                5432,
            ),
        };

        let database = if db_raw.is_empty() {
            "postgres".to_string()
        } else {
            pg_percent_decode(db_raw)
        };

        Ok(Self {
            host,
            port,
            user,
            password,
            database,
            sslmode,
        })
    }
}

/// Transport abstraction allowing both cleartext TCP and `rustls` TLS streams.
enum PgTransport {
    Plain(TcpStream),
    #[cfg(feature = "tls-rustls")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for PgTransport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            PgTransport::Plain(s) => s.read(buf),
            #[cfg(feature = "tls-rustls")]
            PgTransport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for PgTransport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            PgTransport::Plain(s) => s.write(buf),
            #[cfg(feature = "tls-rustls")]
            PgTransport::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            PgTransport::Plain(s) => s.flush(),
            #[cfg(feature = "tls-rustls")]
            PgTransport::Tls(s) => s.flush(),
        }
    }
}

impl PgTransport {
    fn set_read_timeout(&self, dur: Option<Duration>) {
        match self {
            PgTransport::Plain(s) => {
                let _ = s.set_read_timeout(dur);
            }
            #[cfg(feature = "tls-rustls")]
            PgTransport::Tls(s) => {
                let _ = s.sock.set_read_timeout(dur);
            }
        }
    }
}

/// A live PostgreSQL Wire Protocol v3.0 connection.
pub struct PgWireConnection {
    transport: PgTransport,
    pub pending_notifications: Vec<(String, String)>,
}

impl PgWireConnection {
    pub fn connect(dsn: &PgDsn) -> Result<Self, String> {
        let addr = format!("{}:{}", dsn.host, dsn.port);
        let mut tcp = TcpStream::connect(&addr)
            .map_err(|e| format!("Failed to connect to PostgreSQL at {}: {}", addr, e))?;
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
        let _ = tcp.set_nodelay(true);

        let transport = if dsn.sslmode != "disable" {
            // Send PostgreSQL SSLRequest (length 8, code 80877103)
            let mut ssl_req = [0u8; 8];
            ssl_req[0..4].copy_from_slice(&8u32.to_be_bytes());
            ssl_req[4..8].copy_from_slice(&80877103u32.to_be_bytes());
            tcp.write_all(&ssl_req)
                .and_then(|_| tcp.flush())
                .map_err(|e| format!("Failed sending PostgreSQL SSLRequest: {}", e))?;
            let mut reply = [0u8; 1];
            match tcp.read_exact(&mut reply) {
                Ok(()) if reply[0] == b'S' => {
                    #[cfg(feature = "tls-rustls")]
                    {
                        let mut roots = rustls::RootCertStore::empty();
                        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                        let cfg = rustls::ClientConfig::builder()
                            .with_root_certificates(roots)
                            .with_no_client_auth();
                        let server_name = rustls::pki_types::ServerName::try_from(dsn.host.clone())
                            .map_err(|e| {
                                format!("Invalid Postgres TLS SNI '{}': {}", dsn.host, e)
                            })?;
                        let conn = rustls::ClientConnection::new(Arc::new(cfg), server_name)
                            .map_err(|e| format!("Postgres TLS init failed: {}", e))?;
                        PgTransport::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)))
                    }
                    #[cfg(not(feature = "tls-rustls"))]
                    {
                        if dsn.sslmode == "require" {
                            return Err(
                                "PostgreSQL sslmode=require requires the 'tls-rustls' feature"
                                    .to_string(),
                            );
                        }
                        // Reconnect without SSLRequest when tls-rustls is not compiled in
                        drop(tcp);
                        let plain = TcpStream::connect(&addr).map_err(|e| {
                            format!("Failed to reconnect to PostgreSQL at {}: {}", addr, e)
                        })?;
                        let _ = plain.set_read_timeout(Some(Duration::from_secs(10)));
                        let _ = plain.set_write_timeout(Some(Duration::from_secs(10)));
                        PgTransport::Plain(plain)
                    }
                }
                Ok(()) if reply[0] == b'N' => {
                    if dsn.sslmode == "require" {
                        return Err("PostgreSQL server rejected SSL (sslmode=require)".to_string());
                    }
                    PgTransport::Plain(tcp)
                }
                Ok(()) => {
                    return Err(format!(
                        "Unexpected PostgreSQL SSLRequest response byte: 0x{:02x}",
                        reply[0]
                    ));
                }
                Err(e) => {
                    return Err(format!("Failed reading PostgreSQL SSLRequest reply: {}", e));
                }
            }
        } else {
            PgTransport::Plain(tcp)
        };

        let mut conn = Self {
            transport,
            pending_notifications: Vec::new(),
        };
        conn.startup_and_authenticate(dsn)?;
        Ok(conn)
    }

    fn write_startup_message(&mut self, dsn: &PgDsn) -> Result<(), String> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&196608u32.to_be_bytes()); // protocol 3.0
        payload.extend_from_slice(b"user\0");
        payload.extend_from_slice(dsn.user.as_bytes());
        payload.push(0);
        payload.extend_from_slice(b"database\0");
        payload.extend_from_slice(dsn.database.as_bytes());
        payload.push(0);
        payload.extend_from_slice(b"application_name\0omni-mcp\0");
        payload.push(0);

        let total_len = (4 + payload.len()) as u32;
        self.transport
            .write_all(&total_len.to_be_bytes())
            .map_err(|e| format!("PG startup write len failed: {}", e))?;
        self.transport
            .write_all(&payload)
            .map_err(|e| format!("PG startup write body failed: {}", e))?;
        self.transport
            .flush()
            .map_err(|e| format!("PG startup flush failed: {}", e))
    }

    fn write_message(&mut self, msg_type: u8, payload: &[u8]) -> Result<(), String> {
        let total_len = (4 + payload.len()) as u32;
        self.transport
            .write_all(&[msg_type])
            .map_err(|e| format!("PG write type failed: {}", e))?;
        self.transport
            .write_all(&total_len.to_be_bytes())
            .map_err(|e| format!("PG write len failed: {}", e))?;
        if !payload.is_empty() {
            self.transport
                .write_all(payload)
                .map_err(|e| format!("PG write body failed: {}", e))?;
        }
        Ok(())
    }

    fn read_message(&mut self) -> Result<(u8, Vec<u8>), String> {
        let mut hdr = [0u8; 5];
        self.transport
            .read_exact(&mut hdr)
            .map_err(|e| format!("PG read header failed: {}", e))?;
        let msg_type = hdr[0];
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        if !(4..=64 * 1024 * 1024).contains(&len) {
            return Err(format!("Invalid PG message length: {}", len));
        }
        let mut body = vec![0u8; len - 4];
        if !body.is_empty() {
            self.transport
                .read_exact(&mut body)
                .map_err(|e| format!("PG read body failed: {}", e))?;
        }
        Ok((msg_type, body))
    }

    fn parse_error_response(body: &[u8]) -> String {
        let mut message = String::new();
        let mut code = String::new();
        let mut pos = 0;
        while pos < body.len() {
            let field_type = body[pos];
            pos += 1;
            if field_type == 0 {
                break;
            }
            let end = body[pos..]
                .iter()
                .position(|&b| b == 0)
                .map(|i| pos + i)
                .unwrap_or(body.len());
            let val = String::from_utf8_lossy(&body[pos..end]);
            match field_type {
                b'M' => message = val.into_owned(),
                b'C' => code = val.into_owned(),
                _ => {}
            }
            pos = (end + 1).min(body.len());
        }
        if code.is_empty() {
            format!("PostgreSQL error: {}", message)
        } else {
            format!("PostgreSQL error [{}]: {}", code, message)
        }
    }

    fn startup_and_authenticate(&mut self, dsn: &PgDsn) -> Result<(), String> {
        self.write_startup_message(dsn)?;
        let mut scram_state: Option<(String, String, Vec<u8>)> = None;

        loop {
            let (msg_type, body) = self.read_message()?;
            match msg_type {
                b'R' => {
                    if body.len() < 4 {
                        return Err("Malformed Authentication message".to_string());
                    }
                    let auth_code = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    match auth_code {
                        0 => {
                            // AuthenticationOk: reject if server skipped SASLFinal mutual verification
                            if scram_state.is_some() {
                                return Err(
                                    "SCRAM-SHA-256 downgrade rejected: server sent AuthenticationOk before SASLFinal"
                                        .to_string(),
                                );
                            }
                        }
                        3 => {
                            // Cleartext password
                            let mut pwd = dsn.password.as_bytes().to_vec();
                            pwd.push(0);
                            self.write_message(b'p', &pwd)?;
                            self.transport.flush().map_err(|e| e.to_string())?;
                        }
                        10 => {
                            // AuthenticationSASL -> verify SCRAM-SHA-256 is offered
                            let offered: Vec<String> = body[4..]
                                .split(|&b| b == 0)
                                .filter(|s| !s.is_empty())
                                .map(|s| String::from_utf8_lossy(s).into_owned())
                                .collect();
                            if !offered.iter().any(|m| m == "SCRAM-SHA-256") {
                                return Err(format!(
                                    "PostgreSQL server did not offer SCRAM-SHA-256 (offered: {:?})",
                                    offered
                                ));
                            }
                            let mut nonce_bytes = [0u8; 18];
                            crate::models::gather_entropy(&mut nonce_bytes);
                            let client_nonce = base64_std_encode(&nonce_bytes);
                            let client_first_bare = format!("n=,r={}", client_nonce);
                            let client_first_msg = format!("n,,{}", client_first_bare);

                            let mut resp = Vec::new();
                            resp.extend_from_slice(b"SCRAM-SHA-256\0");
                            resp.extend_from_slice(&(client_first_msg.len() as u32).to_be_bytes());
                            resp.extend_from_slice(client_first_msg.as_bytes());
                            self.write_message(b'p', &resp)?;
                            self.transport.flush().map_err(|e| e.to_string())?;
                            scram_state = Some((client_nonce, client_first_bare, Vec::new()));
                        }
                        11 => {
                            // AuthenticationSASLContinue
                            let server_first = String::from_utf8_lossy(&body[4..]).into_owned();
                            let (client_nonce, client_first_bare, _) = scram_state
                                .take()
                                .ok_or("Unexpected SASLContinue without initial SASL state")?;

                            let mut srv_nonce = String::new();
                            let mut salt_b64 = String::new();
                            let mut iterations = 4096u32;
                            for part in server_first.split(',') {
                                if let Some(v) = part.strip_prefix("r=") {
                                    srv_nonce = v.to_string();
                                } else if let Some(v) = part.strip_prefix("s=") {
                                    salt_b64 = v.to_string();
                                } else if let Some(v) = part.strip_prefix("i=") {
                                    iterations = v.parse().unwrap_or(4096);
                                }
                            }
                            if !srv_nonce.starts_with(&client_nonce)
                                || srv_nonce.len() <= client_nonce.len()
                            {
                                return Err("SCRAM-SHA-256 server nonce mismatch".to_string());
                            }
                            let salt = base64_std_decode(&salt_b64)
                                .ok_or("Invalid SCRAM-SHA-256 salt base64")?;
                            let salted_pwd =
                                pbkdf2_hmac_sha256(dsn.password.as_bytes(), &salt, iterations);
                            let client_key = hmac_sha256(&salted_pwd, b"Client Key");
                            let stored_key = sha256(&client_key);
                            let client_final_without_proof = format!("c=biws,r={}", srv_nonce);
                            let auth_msg = format!(
                                "{},{},{}",
                                client_first_bare, server_first, client_final_without_proof
                            );
                            let client_sig = hmac_sha256(&stored_key, auth_msg.as_bytes());
                            let mut proof = [0u8; 32];
                            for i in 0..32 {
                                proof[i] = client_key[i] ^ client_sig[i];
                            }
                            let server_key = hmac_sha256(&salted_pwd, b"Server Key");
                            let expected_srv_sig = hmac_sha256(&server_key, auth_msg.as_bytes());

                            let client_final = format!(
                                "{},p={}",
                                client_final_without_proof,
                                base64_std_encode(&proof)
                            );
                            self.write_message(b'p', client_final.as_bytes())?;
                            self.transport.flush().map_err(|e| e.to_string())?;
                            scram_state =
                                Some((client_nonce, client_first_bare, expected_srv_sig.to_vec()));
                        }
                        12 => {
                            // AuthenticationSASLFinal: MUST verify server signature `v=`
                            let server_final = String::from_utf8_lossy(&body[4..]);
                            let (_, _, expected_sig) = scram_state
                                .take()
                                .ok_or("Unexpected SASLFinal without pending SCRAM state")?;
                            let v_field = server_final
                                .split(',')
                                .find_map(|p| p.strip_prefix("v="))
                                .ok_or_else(|| {
                                    format!(
                                        "SCRAM-SHA-256 server final missing 'v=' signature: {}",
                                        server_final
                                    )
                                })?;
                            let expected_b64 = base64_std_encode(&expected_sig);
                            if !crate::models::constant_time_eq(
                                v_field.as_bytes(),
                                expected_b64.as_bytes(),
                            ) {
                                return Err("SCRAM-SHA-256 server signature verification failed"
                                    .to_string());
                            }
                        }
                        other => {
                            return Err(format!(
                                "Unsupported PostgreSQL authentication type: {}",
                                other
                            ));
                        }
                    }
                }
                b'S' | b'K' | b'N' => {} // ParameterStatus, BackendKeyData, NoticeResponse
                b'Z' => return Ok(()),   // ReadyForQuery
                b'E' => return Err(Self::parse_error_response(&body)),
                _ => {}
            }
        }
    }

    /// Executes a simple query (DDL, `LISTEN`, `NOTIFY`, multi-statement schema batch).
    pub fn execute_simple(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
        let mut payload = sql.as_bytes().to_vec();
        payload.push(0);
        self.write_message(b'Q', &payload)?;
        self.transport.flush().map_err(|e| e.to_string())?;
        self.collect_query_results()
    }

    /// Executes a parameterized Extended Query (`Parse` + `Bind` + `Execute` + `Sync`),
    /// preventing SQL injection for arbitrary JSON payloads and identifiers.
    pub fn execute_params(
        &mut self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<Vec<Option<String>>>, String> {
        // Parse ('P'): unnamed statement "", query\0, param count 0 (server infers types)
        let mut parse_buf = Vec::new();
        parse_buf.push(0); // unnamed statement
        parse_buf.extend_from_slice(sql.as_bytes());
        parse_buf.push(0);
        parse_buf.extend_from_slice(&0u16.to_be_bytes());
        self.write_message(b'P', &parse_buf)?;

        // Bind ('B'): unnamed portal "", unnamed statement "", 0 param formats (all text),
        // N param values, 0 result formats (all text)
        let mut bind_buf = Vec::new();
        bind_buf.push(0); // portal
        bind_buf.push(0); // statement
        bind_buf.extend_from_slice(&0u16.to_be_bytes()); // param format codes
        bind_buf.extend_from_slice(&(params.len() as u16).to_be_bytes());
        for p in params {
            let bytes = p.as_bytes();
            bind_buf.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
            bind_buf.extend_from_slice(bytes);
        }
        bind_buf.extend_from_slice(&0u16.to_be_bytes()); // result format codes (text)
        self.write_message(b'B', &bind_buf)?;

        // Execute ('E'): unnamed portal "", max rows 0 (unlimited)
        let mut exec_buf = Vec::new();
        exec_buf.push(0);
        exec_buf.extend_from_slice(&0u32.to_be_bytes());
        self.write_message(b'E', &exec_buf)?;

        // Sync ('S')
        self.write_message(b'S', &[])?;
        self.transport.flush().map_err(|e| e.to_string())?;

        self.collect_query_results()
    }

    fn collect_query_results(&mut self) -> Result<Vec<Vec<Option<String>>>, String> {
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut err: Option<String> = None;

        loop {
            let (msg_type, body) = self.read_message()?;
            match msg_type {
                b'D' => {
                    // DataRow: i16 num_cols, then for each col: i32 len (-1 = NULL) + bytes
                    if body.len() >= 2 {
                        let num_cols = u16::from_be_bytes([body[0], body[1]]) as usize;
                        let mut pos = 2;
                        let mut row = Vec::with_capacity(num_cols);
                        for _ in 0..num_cols {
                            if pos + 4 > body.len() {
                                break;
                            }
                            let col_len = i32::from_be_bytes([
                                body[pos],
                                body[pos + 1],
                                body[pos + 2],
                                body[pos + 3],
                            ]);
                            pos += 4;
                            if col_len < 0 {
                                row.push(None);
                            } else {
                                let end = (pos + col_len as usize).min(body.len());
                                row.push(Some(
                                    String::from_utf8_lossy(&body[pos..end]).into_owned(),
                                ));
                                pos = end;
                            }
                        }
                        rows.push(row);
                    }
                }
                b'A' => {
                    // NotificationResponse: i32 pid, channel\0, payload\0
                    if body.len() > 4 {
                        let rest = &body[4..];
                        let parts: Vec<&[u8]> = rest.split(|&b| b == 0).collect();
                        if parts.len() >= 2 {
                            let ch = String::from_utf8_lossy(parts[0]).into_owned();
                            let pl = String::from_utf8_lossy(parts[1]).into_owned();
                            self.pending_notifications.push((ch, pl));
                        }
                    }
                }
                b'E' => {
                    err = Some(Self::parse_error_response(&body));
                }
                b'Z' => {
                    // ReadyForQuery
                    if let Some(e) = err {
                        return Err(e);
                    }
                    return Ok(rows);
                }
                b'1' | b'2' | b'T' | b'C' | b'I' | b'n' | b'N' | b'S' => {}
                _ => {}
            }
        }
    }

    /// Polls for an asynchronous `NotificationResponse ('A')` with a short read timeout.
    pub fn poll_notification(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(String, String)>, String> {
        if let Some(notif) = self.pending_notifications.pop() {
            return Ok(Some(notif));
        }
        self.transport.set_read_timeout(Some(timeout));
        match self.read_message() {
            Ok((b'A', body)) if body.len() > 4 => {
                let rest = &body[4..];
                let parts: Vec<&[u8]> = rest.split(|&b| b == 0).collect();
                if parts.len() >= 2 {
                    let ch = String::from_utf8_lossy(parts[0]).into_owned();
                    let pl = String::from_utf8_lossy(parts[1]).into_owned();
                    return Ok(Some((ch, pl)));
                }
                Ok(None)
            }
            Ok(_) => Ok(None),
            Err(e) => {
                if e.contains("timed out")
                    || e.contains("WouldBlock")
                    || e.contains("10060")
                    || e.contains("Resource temporarily unavailable")
                {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// Real PostgreSQL multi-replica storage backend (`storage-postgres`, v6.5.0).
pub struct PostgresRegistryBackend {
    pub dsn: String,
    parsed_dsn: Option<PgDsn>,
    wire_conn: Mutex<Option<PgWireConnection>>,
    memory_fallback: Option<WalRegistryStore>,
    memory_audit: RwLock<Vec<MerkleAuditRecord>>,
    listener_running: Arc<AtomicBool>,
}

impl PostgresRegistryBackend {
    pub const SCHEMA_SQL: &'static str = "
        CREATE TABLE IF NOT EXISTS omni_servers (name TEXT PRIMARY KEY, payload JSONB NOT NULL, updated_at DOUBLE PRECISION NOT NULL);
        CREATE TABLE IF NOT EXISTS omni_diffs (diff_id TEXT PRIMARY KEY, payload JSONB NOT NULL, resolved BOOLEAN NOT NULL DEFAULT FALSE);
        CREATE TABLE IF NOT EXISTS omni_tool_groups (slug TEXT PRIMARY KEY, payload JSONB NOT NULL);
        CREATE TABLE IF NOT EXISTS omni_suspended_frames (execution_id TEXT PRIMARY KEY, payload JSONB NOT NULL);
        CREATE TABLE IF NOT EXISTS omni_audit_log (idx BIGINT PRIMARY KEY, current_hash TEXT NOT NULL, payload JSONB NOT NULL);
    ";

    pub const NOTIFY_CHANNEL: &'static str = "omni_registry_changed";

    /// Connects to a real PostgreSQL cluster via Wire Protocol v3.0 (or uses an in-memory
    /// backend with real cryptographic audit verification when `dsn == ":memory:"`).
    pub fn connect(dsn: impl Into<String>) -> Result<Self, String> {
        let dsn = dsn.into();
        if dsn == ":memory:" {
            return Ok(Self {
                dsn,
                parsed_dsn: None,
                wire_conn: Mutex::new(None),
                memory_fallback: Some(WalRegistryStore::new(":memory:")),
                memory_audit: RwLock::new(Vec::new()),
                listener_running: Arc::new(AtomicBool::new(false)),
            });
        }

        let parsed = PgDsn::parse(&dsn)?;
        let mut conn = PgWireConnection::connect(&parsed)?;
        conn.execute_simple(Self::SCHEMA_SQL)
            .map_err(|e| format!("Failed to initialize PostgreSQL schema: {}", e))?;

        Ok(Self {
            dsn,
            parsed_dsn: Some(parsed),
            wire_conn: Mutex::new(Some(conn)),
            memory_fallback: None,
            memory_audit: RwLock::new(Vec::new()),
            listener_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Executes a parameterized query against PostgreSQL with 1 automatic reconnect attempt.
    fn run_query(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Option<String>>>, String> {
        let mut guard = self
            .wire_conn
            .lock()
            .map_err(|_| "PostgreSQL connection lock poisoned".to_string())?;

        if guard.is_none() {
            if let Some(ref parsed) = self.parsed_dsn {
                let conn = PgWireConnection::connect(parsed)?;
                *guard = Some(conn);
            } else {
                return Err("PostgreSQL connection not initialized".to_string());
            }
        }

        let first_attempt = guard.as_mut().unwrap().execute_params(sql, params);
        match first_attempt {
            Ok(rows) => Ok(rows),
            Err(first_err) => {
                // Only reconnect on transport/socket errors — never replay SQL errors
                if first_err.starts_with("PostgreSQL error") {
                    return Err(first_err);
                }
                *guard = None;
                if let Some(ref parsed) = self.parsed_dsn {
                    if let Ok(mut fresh) = PgWireConnection::connect(parsed) {
                        let retry = fresh.execute_params(sql, params);
                        if retry.is_ok() {
                            *guard = Some(fresh);
                        }
                        return retry;
                    }
                }
                Err(first_err)
            }
        }
    }

    fn emit_notify(&self, key: &str) {
        if self.memory_fallback.is_some() {
            return;
        }
        let sanitized: String = key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
            .collect();
        let sql = format!("NOTIFY {}, '{}'", Self::NOTIFY_CHANNEL, sanitized);
        if let Ok(mut guard) = self.wire_conn.lock() {
            if let Some(ref mut conn) = *guard {
                let _ = conn.execute_simple(&sql);
            }
        }
    }

    /// Starts a dedicated background PostgreSQL `LISTEN omni_registry_changed` connection
    /// that invokes `on_change(payload)` immediately when sibling replicas commit mutations.
    pub fn start_invalidation_listener<F>(&self, on_change: F) -> Result<(), String>
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        let parsed = match &self.parsed_dsn {
            Some(p) => p.clone(),
            None => return Ok(()),
        };
        if self.listener_running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let running = self.listener_running.clone();
        let mut listen_conn = PgWireConnection::connect(&parsed)?;
        listen_conn.execute_simple(&format!("LISTEN {}", Self::NOTIFY_CHANNEL))?;

        thread::spawn(move || {
            let mut conn = Some(listen_conn);
            while running.load(Ordering::SeqCst) {
                if conn.is_none() {
                    thread::sleep(Duration::from_millis(250));
                    if let Ok(mut fresh) = PgWireConnection::connect(&parsed) {
                        if fresh
                            .execute_simple(&format!("LISTEN {}", Self::NOTIFY_CHANNEL))
                            .is_ok()
                        {
                            conn = Some(fresh);
                        }
                    }
                    continue;
                }
                let poll_res = conn
                    .as_mut()
                    .unwrap()
                    .poll_notification(Duration::from_millis(200));
                match poll_res {
                    Ok(Some((_channel, payload))) => {
                        on_change(&payload);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        conn = None;
                    }
                }
            }
        });
        Ok(())
    }

    pub fn stop_invalidation_listener(&self) {
        self.listener_running.store(false, Ordering::SeqCst);
    }

    pub fn save_suspended_frame(&self, frame: &SuspendedExecutionFrame) {
        if let Some(ref mem) = self.memory_fallback {
            mem.save_suspended_frame(frame);
            return;
        }
        let payload = frame.to_json().to_string_compact();
        let _ = self.run_query(
            "INSERT INTO omni_suspended_frames (execution_id, payload) VALUES ($1, $2::jsonb) \
             ON CONFLICT (execution_id) DO UPDATE SET payload = EXCLUDED.payload",
            &[&frame.execution_id, &payload],
        );
        self.emit_notify(&format!("frame:{}", frame.execution_id));
    }

    pub fn load_all_suspended_frames(&self) -> Vec<SuspendedExecutionFrame> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.list_suspended_frames();
        }
        let rows = match self.run_query(
            "SELECT payload::text FROM omni_suspended_frames ORDER BY execution_id ASC",
            &[],
        ) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .filter_map(|p| JsonValue::parse(&p).ok())
            .filter_map(|j| SuspendedExecutionFrame::from_json(&j))
            .collect()
    }

    pub fn delete_suspended_frame(&self, execution_id: &str) -> Option<SuspendedExecutionFrame> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.delete_suspended_frame(execution_id);
        }
        let rows = self
            .run_query(
                "DELETE FROM omni_suspended_frames WHERE execution_id = $1 RETURNING payload::text",
                &[execution_id],
            )
            .ok()?;
        let payload = rows.into_iter().next()?.into_iter().next().flatten()?;
        let json = JsonValue::parse(&payload).ok()?;
        SuspendedExecutionFrame::from_json(&json)
    }

    /// Recomputes SHA-256 hash chain over a slice of `(stored_hash, MerkleAuditRecord)` tuples.
    pub fn verify_audit_chain(records: &[(String, MerkleAuditRecord)]) -> (bool, usize) {
        let mut expected_prev = GENESIS_HASH.to_string();
        let mut count = 0usize;
        for (stored_hash, rec) in records {
            let recomputed = MerkleAuditRecord::compute_hash(
                rec.index,
                rec.timestamp,
                &rec.prev_hash,
                &rec.actor,
                &rec.server_name,
                &rec.tool_name,
                &rec.args_hash,
                &rec.status,
            );
            if rec.prev_hash != expected_prev
                || rec.current_hash != *stored_hash
                || rec.current_hash != recomputed
            {
                return (false, count);
            }
            expected_prev = rec.current_hash.clone();
            count += 1;
        }
        (true, count)
    }
}

impl Drop for PostgresRegistryBackend {
    fn drop(&mut self) {
        self.stop_invalidation_listener();
    }
}

impl RegistryStorageBackend for PostgresRegistryBackend {
    fn load_all_servers(&self) -> Vec<ServerRecord> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.load_all_servers();
        }
        let rows = match self.run_query(
            "SELECT payload::text FROM omni_servers ORDER BY name ASC",
            &[],
        ) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .filter_map(|p| JsonValue::parse(&p).ok())
            .filter_map(|j| ServerRecord::from_json(&j))
            .collect()
    }

    fn save_server(&self, server: &ServerRecord) {
        if let Some(ref mem) = self.memory_fallback {
            mem.save_server(server);
            return;
        }
        let payload = server.to_json().to_string_compact();
        let updated_at = server
            .heartbeat_timestamp
            .unwrap_or_else(now_epoch_secs)
            .to_string();
        let _ = self.run_query(
            "INSERT INTO omni_servers (name, payload, updated_at) VALUES ($1, $2::jsonb, $3::double precision) \
             ON CONFLICT (name) DO UPDATE SET payload = EXCLUDED.payload, updated_at = EXCLUDED.updated_at",
            &[&server.name, &payload, &updated_at],
        );
        self.emit_notify(&format!("server:{}", server.name));
    }

    fn delete_server(&self, name: &str) -> Option<ServerRecord> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.delete_server(name);
        }
        let rows = self
            .run_query(
                "DELETE FROM omni_servers WHERE name = $1 RETURNING payload::text",
                &[name],
            )
            .ok()?;
        let payload = rows.into_iter().next()?.into_iter().next().flatten()?;
        self.emit_notify(&format!("server:{}", name));
        let json = JsonValue::parse(&payload).ok()?;
        ServerRecord::from_json(&json)
    }

    fn load_all_diffs(&self) -> Vec<ConfigDiff> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.load_all_diffs();
        }
        let rows = match self.run_query(
            "SELECT payload::text FROM omni_diffs ORDER BY diff_id ASC",
            &[],
        ) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .filter_map(|p| JsonValue::parse(&p).ok())
            .filter_map(|j| ConfigDiff::from_json(&j))
            .collect()
    }

    fn save_diff(&self, diff: &ConfigDiff) {
        if let Some(ref mem) = self.memory_fallback {
            mem.save_diff(diff);
            return;
        }
        let payload = diff.to_json().to_string_compact();
        let resolved = if diff.approved { "true" } else { "false" };
        let _ = self.run_query(
            "INSERT INTO omni_diffs (diff_id, payload, resolved) VALUES ($1, $2::jsonb, $3::boolean) \
             ON CONFLICT (diff_id) DO UPDATE SET payload = EXCLUDED.payload, resolved = EXCLUDED.resolved",
            &[&diff.diff_id, &payload, resolved],
        );
        self.emit_notify(&format!("diff:{}", diff.diff_id));
    }

    fn load_all_tool_groups(&self) -> Vec<ToolGroupRecord> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.load_all_tool_groups();
        }
        let rows = match self.run_query(
            "SELECT payload::text FROM omni_tool_groups ORDER BY slug ASC",
            &[],
        ) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .filter_map(|p| JsonValue::parse(&p).ok())
            .filter_map(|j| ToolGroupRecord::from_json(&j))
            .collect()
    }

    fn save_tool_group(&self, group: &ToolGroupRecord) {
        if let Some(ref mem) = self.memory_fallback {
            mem.save_tool_group(group);
            return;
        }
        let payload = group.to_json().to_string_compact();
        let _ = self.run_query(
            "INSERT INTO omni_tool_groups (slug, payload) VALUES ($1, $2::jsonb) \
             ON CONFLICT (slug) DO UPDATE SET payload = EXCLUDED.payload",
            &[&group.slug, &payload],
        );
        self.emit_notify(&format!("group:{}", group.slug));
    }

    fn delete_tool_group(&self, slug: &str) -> Option<ToolGroupRecord> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.delete_tool_group(slug);
        }
        let rows = self
            .run_query(
                "DELETE FROM omni_tool_groups WHERE slug = $1 RETURNING payload::text",
                &[slug],
            )
            .ok()?;
        let payload = rows.into_iter().next()?.into_iter().next().flatten()?;
        self.emit_notify(&format!("group:{}", slug));
        let json = JsonValue::parse(&payload).ok()?;
        ToolGroupRecord::from_json(&json)
    }

    fn load_all_suspended_frames(&self) -> Vec<SuspendedExecutionFrame> {
        PostgresRegistryBackend::load_all_suspended_frames(self)
    }

    fn save_suspended_frame(&self, frame: &SuspendedExecutionFrame) {
        PostgresRegistryBackend::save_suspended_frame(self, frame)
    }

    fn delete_suspended_frame(&self, execution_id: &str) -> Option<SuspendedExecutionFrame> {
        PostgresRegistryBackend::delete_suspended_frame(self, execution_id)
    }

    /// Distributed atomic Compare-And-Swap across N horizontal gateway replicas using
    /// `DELETE FROM omni_suspended_frames WHERE execution_id = $1 RETURNING payload::text`.
    fn consume_frame_cas(
        &self,
        execution_id: &str,
        approver: &str,
    ) -> Result<SuspendedExecutionFrame, String> {
        if let Some(ref mem) = self.memory_fallback {
            return mem.consume_frame_cas(execution_id, approver);
        }

        let rows = self.run_query(
            "DELETE FROM omni_suspended_frames WHERE execution_id = $1 RETURNING payload::text",
            &[execution_id],
        )?;
        let payload = rows
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next().flatten())
            .ok_or_else(|| {
                format!(
                    "Suspended execution frame '{}' not found or already consumed",
                    execution_id
                )
            })?;

        let json = JsonValue::parse(&payload)
            .map_err(|e| format!("Corrupt suspended frame JSON: {}", e))?;
        let mut frame = SuspendedExecutionFrame::from_json(&json)
            .ok_or_else(|| "Failed to deserialize suspended frame".to_string())?;

        if frame.is_consumed {
            return Err("Suspended execution frame has already been consumed".to_string());
        }
        if approver == frame.caller_actor {
            // Re-insert so a rejected self-approval does not destroy the pending frame
            self.save_suspended_frame(&frame);
            return Err(
                "Separation of duties violation: requester cannot approve own frame".to_string(),
            );
        }
        if now_epoch_secs() > frame.expires_at {
            return Err("Suspended execution frame has expired".to_string());
        }

        frame.is_consumed = true;
        frame.approver_identity = Some(approver.to_string());
        self.emit_notify(&format!("frame_consumed:{}", execution_id));
        Ok(frame)
    }
}

impl AuditSink for PostgresRegistryBackend {
    fn emit_record(&self, record: &MerkleAuditRecord) -> Result<(), String> {
        if self.memory_fallback.is_some() {
            let mut guard = self
                .memory_audit
                .write()
                .map_err(|_| "Postgres audit lock poisoned".to_string())?;
            guard.push(record.clone());
            return Ok(());
        }
        let idx_str = record.index.to_string();
        let payload = record.to_json().to_string_compact();
        self.run_query(
            "INSERT INTO omni_audit_log (idx, current_hash, payload) VALUES ($1::bigint, $2, $3::jsonb) \
             ON CONFLICT (idx) DO UPDATE SET current_hash = EXCLUDED.current_hash, payload = EXCLUDED.payload",
            &[&idx_str, &record.current_hash, &payload],
        )?;
        Ok(())
    }

    fn verify_integrity(&self) -> (bool, usize) {
        if self.memory_fallback.is_some() {
            let guard = match self.memory_audit.read() {
                Ok(g) => g,
                Err(_) => return (false, 0),
            };
            let mut sorted: Vec<(String, MerkleAuditRecord)> = guard
                .iter()
                .map(|r| (r.current_hash.clone(), r.clone()))
                .collect();
            sorted.sort_by_key(|(_, r)| r.index);
            return Self::verify_audit_chain(&sorted);
        }

        let rows = match self.run_query(
            "SELECT current_hash, payload::text FROM omni_audit_log ORDER BY idx ASC",
            &[],
        ) {
            Ok(r) => r,
            Err(_) => return (false, 0),
        };
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() < 2 {
                return (false, records.len());
            }
            let stored_hash = match &row[0] {
                Some(h) => h.clone(),
                None => return (false, records.len()),
            };
            let payload_str = match &row[1] {
                Some(p) => p,
                None => return (false, records.len()),
            };
            let json = match JsonValue::parse(payload_str) {
                Ok(j) => j,
                Err(_) => return (false, records.len()),
            };
            let rec = match MerkleAuditRecord::from_json(&json) {
                Some(r) => r,
                None => return (false, records.len()),
            };
            records.push((stored_hash, rec));
        }
        Self::verify_audit_chain(&records)
    }
}

fn pg_percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut salt_block = Vec::with_capacity(salt.len() + 4);
    salt_block.extend_from_slice(salt);
    salt_block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &salt_block);
    let mut out = u;
    for _ in 1..iterations.max(1) {
        u = hmac_sha256(password, &u);
        for i in 0..32 {
            out[i] ^= u[i];
        }
    }
    out
}

fn base64_std_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_std_decode(input: &str) -> Option<Vec<u8>> {
    let cleaned: String = input.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
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
            out.push(((val >> valb) & 0xFF) as u8);
            valb -= 8;
        }
    }
    Some(out)
}
