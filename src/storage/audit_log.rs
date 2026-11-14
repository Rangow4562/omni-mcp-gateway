//! Merkle Cryptographic Audit Log - Zero-dependency SHA-256 chained audit trail
//! for enterprise compliance, tamper-evident agent execution records, and ABAC verification.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::sync::RwLock;

/// Pure standard library SHA-256 implementation (FIPS 180-4 compliant).
#[allow(unknown_lints)]
#[allow(clippy::chunks_exact_to_as_chunks)]
pub fn sha256(input: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64) * 8;
    let mut msg = input.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, val) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
    }
    out
}

pub fn sha256_hex(input: &[u8]) -> String {
    let digest = sha256(input);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Computes HMAC-SHA256 (RFC 2104) using the in-tree FIPS 180-4 SHA-256 implementation.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let hashed = sha256(key);
        k[..32].copy_from_slice(&hashed);
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }

    let mut inner = Vec::with_capacity(64 + data.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);

    let mut outer = Vec::with_capacity(64 + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

/// Computes HMAC-SHA256 and formats the resulting 32-byte digest as a lower-case hex string.
pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    let digest = hmac_sha256(key, data);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Debug, PartialEq)]
pub struct MerkleAuditRecord {
    pub index: u64,
    pub timestamp: f64,
    pub prev_hash: String,
    pub current_hash: String,
    pub actor: String,
    pub server_name: String,
    pub tool_name: String,
    pub args_hash: String,
    pub status: String,
    pub execution_us: u64,
}

impl MerkleAuditRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn compute_hash(
        index: u64,
        timestamp: f64,
        prev_hash: &str,
        actor: &str,
        server_name: &str,
        tool_name: &str,
        args_hash: &str,
        status: &str,
    ) -> String {
        let content = format!(
            "{}:{}:{}:{}:{}:{}:{}:{}",
            index, timestamp as u64, prev_hash, actor, server_name, tool_name, args_hash, status
        );
        sha256_hex(content.as_bytes())
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "index": self.index as i64,
            "timestamp": self.timestamp,
            "prev_hash": self.prev_hash.clone(),
            "current_hash": self.current_hash.clone(),
            "actor": self.actor.clone(),
            "server_name": self.server_name.clone(),
            "tool_name": self.tool_name.clone(),
            "args_hash": self.args_hash.clone(),
            "status": self.status.clone(),
            "execution_us": self.execution_us as i64,
        })
    }

    pub fn from_json(v: &JsonValue) -> Option<Self> {
        let index = v.get("index").and_then(|x| x.as_u64())?;
        let timestamp = v.get("timestamp").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let prev_hash = v.get("prev_hash").and_then(|x| x.as_str())?.to_string();
        let current_hash = v.get("current_hash").and_then(|x| x.as_str())?.to_string();
        let actor = v
            .get("actor")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string();
        let server_name = v
            .get("server_name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let tool_name = v
            .get("tool_name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let args_hash = v
            .get("args_hash")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let status = v
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("OK")
            .to_string();
        let execution_us = v.get("execution_us").and_then(|x| x.as_u64()).unwrap_or(0);

        Some(Self {
            index,
            timestamp,
            prev_hash,
            current_hash,
            actor,
            server_name,
            tool_name,
            args_hash,
            status,
            execution_us,
        })
    }
}

struct AuditState {
    records: VecDeque<MerkleAuditRecord>,
    total_count: u64,
    last_hash: String,
    window_base_prev_hash: String,
    segment_index: u64,
    records_in_segment: usize,
    memory_window: usize,
    segment_max_records: usize,
}

/// Pluggable audit sink trait for forwarding immutable audit records (v6.0.0).
pub trait AuditSink: Send + Sync {
    fn emit_record(&self, record: &MerkleAuditRecord) -> Result<(), String>;
    fn verify_integrity(&self) -> (bool, usize);
}

pub struct MerkleAuditStore {
    log_path: Option<String>,
    state: RwLock<AuditState>,
}

impl MerkleAuditStore {
    pub fn new(log_path: Option<String>) -> Self {
        let memory_window = std::env::var("OMNI_AUDIT_MEMORY_WINDOW")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(2_000);
        let segment_max_records = std::env::var("OMNI_AUDIT_SEGMENT_MAX_RECORDS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(10_000);
        Self::with_limits(log_path, memory_window, segment_max_records)
    }

    pub fn with_limits(
        log_path: Option<String>,
        memory_window: usize,
        segment_max_records: usize,
    ) -> Self {
        let memory_window = memory_window.max(1);
        let segment_max_records = segment_max_records.max(1);
        let store = Self {
            log_path: log_path.clone(),
            state: RwLock::new(AuditState {
                records: VecDeque::new(),
                total_count: 0,
                last_hash: GENESIS_HASH.to_string(),
                window_base_prev_hash: GENESIS_HASH.to_string(),
                segment_index: 0,
                records_in_segment: 0,
                memory_window,
                segment_max_records,
            }),
        };

        if let Some(path) = &log_path {
            let mut guard = store.state.write().unwrap();
            // Load checkpoint anchor if present
            let ckpt_path = format!("{}.ckpt", path);
            if let Ok(raw_ckpt) = fs::read_to_string(&ckpt_path) {
                if let Ok(val) = JsonValue::parse(&raw_ckpt) {
                    if let Some(seg) = val.get("segment_index").and_then(|x| x.as_u64()) {
                        guard.segment_index = seg;
                    }
                    if let Some(cnt) = val.get("total_count").and_then(|x| x.as_u64()) {
                        guard.total_count = cnt;
                    }
                    if let Some(h) = val.get("last_hash").and_then(|x| x.as_str()) {
                        guard.last_hash = h.to_string();
                        guard.window_base_prev_hash = h.to_string();
                    }
                }
            }

            if let Ok(file) = File::open(path) {
                let reader = BufReader::new(file);
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(json_val) = JsonValue::parse(trimmed) {
                        if let Some(record) = MerkleAuditRecord::from_json(&json_val) {
                            guard.total_count = (record.index + 1).max(guard.total_count);
                            guard.last_hash = record.current_hash.clone();
                            guard.records_in_segment += 1;
                            if guard.records.len() >= guard.memory_window {
                                if let Some(evicted) = guard.records.pop_front() {
                                    guard.window_base_prev_hash = evicted.current_hash;
                                }
                            }
                            guard.records.push_back(record);
                        }
                    }
                }
            }
        }

        store
    }

    fn write_signed_checkpoint(path: &str, state: &AuditState) {
        let ckpt_path = format!("{}.ckpt", path);
        let sig_payload = format!(
            "{}:{}:{}",
            state.segment_index, state.total_count, state.last_hash
        );
        let sig = hmac_sha256_hex(
            crate::models::continuation_key().as_bytes(),
            sig_payload.as_bytes(),
        );
        let ckpt_json = json!({
            "segment_index": state.segment_index as i64,
            "total_count": state.total_count as i64,
            "last_hash": state.last_hash.clone(),
            "hmac_sha256": sig,
        });
        if let Ok(mut f) = File::create(&ckpt_path) {
            let _ = f.write_all(ckpt_json.to_string_compact().as_bytes());
            let _ = f.flush();
            let _ = f.sync_all();
        }
    }

    pub fn record_invocation(
        &self,
        actor: &str,
        server_name: &str,
        tool_name: &str,
        arguments_json: &JsonValue,
        status: &str,
        execution_us: u64,
    ) -> MerkleAuditRecord {
        let mut guard = self.state.write().unwrap();
        let index = guard.total_count;
        let timestamp = now_epoch_secs();
        let prev_hash = guard.last_hash.clone();

        let args_compact = arguments_json.to_string_compact();
        let args_hash = sha256_hex(args_compact.as_bytes());

        let current_hash = MerkleAuditRecord::compute_hash(
            index,
            timestamp,
            &prev_hash,
            actor,
            server_name,
            tool_name,
            &args_hash,
            status,
        );

        let record = MerkleAuditRecord {
            index,
            timestamp,
            prev_hash,
            current_hash: current_hash.clone(),
            actor: actor.to_string(),
            server_name: server_name.to_string(),
            tool_name: tool_name.to_string(),
            args_hash,
            status: status.to_string(),
            execution_us,
        };

        if let Some(path) = &self.log_path {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let line = record.to_json().to_string_compact();
                let _ = writeln!(f, "{}", line);
                let _ = f.flush();
                let _ = f.sync_all();
            }
        }

        guard.total_count += 1;
        guard.last_hash = current_hash;
        guard.records_in_segment += 1;
        if guard.records.len() >= guard.memory_window {
            if let Some(evicted) = guard.records.pop_front() {
                guard.window_base_prev_hash = evicted.current_hash;
            }
        }
        guard.records.push_back(record.clone());

        if guard.records_in_segment >= guard.segment_max_records {
            if let Some(path) = &self.log_path {
                guard.segment_index += 1;
                let seg_path = format!("{}.seg{:06}.jsonl", path, guard.segment_index);
                if fs::rename(path, &seg_path).is_ok() {
                    Self::write_signed_checkpoint(path, &guard);
                }
            }
            guard.records_in_segment = 0;
        }

        record
    }

    pub fn verify_chain(&self) -> (bool, usize) {
        let guard = self.state.read().unwrap();
        if let Some(path) = &self.log_path {
            let ckpt_path = format!("{}.ckpt", path);
            let ckpt_exists = std::path::Path::new(&ckpt_path).exists();
            let segments_exist_on_disk =
                std::path::Path::new(&format!("{}.seg000001.jsonl", path)).exists();
            if (guard.segment_index > 0 || segments_exist_on_disk) && !ckpt_exists {
                return (false, 0);
            }
            if ckpt_exists {
                let raw_ckpt = match fs::read_to_string(&ckpt_path) {
                    Ok(s) => s,
                    Err(_) => return (false, 0),
                };
                let val = match JsonValue::parse(&raw_ckpt) {
                    Ok(v) => v,
                    Err(_) => return (false, 0),
                };
                let seg = match val.get("segment_index").and_then(|x| x.as_u64()) {
                    Some(v) => v,
                    None => return (false, 0),
                };
                let cnt = match val.get("total_count").and_then(|x| x.as_u64()) {
                    Some(v) => v,
                    None => return (false, 0),
                };
                let lh = match val.get("last_hash").and_then(|x| x.as_str()) {
                    Some(v) if !v.is_empty() => v,
                    _ => return (false, 0),
                };
                let sig = match val.get("hmac_sha256").and_then(|x| x.as_str()) {
                    Some(v) if !v.is_empty() => v,
                    _ => return (false, 0),
                };
                let sig_payload = format!("{}:{}:{}", seg, cnt, lh);
                let expected_sig = hmac_sha256_hex(
                    crate::models::continuation_key().as_bytes(),
                    sig_payload.as_bytes(),
                );
                if !crate::models::constant_time_eq(sig.as_bytes(), expected_sig.as_bytes()) {
                    return (false, 0);
                }
                if seg != guard.segment_index || cnt > guard.total_count {
                    return (false, 0);
                }
                if guard.records.is_empty() && (cnt != guard.total_count || lh != guard.last_hash) {
                    return (false, 0);
                }
                if (guard.total_count - cnt) as usize == guard.records.len()
                    && lh != guard.window_base_prev_hash
                {
                    return (false, 0);
                }
            }
        }

        let mut expected_prev = guard.window_base_prev_hash.clone();

        for (idx, record) in guard.records.iter().enumerate() {
            if record.prev_hash != expected_prev {
                return (false, idx);
            }
            let recomputed = MerkleAuditRecord::compute_hash(
                record.index,
                record.timestamp,
                &record.prev_hash,
                &record.actor,
                &record.server_name,
                &record.tool_name,
                &record.args_hash,
                &record.status,
            );
            if recomputed != record.current_hash {
                return (false, idx);
            }
            expected_prev = record.current_hash.clone();
        }

        (true, guard.total_count as usize)
    }

    pub fn list_records(&self, limit: Option<usize>) -> Vec<MerkleAuditRecord> {
        let guard = self.state.read().unwrap();
        let limit = limit.unwrap_or(50);
        guard.records.iter().rev().take(limit).cloned().collect()
    }

    pub fn get_head_hash(&self) -> String {
        let guard = self.state.read().unwrap();
        guard.last_hash.clone()
    }
}

impl AuditSink for MerkleAuditStore {
    fn emit_record(&self, record: &MerkleAuditRecord) -> Result<(), String> {
        if let Some(path) = &self.log_path {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("Failed to open audit log '{}': {}", path, e))?;
            let line = format!("{}\n", record.to_json().to_string_compact());
            file.write_all(line.as_bytes())
                .and_then(|_| file.flush())
                .map_err(|e| format!("Failed to write audit record: {}", e))?;
        }
        Ok(())
    }

    fn verify_integrity(&self) -> (bool, usize) {
        self.verify_chain()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmac_sha256_rfc4231_test_case_2() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let hmac = hmac_sha256_hex(key, data);
        assert_eq!(
            hmac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn test_segmented_audit_log_rotation_and_signed_checkpoint() {
        let dir = std::env::temp_dir().join(format!(
            "omni_audit_seg_{}",
            crate::logging::now_epoch_millis()
        ));
        let _ = fs::create_dir_all(&dir);
        let log_path = dir.join("audit.jsonl");
        let log_path_str = log_path.to_string_lossy().to_string();

        let logger = MerkleAuditStore::new(Some(log_path_str.clone()));
        {
            let mut guard = logger.state.write().unwrap();
            guard.memory_window = 3;
            guard.segment_max_records = 4;
        }

        for i in 0..6 {
            logger.record_invocation(
                "alice",
                "srv",
                &format!("tool_{}", i),
                &JsonValue::object(),
                "ok",
                10,
            );
        }

        let seg1 = format!("{}.seg000001.jsonl", log_path_str);
        let ckpt = format!("{}.ckpt", log_path_str);
        assert!(std::path::Path::new(&seg1).exists());
        assert!(std::path::Path::new(&ckpt).exists());
        assert_eq!(logger.list_records(Some(10)).len(), 3);
        let (ok, total) = logger.verify_chain();
        assert!(ok);
        assert_eq!(total, 6);

        // Tamper with checkpoint signature -> verify_chain must fail
        fs::write(
            &ckpt,
            r#"{"segment_index":1,"total_count":6,"last_hash":"bad","hmac_sha256":"0000"}"#,
        )
        .unwrap();
        let (ok_tampered, _) = logger.verify_chain();
        assert!(!ok_tampered);

        // Corrupt checkpoint JSON -> verify_chain must fail
        fs::write(&ckpt, "not-valid-json").unwrap();
        assert!(!logger.verify_chain().0);

        // Missing checkpoint when segment_index > 0 -> verify_chain must fail
        fs::remove_file(&ckpt).unwrap();
        assert!(!logger.verify_chain().0);

        let _ = fs::remove_dir_all(&dir);
    }
}
