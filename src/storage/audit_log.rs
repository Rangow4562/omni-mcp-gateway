//! Merkle Cryptographic Audit Log - Zero-dependency SHA-256 chained audit trail
//! for enterprise compliance, tamper-evident agent execution records, and ABAC verification.

use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::sync::RwLock;

/// Pure standard library SHA-256 implementation (FIPS 180-4 compliant).
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

pub struct MerkleAuditStore {
    log_path: Option<String>,
    records: RwLock<Vec<MerkleAuditRecord>>,
}

impl MerkleAuditStore {
    pub fn new(log_path: Option<String>) -> Self {
        let store = Self {
            log_path: log_path.clone(),
            records: RwLock::new(Vec::new()),
        };

        if let Some(path) = &log_path {
            if let Ok(file) = File::open(path) {
                let reader = BufReader::new(file);
                let mut records_guard = store.records.write().unwrap();
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(json_val) = JsonValue::parse(trimmed) {
                        if let Some(record) = MerkleAuditRecord::from_json(&json_val) {
                            records_guard.push(record);
                        }
                    }
                }
            }
        }

        store
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
        let mut guard = self.records.write().unwrap();
        let index = guard.len() as u64;
        let timestamp = now_epoch_secs();
        let prev_hash = if let Some(last) = guard.last() {
            last.current_hash.clone()
        } else {
            GENESIS_HASH.to_string()
        };

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
            }
        }

        guard.push(record.clone());
        record
    }

    pub fn verify_chain(&self) -> (bool, usize) {
        let guard = self.records.read().unwrap();
        let mut expected_prev = GENESIS_HASH.to_string();

        for (idx, record) in guard.iter().enumerate() {
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

        (true, guard.len())
    }

    pub fn list_records(&self, limit: Option<usize>) -> Vec<MerkleAuditRecord> {
        let guard = self.records.read().unwrap();
        let limit = limit.unwrap_or(50);
        guard.iter().rev().take(limit).cloned().collect()
    }

    pub fn get_head_hash(&self) -> String {
        let guard = self.records.read().unwrap();
        guard
            .last()
            .map(|r| r.current_hash.clone())
            .unwrap_or_else(|| GENESIS_HASH.to_string())
    }
}
